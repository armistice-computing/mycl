//! dreamtOf wires: the journal's links to what each published dream drew
//! on (Eschaton, 2026-10-19). They let a reader walk from a dream to its
//! sources, and they count toward salience for one day, the dream's
//! residue in waking attention. Then they are deleted, never tombstoned:
//!
//! - every cycle first deletes all of them, before it reads the graph;
//! - the scheduler's sweep deletes them at their expiry, so a night that
//!   does not dream (asleep, nightly off, a failed dreamer) leaves nothing;
//! - until the sweep runs, an expired wire reads as gone everywhere
//!   (`wire_state::wire_is_active`).
//!
//! Dreaming never reads them back: its scanners see judgment wires only.

use super::journal::JournalEntry;
use crate::wire_predicates::{WireFamily, DREAMT_OF, DREAM_WIRE_LIFETIME_MS};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// When the current dreamtOf wires expire, so the sweep can tell from one
/// small file whether a graph has any to delete.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DreamWireMarker {
    pub(crate) expires_at_ms: i64,
    pub(crate) count: usize,
}

pub(crate) fn marker_path(graph_dir: &Path) -> PathBuf {
    graph_dir.join("dreaming").join("dream-wires.json")
}

fn read_marker(graph_dir: &Path) -> Option<DreamWireMarker> {
    let path = marker_path(graph_dir);
    path.is_file().then(|| crate::storage::read_json(&path).ok()).flatten()
}

fn write_marker(graph_dir: &Path, marker: &DreamWireMarker) -> Result<(), String> {
    let path = marker_path(graph_dir);
    crate::storage::create_dir_all(path.parent().ok_or("marker path")?).map_err(|e| e.to_string())?;
    crate::storage::write_json(&path, marker).map_err(|e| e.to_string())
}

fn remove_marker(graph_dir: &Path) {
    let _ = std::fs::remove_file(marker_path(graph_dir));
}

/// One wire per source (document, block) across tonight's published dreams,
/// from the journal. A source shared by two dreams is still one wire, so a
/// block weighs one day's residue however often it was dreamt of.
pub(crate) fn wire_payloads(journal: &str, entries: &[JournalEntry], now_ms: i64) -> Vec<(String, Map<String, Value>)> {
    let sources: BTreeSet<(String, Option<String>)> = entries
        .iter()
        .flat_map(|entry| entry.findings.iter().flat_map(|ranked| ranked.finding.sources()))
        .filter(|(doc, _)| doc != journal)
        .collect();
    sources
        .into_iter()
        .map(|(doc, block)| {
            let mut payload = Map::new();
            payload.insert("wireId".into(), json!(format!("w-dream-{}", &uuid::Uuid::new_v4().simple().to_string()[..12])));
            payload.insert("updatedAt".into(), json!(now_ms as f64));
            payload.insert("predicate".into(), json!(DREAMT_OF));
            payload.insert("targetDocumentId".into(), json!(doc));
            if let Some(block) = block {
                payload.insert("targetBlockId".into(), json!(block));
            }
            (journal.to_string(), payload)
        })
        .collect()
}

/// Delete every dreamtOf wire in the graph: the first act of a cycle.
pub(crate) async fn clear(app: &crate::app_runtime::AppHandle, graph_id: &str, graph_dir: &Path) -> Result<usize, String> {
    let removed = crate::crdt_engine::workspace_ops::remove_wires_where(app, graph_id, "dreaming-clear-dream-wires", |wire| {
        WireFamily::of_wire(wire) == WireFamily::Dream
    })
    .await?;
    remove_marker(graph_dir);
    Ok(removed)
}

/// Write tonight's wires and the marker. Blocking: call it from the
/// publication worker, which holds the graph write lease.
pub(crate) fn publish(
    app: &crate::app_runtime::AppHandle,
    graph_id: &str,
    graph_dir: &Path,
    wires: &[(String, Map<String, Value>)],
    now_ms: i64,
) -> Result<usize, String> {
    let created = crate::app_runtime::async_runtime::block_on(crate::crdt_engine::workspace_ops::create_server_wires(
        app,
        graph_id,
        wires,
        "dreaming-dream-wires",
    ))?;
    if created > 0 {
        write_marker(graph_dir, &DreamWireMarker { expires_at_ms: now_ms + DREAM_WIRE_LIFETIME_MS, count: created })?;
    }
    Ok(created)
}

/// The marker says tonight's wires have expired: sweep now rather than at
/// the next periodic look. Reads one small file.
pub(crate) fn marker_due(graph_dir: &Path, now_ms: i64) -> bool {
    read_marker(graph_dir).is_some_and(|marker| now_ms >= marker.expires_at_ms)
}

/// Whether the sweep deletes this wire: every expired wire, and a dream
/// wire with no readable expiry, which the server never writes.
pub(crate) fn sweepable(wire: &Value, now_ms: i64) -> bool {
    crate::wire_state::wire_expired(wire, now_ms)
        || (WireFamily::of_wire(wire) == WireFamily::Dream && crate::wire_state::wire_expires_at_ms(wire).is_none())
}

/// The sweep reads the workspace itself, not the marker: a duplicated,
/// imported or restored graph, a crash before the marker was written, or a
/// raw CRDT write can each hold dream wires the marker doesn't know about
/// (angel review, 2026-10-19). The marker only makes the sweep prompt.
pub(crate) async fn sweep_expired(
    app: &crate::app_runtime::AppHandle,
    graph_id: &str,
    graph_dir: &Path,
    now_ms: i64,
) -> Result<usize, String> {
    let removed = crate::crdt_engine::workspace_ops::remove_wires_where(app, graph_id, "dreaming-sweep-expired-wires", |wire| {
        sweepable(wire, now_ms)
    })
    .await?;
    if marker_due(graph_dir, now_ms) {
        remove_marker(graph_dir);
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_marker_round_trips_and_is_absent_by_default() {
        let dir = std::env::temp_dir().join(format!("dream-wires-{}", uuid::Uuid::new_v4()));
        assert!(read_marker(&dir).is_none());
        write_marker(&dir, &DreamWireMarker { expires_at_ms: 5, count: 2 }).unwrap();
        assert_eq!(read_marker(&dir), Some(DreamWireMarker { expires_at_ms: 5, count: 2 }));
        remove_marker(&dir);
        assert!(read_marker(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn one_wire_per_source_across_the_night() {
        use super::super::scanner::{CondensedBlock, Condensation, Finding, Isomorphism};
        use super::super::weave::{Charge, Ranked};
        let ranked = |finding: Finding| Ranked { finding, charge: Charge { layers: [0.0; 5], composite: 1.0, is_navel: false }, residue: false };
        let block = |doc: &str, block: &str| CondensedBlock { block: block.into(), doc: doc.into(), text: String::new() };
        let condensation = Finding::Condensation(Condensation {
            blocks: vec![block("a", "b1"), block("a", "b1"), block("b", "b2"), block("journal", "j1")],
            centroid_text: String::new(),
            avg_similarity: 0.9,
        });
        let isomorphism = Finding::Isomorphism(Isomorphism {
            doc_a: "a".into(), doc_b: "c".into(), title_a: String::new(), title_b: String::new(),
            structural_similarity: 1.0, semantic_distance: 1.0, score: 1.0, matched: vec![],
        });
        let entry = |findings: Vec<Finding>| JournalEntry {
            narrative: String::new(), confidence: 0.8, is_navel: false, total_charge: 1.0,
            findings: findings.into_iter().map(ranked).collect(),
        };
        // The same block in two dreams is one wire; the journal never wires itself.
        let entries = vec![entry(vec![condensation.clone(), isomorphism]), entry(vec![condensation])];
        let wires = wire_payloads("journal", &entries, 1_000);
        let targets: Vec<(String, Option<String>)> = wires
            .iter()
            .map(|(source, p)| {
                assert_eq!(source, "journal");
                assert_eq!(p["predicate"], DREAMT_OF);
                (p["targetDocumentId"].as_str().unwrap().to_string(), p.get("targetBlockId").and_then(Value::as_str).map(str::to_string))
            })
            .collect();
        assert_eq!(targets, vec![
            ("a".into(), None),
            ("a".into(), Some("b1".into())),
            ("b".into(), Some("b2".into())),
            ("c".into(), None),
        ]);
    }

    /// Every wire id in the materialized workspace, tombstoned or not (every
    /// write here persists the snapshot).
    #[cfg(feature = "headless")]
    async fn wire_ids(app: &crate::app_runtime::AppHandle, graph_id: &str) -> Vec<String> {
        let graph_dir = crate::graph_paths::existing_graph_dir(app, graph_id).unwrap();
        let snapshot: Value = crate::storage::read_json(&crate::ydoc_paths::workspace_snapshot_path(&graph_dir)).unwrap();
        snapshot["wires"].as_array().unwrap().iter().filter_map(|w| w["id"].as_str().map(str::to_string)).collect()
    }

    #[cfg(feature = "headless")]
    #[test]
    fn dream_wires_are_deleted_for_good_by_the_clear_and_at_expiry() {
        let _serial = crate::tauri_runtime::profile_env_serial().lock().unwrap_or_else(|p| p.into_inner());
        let profile = std::env::temp_dir().join(format!("garden-dream-wires-{}", uuid::Uuid::new_v4()));
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);
        let result = std::panic::catch_unwind(|| {
            crate::app_runtime::async_runtime::block_on(async {
                let app = crate::tauri_runtime::build_mock_app_for_tests(true);
                let graph_id = "dream-wires";
                crate::graph_service::create_graph_service(
                    &app,
                    crate::graph_service::CreateGraphInput {
                        title: "Dream wires".into(),
                        graph_id: Some(graph_id.into()),
                        description: None,
                        operation_id: None,
                    },
                )
                .expect("create graph");
                for id in ["journal", "source"] {
                    crate::document_service::create_document(
                        app.clone(),
                        crate::document_service::CreateDocumentInput { graph_id: graph_id.into(), title: id.into(), document_id: Some(id.into()) },
                    )
                    .expect("create document");
                }
                let graph_dir = crate::graph_paths::existing_graph_dir(&app, graph_id).unwrap();
                let now = chrono::Utc::now().timestamp_millis();
                let wire = |id: &str, predicate: &str| {
                    let mut payload = Map::new();
                    payload.insert("wireId".into(), json!(id));
                    payload.insert("updatedAt".into(), json!(now as f64));
                    payload.insert("predicate".into(), json!(predicate));
                    payload.insert("targetDocumentId".into(), json!("source"));
                    ("journal".to_string(), payload)
                };
                let write = |ids: &[(&str, &str)]| {
                    let wires: Vec<_> = ids.iter().map(|(id, p)| wire(id, p)).collect();
                    let app = app.clone();
                    async move { crate::crdt_engine::workspace_ops::create_server_wires(&app, graph_id, &wires, "test").await.unwrap() }
                };
                write(&[("w-judgment", "supports"), ("w-dream-1", DREAMT_OF)]).await;
                write_marker(&graph_dir, &DreamWireMarker { expires_at_ms: now + DREAM_WIRE_LIFETIME_MS, count: 1 }).unwrap();
                assert_eq!(wire_ids(&app, graph_id).await, vec!["w-dream-1", "w-judgment"]);

                // Before its expiry the sweep leaves it alone.
                assert_eq!(sweep_expired(&app, graph_id, &graph_dir, now + 1000).await.unwrap(), 0);
                assert!(read_marker(&graph_dir).is_some());
                // After it, the wire is gone from the workspace itself, not
                // tombstoned; the judgment wire stays.
                let later = now + DREAM_WIRE_LIFETIME_MS + 1;
                assert_eq!(sweep_expired(&app, graph_id, &graph_dir, later).await.unwrap(), 1);
                assert_eq!(wire_ids(&app, graph_id).await, vec!["w-judgment"]);
                assert!(read_marker(&graph_dir).is_none());

                // The cycle's clear deletes every dream wire, expired or not.
                write(&[("w-dream-2", DREAMT_OF), ("w-dream-3", "http://mnemosyne.ai/vocab#dreamtOf")]).await;
                assert_eq!(clear(&app, graph_id, &graph_dir).await.unwrap(), 2);
                assert_eq!(wire_ids(&app, graph_id).await, vec!["w-judgment"]);

                // A dream wire the marker doesn't know about, written raw with
                // no expiry (as a CRDT client could): it weighs nothing, and the
                // sweep deletes it with no marker present. A live server wire
                // with no marker stays until it expires.
                write(&[("w-dream-live", DREAMT_OF)]).await;
                let room = crate::crdt_engine::workspace_ops::workspace_room(&app, graph_id, &graph_dir).await.unwrap();
                room.update_doc(|_doc, txn| {
                    use yrs::{Map as _, MapPrelim, WriteTxn as _};
                    let wires = txn.get_or_insert_map("wires");
                    let forged = wires.insert(txn, "w-forged", MapPrelim::default());
                    forged.insert(txn, "predicate", "dreamtOf");
                    forged.insert(txn, "sourceDocumentId", "journal");
                    forged.insert(txn, "targetDocumentId", "source");
                    Ok::<_, String>(())
                })
                .await
                .unwrap();
                let forged = json!({ "id": "w-forged", "predicate": "dreamtOf" });
                assert!(!crate::wire_state::wire_counts_toward_salience(&forged));
                assert!(read_marker(&graph_dir).is_none());
                assert_eq!(sweep_expired(&app, graph_id, &graph_dir, now + 1000).await.unwrap(), 1);
                assert_eq!(wire_ids(&app, graph_id).await, vec!["w-dream-live", "w-judgment"]);
                // A sweep with nothing to delete leaves the document alone.
                assert_eq!(sweep_expired(&app, graph_id, &graph_dir, now + 1000).await.unwrap(), 0);
            });
        });
        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
    }
}
