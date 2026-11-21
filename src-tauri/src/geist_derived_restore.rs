//! Preserve native Geist projections while restoring ordinary graph documents.
use crate::{
    app_error::{AppError, AppResult},
    crdt_engine::workspace_ops::materialize_workspace_snapshot_json,
    geist_derived_view::is_derived,
    storage::read_json,
    time_travel_store::read_workspace_bundle,
    time_travel_types::RestorePointManifest,
    ydoc_paths::{workspace_snapshot_path, workspace_ydoc_state_path},
};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};
use yrs::{
    types::ToJson, updates::decoder::Decode, Any, Doc, Map, MapPrelim, Out, ReadTxn, StateVector,
    Transact, Update,
};

pub(crate) struct PreservedRestore {
    pub(crate) derived_ids: BTreeSet<String>,
    pub(crate) workspace_bytes: Vec<u8>,
    pub(crate) workspace_snapshot: Value,
}

fn decode_workspace(bytes: &[u8]) -> AppResult<Doc> {
    let doc = Doc::new();
    if !bytes.is_empty() {
        let update = Update::decode_v1(bytes)
            .map_err(|error| AppError::storage(format!("decode restore workspace: {error}")))?;
        doc.transact_mut()
            .apply_update(update)
            .map_err(|error| AppError::storage(format!("apply restore workspace: {error}")))?;
    }
    Ok(doc)
}

fn document_rows(
    doc: &Doc,
    derived_ids: &BTreeSet<String>,
) -> AppResult<BTreeMap<String, BTreeMap<String, Any>>> {
    let txn = doc.transact();
    let mut rows = BTreeMap::new();
    if let Some(documents) = txn.get_map("documents") {
        for (id, row) in documents.iter(&txn) {
            if !derived_ids.contains(id) {
                continue;
            }
            let Out::YMap(row) = row else {
                return Err(AppError::storage(format!(
                    "invalid workspace document row: {id}"
                )));
            };
            rows.insert(
                id.to_string(),
                row.iter(&txn)
                    .map(|(key, value)| (key.to_string(), value.to_json(&txn)))
                    .collect(),
            );
        }
    }
    Ok(rows)
}

fn collect_disk_ids(graph_dir: &Path, ids: &mut BTreeSet<String>) -> AppResult<()> {
    for (relative, markers) in [("documents", false), ("narrative/derived-views", true)] {
        let path = graph_dir.join(relative);
        if !path.exists() {
            continue;
        }
        for entry in
            std::fs::read_dir(path).map_err(|error| AppError::storage(error.to_string()))?
        {
            let entry = entry.map_err(|error| AppError::storage(error.to_string()))?;
            let path = entry.path();
            let id = if markers {
                if path.extension().and_then(|value| value.to_str()) != Some("json") {
                    continue;
                }
                path.file_stem()
            } else {
                if !path.is_dir() {
                    continue;
                }
                path.file_name()
            };
            if let Some(id) = id.and_then(|value| value.to_str()) {
                ids.insert(id.to_string());
            }
        }
    }
    Ok(())
}

/// All fallible discovery and workspace transformation precedes restore writes.
pub(crate) fn prepare(
    graph_dir: &Path,
    manifest: &RestorePointManifest,
) -> AppResult<PreservedRestore> {
    let (bytes, snapshot) = read_workspace_bundle(graph_dir, &manifest.restore_point_id)?;
    // Workspace references cannot grant or remove derived status. Keep ordinary
    // restore bundles byte-identical, without imposing a new CRDT schema check.
    let mut candidates: BTreeSet<String> = manifest
        .documents
        .iter()
        .map(|doc| doc.document_id.clone())
        .collect();
    collect_disk_ids(graph_dir, &mut candidates)?;
    let mut derived_ids = BTreeSet::new();
    for id in candidates {
        if is_derived(graph_dir, &id).map_err(AppError::storage)? {
            derived_ids.insert(id);
        }
    }
    if derived_ids.is_empty() {
        return Ok(PreservedRestore {
            derived_ids,
            workspace_bytes: bytes,
            workspace_snapshot: snapshot,
        });
    }
    if bytes.is_empty()
        && ["documents", "folders", "artifacts", "wires"]
            .iter()
            .any(|key| {
                snapshot
                    .get(*key)
                    .and_then(Value::as_array)
                    .is_some_and(|rows| !rows.is_empty())
            })
    {
        return Err(AppError::storage(
            "cannot preserve derived views in a snapshot-only restore bundle without workspace Y.Doc bytes",
        ));
    }
    let target = decode_workspace(&bytes)?;
    let current_path = workspace_ydoc_state_path(graph_dir);
    let current_bytes = if current_path.is_file() {
        std::fs::read(&current_path).map_err(|error| AppError::storage(error.to_string()))?
    } else {
        Vec::new()
    };
    let current = decode_workspace(&current_bytes)?;
    let mut current_rows = document_rows(&current, &derived_ids)?;
    // A legacy workspace may have only its materialized catalog on disk.
    if current_bytes.is_empty() && workspace_snapshot_path(graph_dir).is_file() {
        let snapshot: Value = read_json(&workspace_snapshot_path(graph_dir))?;
        if let Some(rows) = snapshot["documents"].as_array() {
            for row in rows {
                let Some(id) = row
                    .get("id")
                    .or_else(|| row.get("documentId"))
                    .or_else(|| row.get("document_id"))
                    .and_then(Value::as_str)
                else {
                    continue;
                };
                if !derived_ids.contains(id) {
                    continue;
                }
                let values = row
                    .as_object()
                    .ok_or_else(|| AppError::storage("invalid document row"))?;
                let mut fields = BTreeMap::new();
                for (key, value) in values {
                    fields.insert(
                        key.clone(),
                        Any::from_json(&value.to_string()).map_err(|error| {
                            AppError::storage(format!("decode workspace field: {error}"))
                        })?,
                    );
                }
                current_rows.insert(id.to_string(), fields);
            }
        }
    }
    let documents = target.get_or_insert_map("documents");
    {
        let mut txn = target.transact_mut();
        let folders = txn.get_map("folders");
        for id in &derived_ids {
            let fields = if let Some(fields) = current_rows.get(id) {
                Some(fields.clone())
            } else {
                // Projected records can exist before their first workspace flush.
                let path = graph_dir.join("documents").join(id).join("document.json");
                if path.is_file() {
                    let record: Value = read_json(&path)?;
                    Some(BTreeMap::from([
                        (
                            "title".into(),
                            Any::from(record["title"].as_str().unwrap_or("Untitled")),
                        ),
                        ("section".into(), Any::from("documents")),
                        ("order".into(), Any::Number(0.0)),
                    ]))
                } else {
                    None
                }
            };
            documents.remove(&mut txn, id);
            let Some(mut fields) = fields else {
                continue;
            };
            let parent_exists = match fields.get("parentId") {
                Some(Any::String(parent)) => folders.as_ref().is_some_and(|folders| {
                    matches!(folders.get(&txn, parent.as_ref()), Some(Out::YMap(_)))
                }),
                _ => false,
            };
            if !parent_exists {
                fields.insert("parentId".into(), Any::Null);
            }
            fields.insert("readOnly".into(), Any::Bool(true));
            let row = documents.insert(&mut txn, id.as_str(), MapPrelim::default());
            for (key, value) in fields {
                row.insert(&mut txn, key, value);
            }
        }
    }
    let workspace_snapshot = materialize_workspace_snapshot_json(&manifest.graph_id, &target)
        .map_err(AppError::storage)?;
    let workspace_bytes = target
        .transact()
        .encode_state_as_update_v1(&StateVector::default());
    Ok(PreservedRestore {
        derived_ids,
        workspace_bytes,
        workspace_snapshot,
    })
}
