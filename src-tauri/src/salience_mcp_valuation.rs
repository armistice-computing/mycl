use crate::app_runtime::AppHandle;
use crate::{
    clock::{epoch_millis, parse_timestamp, timestamp},
    document_service::read_workspace_record,
    graph_service::touch_graph_updated_at,
    mcp_utils::{mcp_arg_string, mcp_graph_id_or_default},
    paths::existing_graph_dir,
    profile_service::touch_profile_updated_at,
    salience_mcp_inputs::mcp_value_inputs,
    salience_rdf_materializer::reconcile_value_store,
    salience_score_projection::local_block_value_json_with_context,
    salience_value_store::{
        block_value_key, cumulative_importance, cumulative_valence, normalize_value_tags,
        read_value_store_for, write_value_store, LocalBlockValueRecord,
    },
    workspace_entity_projection::workspace_wires,
};
use std::collections::BTreeMap;

/// One stable valuation event from the source-sync ledger.  The public MCP
/// `value` face is an incremental convenience; this event form is the
/// replayable authority used by offline clients and projection rebuilds.
#[derive(Debug, Clone)]
pub(crate) struct SourceValuationEvent {
    pub(crate) event_id: String,
    pub(crate) observer: String,
    pub(crate) document_id: String,
    pub(crate) block_id: String,
    pub(crate) importance: Option<i64>,
    pub(crate) valence: Option<i64>,
    pub(crate) tags: Vec<String>,
    pub(crate) at_ms: i64,
}

/// Deterministically fold a set of stable valuation events and replace the
/// disposable per-observer value stores/projections. Sorting by logical time
/// and event identity makes replay independent of network delivery order; a
/// duplicate event is removed by the source ledger before this function.
pub(crate) fn rebuild_value_store_from_source_events(
    app: &AppHandle,
    graph_id: &str,
    events: &[SourceValuationEvent],
    clear_existing: bool,
) -> Result<serde_json::Value, String> {
    let graph_dir = existing_graph_dir(app, graph_id)?;
    let _value_guard = crate::salience_value_store::value_mutation_guard()?;
    let retired = crate::salience_value_store::retired_value_cutoffs(&graph_dir)?;
    let mut by_observer: BTreeMap<String, Vec<&SourceValuationEvent>> = BTreeMap::new();
    for event in events {
        if retired
            .get(&event.document_id)
            .is_some_and(|cutoff| event.at_ms <= *cutoff)
            || crate::document_tombstone_store::document_is_tombstoned(
                &graph_dir,
                &event.document_id,
            )?
        {
            continue;
        }
        by_observer
            .entry(event.observer.clone())
            .or_default()
            .push(event);
    }

    let mut observer_reports = Vec::new();
    for (observer, mut observer_events) in by_observer {
        observer_events.sort_by(|left, right| {
            (left.at_ms, left.event_id.as_str()).cmp(&(right.at_ms, right.event_id.as_str()))
        });
        let mut store = read_value_store_for(&graph_dir, graph_id, &observer)?;
        if clear_existing {
            store.blocks.clear();
        }
        for event in observer_events {
            let key = block_value_key(&event.document_id, &event.block_id);
            let record = store
                .blocks
                .entry(key)
                .or_insert_with(|| LocalBlockValueRecord {
                    document_id: event.document_id.clone(),
                    block_id: event.block_id.clone(),
                    ..LocalBlockValueRecord::default()
                });
            fold_valuation(
                record,
                event.importance,
                event.valence,
                &event.tags,
                &crate::emporium::terms::iso_from_ms(event.at_ms),
            );
        }
        write_value_store(&graph_dir, &store)?;
        let diff = reconcile_value_store(&graph_dir, &store)?;
        observer_reports.push(serde_json::json!({
            "observer": &observer,
            "events": events.iter().filter(|event| event.observer == observer).count(),
            "blocks": store.blocks.len(),
            "rdfOperations": diff.op_count(),
        }));
    }
    // This function is a source replay/rebuild, not a new authoring event.
    // Touching graph/profile clocks here made the projection depend on when a
    // repair happened and therefore prevented byte-set-identical replay.
    // Incremental public `value` writes retain their normal clock touches.
    Ok(serde_json::json!({
        "events": events.len(),
        "observers": observer_reports,
    }))
}

fn local_valuation_event_id(
    arguments: &serde_json::Value,
    single: bool,
    index: usize,
) -> Option<String> {
    let keys = &["valuationEventId", "valuation_event_id"];
    if !single {
        if let Some(id) = arguments
            .get("valuations")
            .and_then(|entries| entries.get(index))
            .and_then(|entry| mcp_arg_string(entry, keys))
        {
            return Some(id);
        }
    }
    mcp_arg_string(arguments, keys).map(|base| {
        if single {
            base
        } else {
            format!("{base}/{index}")
        }
    })
}

/// A false result is a replay: no counters, decay, tags, or activity are changed.
fn apply_local_valuation(
    record: &mut LocalBlockValueRecord,
    entry: &crate::salience_mcp_inputs::LocalValueInputEntry,
    event_id: Option<&str>,
    now: &str,
) -> Result<bool, String> {
    // Canonical JSON is a collision-free payload fingerprint, including identity.
    let fingerprint = serde_json::to_string(&(
        &entry.document_id,
        &entry.block_id,
        entry.importance,
        entry.valence,
        &entry.tags,
    ))
    .map_err(|error| error.to_string())?;
    if let Some(id) = event_id {
        if let Some(previous) = record.valuation_receipts.get(id) {
            return if previous == &fingerprint {
                Ok(false)
            } else {
                Err(format!(
                    "valuationEventId '{id}' was already accepted with different content"
                ))
            };
        }
    }
    record.document_id = entry.document_id.clone();
    record.block_id = entry.block_id.clone();
    fold_valuation(record, entry.importance, entry.valence, &entry.tags, now);
    if let Some(id) = event_id {
        record
            .valuation_receipts
            .insert(id.to_string(), fingerprint);
    }
    Ok(true)
}

/// Live writes and source replay must agree on forgetting and activity clocks.
fn fold_valuation(
    record: &mut LocalBlockValueRecord,
    importance: Option<i64>,
    valence: Option<i64>,
    tags: &[String],
    now: &str,
) {
    if let Some(importance) = importance {
        if importance == 0 {
            // Only an unrated record has an implicit baseline. An explicitly
            // forgotten legacy record (count > 0, raw == 0) must stay forgotten.
            if record.raw_importance_sum == 0.0
                && record.importance_count == 0
                && record.user_importance.is_none()
            {
                record.raw_importance_sum = 2.0;
            }
            record.raw_importance_sum *= 0.2;
        } else {
            record.raw_importance_sum += importance as f64;
        }
        record.importance_count += 1;
    }
    if let Some(valence) = valence {
        record.raw_valence_sum += valence as f64;
        record.valence_count += 1;
    }
    record.cumulative_importance = cumulative_importance(record.raw_importance_sum);
    record.cumulative_valence = cumulative_valence(record.raw_valence_sum);
    record.valuation_count = record.importance_count + record.valence_count;
    if !tags.is_empty() {
        let mut merged_tags = record.tags.clone();
        merged_tags.extend_from_slice(tags);
        record.tags = normalize_value_tags(merged_tags);
    }
    // Forgetting never refreshes attention, even when paired with valence.
    if importance != Some(0) && (importance.is_some() || valence.is_some()) {
        record.last_valuated_at = now.to_string();
    }
}

pub(super) fn mcp_local_value(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_graph_id_or_default(&app, arguments)?;
    let graph_dir = existing_graph_dir(&app, &graph_id)?;
    // Per-observer salience: each witness values into its OWN store + named graph, so
    // two witnesses' valuations of the same block never sum. Empty/absent ⇒ the shared
    // commons (today's behavior, byte-identical).
    let observer =
        mcp_arg_string(arguments, &["observer_agent_id", "observerAgentId"]).unwrap_or_default();
    let (single, entries) = mcp_value_inputs(arguments)?;
    let workspace = read_workspace_record(&graph_dir, &graph_id)?;
    let wires = workspace_wires(workspace.snapshot.as_ref());
    let mut documents = BTreeMap::new();
    for entry in &entries {
        if !entry.document_id.is_empty() {
            crate::document_tombstone_store::require_document_not_tombstoned(
                &graph_dir,
                &entry.document_id,
            )?;
        }
        if documents.contains_key(&entry.document_id) || entry.document_id.is_empty() {
            continue;
        }
        let dir = crate::paths::document_dir(&graph_dir, &entry.document_id)?;
        if dir.join("document.json").is_file() {
            let doc = crate::document_record_store::read_document_record_cold(
                &graph_dir,
                &dir.join("document.json"),
            )?;
            if doc.document_id != entry.document_id {
                return Err("Document identity does not match valuation target".into());
            }
            documents.insert(entry.document_id.clone(), doc.created_at);
        }
    }
    let (_, graph) = crate::graph_record_store::read_graph_record_no_heal(&app, &graph_id)
        .map_err(|error| error.to_string())?;
    let mut documents =
        crate::salience_wire_context::with_graph_creation_fallback(documents, &graph.created_at);
    for entry in &entries {
        documents
            .entry(entry.document_id.clone())
            .or_insert_with(|| graph.created_at.clone());
    }
    let _value_guard = crate::salience_value_store::value_mutation_guard()?;
    for entry in &entries {
        if !entry.document_id.is_empty() { crate::document_tombstone_store::require_document_not_tombstoned(&graph_dir, &entry.document_id)?; }
    }
    let mut store = read_value_store_for(&graph_dir, &graph_id, &observer)?;
    let now = timestamp();
    let now_ms = parse_timestamp(&now).unwrap_or_else(epoch_millis);
    let mut results = Vec::new();
    let mut errors = Vec::new();
    let mut tags_applied = 0usize;
    let mut changed = false;

    for entry in entries {
        if entry.document_id.is_empty() || entry.block_id.is_empty() {
            errors.push(serde_json::json!({
                "index": entry.input_index,
                "error": "document_id and block_id are required",
            }));
            continue;
        }
        if entry.importance.is_none() && entry.valence.is_none() && entry.tags.is_empty() {
            errors.push(serde_json::json!({
                "index": entry.input_index,
                "error": "At least one of importance, valence, or tags required",
            }));
            continue;
        }
        if let Some(importance) = entry.importance {
            if !(0..=5).contains(&importance) {
                errors.push(serde_json::json!({
                    "index": entry.input_index,
                    "error": "importance must be between 0 and 5",
                }));
                continue;
            }
        }
        if let Some(valence) = entry.valence {
            if !(-5..=5).contains(&valence) {
                errors.push(serde_json::json!({
                    "index": entry.input_index,
                    "error": "valence must be between -5 and +5",
                }));
                continue;
            }
        }

        let key = block_value_key(&entry.document_id, &entry.block_id);
        let record = store
            .blocks
            .entry(key)
            .or_insert_with(|| LocalBlockValueRecord {
                document_id: entry.document_id.clone(),
                block_id: entry.block_id.clone(),
                ..LocalBlockValueRecord::default()
            });
        let event_id = local_valuation_event_id(arguments, single, entry.input_index);
        match apply_local_valuation(record, &entry, event_id.as_deref(), &now) {
            Ok(applied) => changed |= applied,
            Err(error) => {
                errors.push(serde_json::json!({ "index": entry.input_index, "error": error }));
                continue;
            }
        }
        if !entry.tags.is_empty() {
            tags_applied += 1;
        }
        if entry.importance.is_some() || entry.valence.is_some() {
            let scores = local_block_value_json_with_context(
                record,
                &store.config,
                &wires,
                &documents,
                now_ms,
            );
            results.push(
                serde_json::to_value(&scores).expect("LocalBlockValueScores always serializes"),
            );
        }
    }

    if changed {
        write_value_store(&graph_dir, &store)?;
    }
    // Reconcile even on replay: the previous commit may have outlived projection failure.
    // TODO(MO-delta-routing): route the structured TripleDiff instead of dropping it.
    let _diff = reconcile_value_store(&graph_dir, &store)?;
    if changed {
        touch_graph_updated_at(&graph_dir)?;
        touch_profile_updated_at(&app)?;
        if observer.is_empty() {
            crate::geist_important_blocks::schedule_important_blocks_rebuild(&app, &graph_id);
        }
    }

    if single {
        if let Some(error) = errors.first() {
            return Err(error
                .get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("valuation failed")
                .to_string());
        }
        if !results.is_empty() {
            return Ok(results.remove(0));
        }
        if tags_applied > 0 {
            return Ok(serde_json::json!({
                "tags_applied": tags_applied,
                "tagsApplied": tags_applied,
            }));
        }
        Ok(serde_json::json!({}))
    } else {
        let mut output = serde_json::json!({
            "results": results,
            "updated_count": results.len(),
            "updatedCount": results.len(),
        });
        if !errors.is_empty() {
            output["errors"] = serde_json::Value::Array(errors.clone());
            output["error_count"] = serde_json::json!(errors.len());
            output["errorCount"] = serde_json::json!(errors.len());
        }
        if tags_applied > 0 {
            output["tags_applied"] = serde_json::json!(tags_applied);
            output["tagsApplied"] = serde_json::json!(tags_applied);
        }
        Ok(output)
    }
}

#[cfg(test)]
mod local_event_tests {
    use super::*;
    #[test]
    fn first_agent_zero_does_not_add_importance_to_a_human_rated_block() {
        for human in [0.0, 3.0, 5.0] {
            let mut record = LocalBlockValueRecord {
                user_importance: Some(human),
                ..Default::default()
            };
            fold_valuation(&mut record, Some(0), None, &[], "later");
            assert_eq!(record.raw_importance_sum, 0.0);
            assert_eq!(record.user_importance, Some(human));
            assert!(record.last_valuated_at.is_empty());
        }
    }
    use crate::salience_mcp_inputs::LocalValueInputEntry;

    fn entry(importance: i64) -> LocalValueInputEntry {
        LocalValueInputEntry {
            input_index: 0,
            document_id: "doc".into(),
            block_id: "block".into(),
            importance: Some(importance),
            valence: Some(-2),
            tags: vec!["decision".into()],
        }
    }

    #[test]
    fn first_zero_seeds_baseline_and_repeated_zero_decays_without_refresh() {
        // Prior valence does not count as an importance rating.
        let mut record = LocalBlockValueRecord {
            raw_valence_sum: 2.0,
            valence_count: 1,
            last_valuated_at: "prior".into(),
            ..Default::default()
        };
        let mut zero = entry(0);
        zero.valence = None;
        apply_local_valuation(&mut record, &zero, Some("zero"), "first").unwrap();
        assert_eq!(record.raw_importance_sum, 0.4);
        assert_eq!(record.cumulative_importance, cumulative_importance(0.4));
        assert_eq!(record.importance_count, 1);
        assert_eq!(record.last_valuated_at, "prior");
        let before = serde_json::to_value(&record).unwrap();
        assert!(!apply_local_valuation(&mut record, &zero, Some("zero"), "retry").unwrap());
        assert_eq!(serde_json::to_value(&record).unwrap(), before);
        apply_local_valuation(&mut record, &zero, Some("next"), "second").unwrap();
        assert!((record.raw_importance_sum - 0.08).abs() < 1e-12);
        assert_eq!(record.importance_count, 2);
        assert_eq!(record.last_valuated_at, "prior");
    }

    #[test]
    fn zero_with_valence_never_refreshes_clock_or_reseeds_legacy_zero() {
        for count in [0, 1, 5] {
            let mut record = LocalBlockValueRecord {
                importance_count: count,
                last_valuated_at: "prior".into(),
                ..Default::default()
            };
            apply_local_valuation(&mut record, &entry(0), None, "later").unwrap();
            assert_eq!(
                record.raw_importance_sum,
                if count == 0 { 0.4 } else { 0.0 }
            );
            assert_eq!(record.importance_count, count + 1);
            assert_eq!(record.raw_valence_sum, -2.0);
            assert_eq!(record.valence_count, 1);
            assert_eq!(record.valuation_count, count + 2);
            assert_eq!(record.last_valuated_at, "prior");
        }
        let mut fresh = LocalBlockValueRecord::default();
        apply_local_valuation(&mut fresh, &entry(0), None, "later").unwrap();
        assert!(fresh.last_valuated_at.is_empty());
    }

    #[test]
    fn source_fold_matches_live_after_each_event() {
        let mut live = LocalBlockValueRecord::default();
        let mut replay = LocalBlockValueRecord {
            document_id: "doc".into(),
            block_id: "block".into(),
            ..Default::default()
        };
        let sequence = [
            (Some(0), None),
            (Some(0), Some(-2)),
            (Some(3), Some(1)),
            (Some(0), Some(5)),
            (None, Some(0)),
            (None, None),
            (Some(0), None),
        ];
        for (index, (importance, valence)) in sequence.into_iter().enumerate() {
            let event = SourceValuationEvent {
                event_id: index.to_string(),
                observer: String::new(),
                document_id: "doc".into(),
                block_id: "block".into(),
                importance,
                valence,
                tags: vec![format!("tag-{index}"), "shared".into()],
                at_ms: 1_700_000_000_000 + index as i64 * 1000,
            };
            let input = LocalValueInputEntry {
                input_index: index,
                document_id: event.document_id.clone(),
                block_id: event.block_id.clone(),
                importance,
                valence,
                tags: event.tags.clone(),
            };
            let now = crate::emporium::terms::iso_from_ms(event.at_ms);
            apply_local_valuation(&mut live, &input, None, &now).unwrap();
            fold_valuation(
                &mut replay,
                event.importance,
                event.valence,
                &event.tags,
                &now,
            );
            assert_eq!(
                serde_json::to_value(&live).unwrap(),
                serde_json::to_value(&replay).unwrap()
            );
        }
    }

    #[test]
    fn replay_fresh_conflict_and_legacy_additivity() {
        let mut record = LocalBlockValueRecord::default();
        assert!(apply_local_valuation(&mut record, &entry(3), Some("a"), "first").unwrap());
        let before = serde_json::to_value(&record).unwrap();
        assert!(!apply_local_valuation(&mut record, &entry(3), Some("a"), "later").unwrap());
        assert_eq!(serde_json::to_value(&record).unwrap(), before);
        assert!(
            apply_local_valuation(&mut record, &entry(4), Some("a"), "later")
                .unwrap_err()
                .contains("different content")
        );
        assert_eq!(serde_json::to_value(&record).unwrap(), before);
        assert!(apply_local_valuation(&mut record, &entry(3), Some("b"), "fresh").unwrap());
        assert_eq!(record.raw_importance_sum, 6.0);
        assert_eq!(record.raw_valence_sum, -4.0);
        assert_eq!(record.valuation_count, 4);
        for _ in 0..2 {
            assert!(apply_local_valuation(&mut record, &entry(3), None, "legacy").unwrap());
        }
        assert_eq!(record.raw_importance_sum, 12.0);
        assert_eq!(record.valuation_count, 8);
        assert_eq!(record.valuation_receipts.len(), 2);
    }

    #[test]
    fn persisted_receipt_prevents_repeat_decay_after_projection_failure_boundary() {
        let dir = std::env::temp_dir().join(format!("value-receipt-{}", uuid::Uuid::new_v4()));
        let mut store = read_value_store_for(&dir, "graph", "").unwrap();
        let key = block_value_key("doc", "block");
        let record = store.blocks.entry(key.clone()).or_default();
        apply_local_valuation(record, &entry(5), Some("seed"), "first").unwrap();
        let mut decay = entry(0);
        decay.valence = None;
        apply_local_valuation(record, &decay, Some("decay"), "second").unwrap();
        assert_eq!(record.raw_importance_sum, 1.0);
        let before = serde_json::to_value(&*record).unwrap();
        // Stop exactly at the durable commit, before the fallible RDF projection.
        write_value_store(&dir, &store).unwrap();
        let mut reloaded = read_value_store_for(&dir, "graph", "").unwrap();
        let record = reloaded.blocks.get_mut(&key).unwrap();
        assert!(!apply_local_valuation(record, &decay, Some("decay"), "retry").unwrap());
        assert_eq!(serde_json::to_value(&*record).unwrap(), before);
        assert_eq!(record.importance_count, 2);
        assert_eq!(record.last_valuated_at, "first");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn event_id_single_batch_and_entry_override() {
        let args = serde_json::json!({
            "valuationEventId": "base",
            "valuations": [{}, {"valuation_event_id": "explicit"}]
        });
        assert_eq!(
            local_valuation_event_id(&args, true, 0).as_deref(),
            Some("base")
        );
        assert_eq!(
            local_valuation_event_id(&args, false, 0).as_deref(),
            Some("base/0")
        );
        assert_eq!(
            local_valuation_event_id(&args, false, 1).as_deref(),
            Some("explicit")
        );
        assert_eq!(
            local_valuation_event_id(&serde_json::json!({}), true, 0),
            None
        );
        assert_eq!(
            local_valuation_event_id(
                &serde_json::json!({"valuations": [{"valuationEventId": "entry"}]}),
                false,
                0
            )
            .as_deref(),
            Some("entry")
        );
    }

    #[test]
    fn fingerprint_covers_identity_tags_and_missing_scores() {
        let mut record = LocalBlockValueRecord::default();
        apply_local_valuation(&mut record, &entry(3), Some("a"), "first").unwrap();
        for field in ["document", "block", "tags", "valence"] {
            let mut changed = entry(3);
            match field {
                "document" => changed.document_id = "other".into(),
                "block" => changed.block_id = "other".into(),
                "tags" => changed.tags.clear(),
                _ => changed.valence = None,
            }
            assert!(apply_local_valuation(&mut record, &changed, Some("a"), "later").is_err());
        }
    }
}
