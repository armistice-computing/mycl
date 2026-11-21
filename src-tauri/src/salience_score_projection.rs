use crate::app_runtime::AppHandle;
use crate::{
    clock::epoch_millis,
    document_projection_service::document_blocks_for_read,
    document_service::{read_workspace_record, BlockSnapshot, DocumentRecord},
    json_utils::json_string,
    paths::existing_graph_dir,
    salience_score_values::local_unvaluated_wired_block_json,
    salience_value_store::{
        block_value_key, normalize_value_config, read_value_store_for, record_has_value_score,
    },
    salience_wire_context::{
        document_created_age_days, newer_value_timestamp, value_timestamp_age_days,
        value_wire_timestamp,
    },
    wire_state::wire_counts_toward_salience,
    workspace_entity_projection::workspace_wires,
};
use std::collections::{BTreeMap, BTreeSet};

#[derive(serde::Serialize)]
#[serde(untagged)]
enum ScoredRow {
    Valued(crate::salience_score_values::LocalBlockValueScores),
    Baseline(crate::salience_score_values::UnvaluatedWiredBlock),
}

struct RankKey {
    score: f64,
    document: String,
    block: String,
}
impl PartialEq for RankKey {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}
impl Eq for RankKey {}
impl PartialOrd for RankKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for RankKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other
            .score
            .total_cmp(&self.score)
            .then_with(|| self.document.cmp(&other.document))
            .then_with(|| self.block.cmp(&other.block))
    }
}

fn retain_score(
    rows: &mut BTreeMap<RankKey, ScoredRow>,
    limit: usize,
    score: f64,
    document: &str,
    block: &str,
    row: ScoredRow,
) {
    if limit == 0 {
        return;
    }
    let key = RankKey {
        score,
        document: document.into(),
        block: block.into(),
    };
    if rows.len() == limit && rows.last_key_value().is_some_and(|(last, _)| key >= *last) {
        return;
    }
    rows.insert(key, row);
    if rows.len() > limit {
        rows.pop_last();
    }
}

pub(super) use crate::salience_score_values::local_block_value_json_with_context;

pub(super) fn find_projected_block(
    document: &DocumentRecord,
    block_id: &str,
) -> Option<BlockSnapshot> {
    document_blocks_for_read(document)
        .into_iter()
        .find(|block| block.id == block_id)
}

/// Commons read of block value scores (empty observer) — preserved for call sites
/// that have no witness. Thin wrapper over [`local_block_value_scores_for`].
pub(super) fn local_block_value_scores(
    app: &AppHandle,
    graph_id: &str,
    document_id: Option<&str>,
    block_id: Option<&str>,
    block_id_list: Option<&BTreeSet<String>>,
    limit: usize,
    min_score: Option<f64>,
    valence_filter: Option<&str>,
) -> Result<Vec<serde_json::Value>, String> {
    local_block_value_scores_for(
        app,
        graph_id,
        "",
        document_id,
        block_id,
        block_id_list,
        limit,
        min_score,
        valence_filter,
    )
}

/// Per-OBSERVER read of block value scores — reads THIS witness's value store
/// (`values/{observer}/…`) so a query returns only that witness's valuations, never
/// the global sum. Empty observer ⇒ the commons (byte-identical to today).
#[allow(clippy::too_many_arguments)]
pub(super) fn local_block_value_scores_for(
    app: &AppHandle,
    graph_id: &str,
    observer: &str,
    document_id: Option<&str>,
    block_id: Option<&str>,
    block_id_list: Option<&BTreeSet<String>>,
    limit: usize,
    min_score: Option<f64>,
    valence_filter: Option<&str>,
) -> Result<Vec<serde_json::Value>, String> {
    local_block_value_scores_with_documents(
        app,
        graph_id,
        observer,
        document_id,
        block_id,
        block_id_list,
        limit,
        min_score,
        valence_filter,
        None,
    )
}

/// Reuse a caller's cold projection snapshot; values, wires and time stay live.
#[allow(clippy::too_many_arguments)]
pub(super) fn local_block_value_scores_with_documents(
    app: &AppHandle,
    graph_id: &str,
    observer: &str,
    document_id: Option<&str>,
    block_id: Option<&str>,
    block_id_list: Option<&BTreeSet<String>>,
    limit: usize,
    min_score: Option<f64>,
    valence_filter: Option<&str>,
    documents: Option<Vec<DocumentRecord>>,
) -> Result<Vec<serde_json::Value>, String> {
    let Some(documents) = documents else {
        return local_block_value_scores_with_created_at(
            app,
            graph_id,
            observer,
            document_id,
            block_id,
            block_id_list,
            limit,
            min_score,
            valence_filter,
            &Default::default(),
        );
    };
    let created_at = crate::salience_wire_context::document_created_at_map(&documents);
    // Only identities are needed here; don't clone every block's text and marks.
    let mut inventory = BTreeSet::new();
    for doc in &documents {
        if doc.blocks.is_empty() {
            inventory.insert((doc.document_id.clone(), "body".to_string()));
        } else {
            inventory.extend(
                doc.blocks
                    .iter()
                    .map(|block| (doc.document_id.clone(), block.id.clone())),
            );
        }
    }
    score_existing_blocks(
        app,
        graph_id,
        observer,
        document_id,
        block_id,
        block_id_list,
        limit,
        min_score,
        valence_filter,
        &created_at,
        &inventory,
    )
}

/// Score using a thin authoritative manifest inventory, refreshing any supplied
/// creation dates. Does not hydrate document content or CRDT state.
#[allow(clippy::too_many_arguments)]
pub(super) fn local_block_value_scores_with_created_at(
    app: &AppHandle,
    graph_id: &str,
    observer: &str,
    document_id: Option<&str>,
    block_id: Option<&str>,
    block_id_list: Option<&BTreeSet<String>>,
    limit: usize,
    min_score: Option<f64>,
    valence_filter: Option<&str>,
    created_at: &crate::salience_wire_context::DocumentCreatedAt,
) -> Result<Vec<serde_json::Value>, String> {
    // Deserialize only scoring fields; never hydrate bodies, trees or Y.Doc state.
    // Manifests, not a possibly lagging RDF projection, own block existence.
    #[derive(serde::Deserialize)]
    struct BlockIdentity {
        id: String,
    }
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct ScoringDocument {
        document_id: String,
        graph_id: String,
        #[serde(default)]
        created_at: String,
        #[serde(default)]
        blocks: Vec<BlockIdentity>,
    }
    let graph_dir = existing_graph_dir(app, graph_id)?;
    let mut dates = created_at.clone();
    let mut inventory = BTreeSet::new();
    let scope = if let Some(id) = document_id {
        let workspace = read_workspace_record(&graph_dir, graph_id)?;
        Some(
            crate::workspace_projection_service::folder_document_ids(
                workspace.snapshot.as_ref(),
                id,
            )
            .unwrap_or_else(|| BTreeSet::from([id.to_string()])),
        )
    } else {
        None
    };
    let directory = graph_dir.join("documents");
    if directory.is_dir() {
        for entry in std::fs::read_dir(&directory).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let id = entry.file_name().to_string_lossy().to_string();
            if scope.as_ref().is_some_and(|ids| !ids.contains(&id)) {
                continue;
            }
            let manifest = entry.path().join("document.json");
            if !manifest.is_file()
                || crate::document_tombstone_store::document_is_tombstoned(&graph_dir, &id)?
            {
                continue;
            }
            let bytes = std::fs::read(manifest).map_err(|e| e.to_string())?;
            let doc: ScoringDocument = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
            if doc.document_id != id || doc.graph_id != graph_id {
                return Err("Scoring manifest identity mismatch".into());
            }
            dates.insert(id.clone(), doc.created_at);
            if doc.blocks.is_empty() {
                inventory.insert((id, "body".into()));
            } else {
                inventory.extend(doc.blocks.into_iter().map(|block| (id.clone(), block.id)));
            }
        }
    }
    score_existing_blocks(
        app,
        graph_id,
        observer,
        document_id,
        block_id,
        block_id_list,
        limit,
        min_score,
        valence_filter,
        &dates,
        &inventory,
    )
}

#[allow(clippy::too_many_arguments)]
fn score_existing_blocks(
    app: &AppHandle,
    graph_id: &str,
    observer: &str,
    document_id: Option<&str>,
    block_id: Option<&str>,
    block_id_list: Option<&BTreeSet<String>>,
    limit: usize,
    min_score: Option<f64>,
    valence_filter: Option<&str>,
    created_at: &crate::salience_wire_context::DocumentCreatedAt,
    inventory: &BTreeSet<(String, String)>,
) -> Result<Vec<serde_json::Value>, String> {
    let graph_dir = existing_graph_dir(app, graph_id)?;
    let (_, graph) = crate::graph_record_store::read_graph_record_no_heal(app, graph_id)
        .map_err(|e| e.to_string())?;
    let mut dates = crate::salience_wire_context::with_graph_creation_fallback(
        created_at.clone(),
        &graph.created_at,
    );
    for (doc, _) in inventory {
        dates
            .entry(doc.clone())
            .or_insert_with(|| graph.created_at.clone());
    }
    let created_at = &dates;
    let mut store = read_value_store_for(&graph_dir, graph_id, observer)?;
    normalize_value_config(&mut store.config);
    let workspace = read_workspace_record(&graph_dir, graph_id)?;
    let folder_documents = document_id.and_then(|id| {
        crate::workspace_projection_service::folder_document_ids(workspace.snapshot.as_ref(), id)
    });
    let wires = workspace_wires(workspace.snapshot.as_ref());
    let now_ms = epoch_millis();
    let mut valued_keys = BTreeSet::new();
    let mut blocks = BTreeMap::new();

    for record in store.blocks.values() {
        if !inventory.contains(&(record.document_id.clone(), record.block_id.clone())) {
            continue;
        }
        let key = block_value_key(&record.document_id, &record.block_id);
        if !record_has_value_score(record) {
            continue;
        }
        valued_keys.insert(key);
        if document_id
            .map(|id| !document_in_scope(&record.document_id, id, folder_documents.as_ref()))
            .unwrap_or(false)
        {
            continue;
        }
        if block_id
            .map(|block_id| record.block_id != block_id)
            .unwrap_or(false)
        {
            continue;
        }
        if block_id_list
            .map(|block_ids| !block_ids.contains(&record.block_id))
            .unwrap_or(false)
        {
            continue;
        }
        // The filter reads the charge the person sees: their own rating
        // together with the agents', as the combined valence is computed.
        let combined_raw_valence = record.raw_valence_sum + record.user_valence.unwrap_or(0.0);
        let charge = if combined_raw_valence != 0.0 {
            combined_raw_valence
        } else {
            record.cumulative_valence
        };
        match valence_filter {
            Some("positive") if charge <= 0.0 => continue,
            Some("negative") if charge >= 0.0 => continue,
            Some("positive") | Some("negative") => {}
            Some(_) => continue,
            None => {}
        }
        let scores =
            local_block_value_json_with_context(record, &store.config, &wires, created_at, now_ms);
        if min_score
            .map(|min_score| scores.composite_score < min_score)
            .unwrap_or(false)
        {
            continue;
        }
        retain_score(
            &mut blocks,
            limit,
            scores.composite_score,
            &record.document_id,
            &record.block_id,
            ScoredRow::Valued(scores),
        );
    }

    {
        let mut block_wire_counts: BTreeMap<String, usize> = BTreeMap::new();
        let mut doc_wire_counts: BTreeMap<String, usize> = BTreeMap::new();
        let mut doc_newest_wire: BTreeMap<String, String> = BTreeMap::new();
        for wire in &wires {
            if !wire_counts_toward_salience(wire) {
                continue;
            }
            if json_string(wire.get("id"))
                .map(|id| id.ends_with("-inv"))
                .unwrap_or(false)
            {
                continue;
            }
            let created = value_wire_timestamp(wire);
            for doc_key in ["sourceDocumentId", "targetDocumentId"] {
                if let Some(doc_id) = json_string(wire.get(doc_key)) {
                    *doc_wire_counts.entry(doc_id.clone()).or_insert(0) += 1;
                    let newest = doc_newest_wire.remove(&doc_id);
                    if let Some(value) = newer_value_timestamp(newest, created.clone()) {
                        doc_newest_wire.insert(doc_id, value);
                    }
                }
            }
            for (block_key, doc_key) in [
                ("sourceBlockId", "sourceDocumentId"),
                ("targetBlockId", "targetDocumentId"),
            ] {
                let Some(wire_block_id) = json_string(wire.get(block_key)) else {
                    continue;
                };
                let Some(wire_document_id) = json_string(wire.get(doc_key)) else {
                    continue;
                };
                let key = block_value_key(&wire_document_id, &wire_block_id);
                *block_wire_counts.entry(key.clone()).or_insert(0) += 1;
            }
        }

        for (wire_document_id, wire_block_id) in inventory {
            let key = block_value_key(wire_document_id, wire_block_id);
            let block_wire_count = block_wire_counts.get(&key).copied().unwrap_or(0);
            if valued_keys.contains(&key) {
                continue;
            }
            if document_id
                .map(|id| !document_in_scope(wire_document_id, id, folder_documents.as_ref()))
                .unwrap_or(false)
            {
                continue;
            }
            if block_id
                .map(|block_id| wire_block_id != block_id)
                .unwrap_or(false)
            {
                continue;
            }
            if block_id_list
                .map(|block_ids| !block_ids.contains(wire_block_id))
                .unwrap_or(false)
            {
                continue;
            }
            if valence_filter.is_some() {
                continue;
            }
            let doc_wire_count = doc_wire_counts.get(wire_document_id).copied().unwrap_or(0);
            let doc_age_days =
                document_created_age_days(created_at, wire_document_id, None, now_ms);
            let wire_age_days = value_timestamp_age_days(
                doc_newest_wire.get(wire_document_id).map(String::as_str),
                now_ms,
            );
            let scores = local_unvaluated_wired_block_json(
                wire_document_id,
                wire_block_id,
                &store.config,
                block_wire_count,
                doc_wire_count,
                doc_age_days,
                wire_age_days,
            );
            if min_score
                .map(|min_score| scores.composite_score < min_score)
                .unwrap_or(false)
            {
                continue;
            }
            retain_score(
                &mut blocks,
                limit,
                scores.composite_score,
                wire_document_id,
                wire_block_id,
                ScoredRow::Baseline(scores),
            );
        }
    }

    Ok(blocks
        .into_values()
        .map(|row| serde_json::to_value(row).expect("ScoredRow always serializes"))
        .collect())
}

fn document_in_scope(
    document: &str,
    scope: &str,
    folder_documents: Option<&BTreeSet<String>>,
) -> bool {
    folder_documents.map_or(document == scope, |ids| ids.contains(document))
}

#[cfg(test)]
mod scope_tests {
    use super::*;

    #[test]
    fn bounded_selection_matches_full_sort_with_ties() {
        let config = crate::salience_value_config::default_value_config();
        let mut bounded = BTreeMap::new();
        let mut all = Vec::new();
        for i in (0..100).rev() {
            let id = format!("b-{i:03}");
            let row = local_unvaluated_wired_block_json("d", &id, &config, i % 4, 0, 10.0, 9999.0);
            all.push(RankKey { score: row.composite_score, document: "d".into(), block: id.clone() });
            retain_score(&mut bounded, 7, row.composite_score, "d", &id, ScoredRow::Baseline(row));
            assert!(bounded.len() <= 7);
        }
        all.sort();
        assert!(bounded.keys().eq(all.iter().take(7)));
    }

    #[test]
    fn document_and_empty_folder_scopes_never_become_graph_queries() {
        assert!(document_in_scope("doc", "doc", None));
        assert!(!document_in_scope("other", "doc", None));
        assert!(!document_in_scope("doc", "empty", Some(&BTreeSet::new())));
        let ids = ["doc".to_string()].into_iter().collect();
        assert!(document_in_scope("doc", "folder", Some(&ids)));
        assert!(!document_in_scope("outside", "folder", Some(&ids)));
    }
}
