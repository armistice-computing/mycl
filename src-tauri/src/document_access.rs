//! Effective document access: mutable import protection and immutable view ownership.
use crate::{document_service::DocumentRecord, json_utils::json_bool};
use serde::Serialize;
use serde_json::Value;
use std::{collections::HashMap, path::Path};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DocumentAccess {
    pub(crate) read_only: bool,
    pub(crate) derived: bool,
    pub(crate) read_only_owner: Option<&'static str>,
}

impl DocumentAccess {
    pub(crate) fn insert_into(&self, value: &mut Value) {
        value["readOnly"] = Value::Bool(self.read_only);
        value["derived"] = Value::Bool(self.derived);
        value["readOnlyOwner"] = self
            .read_only_owner
            .map(|s| Value::String(s.into()))
            .unwrap_or(Value::Null);
    }
}

pub(crate) fn resolve(
    graph_dir: &Path,
    id: &str,
    record: Option<&DocumentRecord>,
    workspace_read_only: bool,
) -> Result<DocumentAccess, String> {
    if record.is_some_and(|record| record.document_id != id) {
        return Err("document access metadata identity mismatch".into());
    }
    let capabilities = record.map(|r| r.capabilities.as_slice()).unwrap_or(&[]);
    let derived = crate::geist_derived_view::from_capabilities(graph_dir, id, capabilities)?;
    Ok(DocumentAccess {
        read_only: derived || workspace_read_only,
        derived,
        read_only_owner: derived.then_some("geist"),
    })
}

pub(crate) fn workspace_flag(entry: &Value) -> bool {
    json_bool(entry.get("readOnly").or_else(|| entry.get("read_only"))).unwrap_or(false)
}

/// No CRDT hydration or repairs just to annotate a bounded document response.
pub(crate) fn workspace_flags(graph_dir: &Path) -> Result<HashMap<String, bool>, String> {
    Ok(workspace_snapshot_for_read(graph_dir)?
        .map(|snapshot| workspace_flags_from(&snapshot))
        .unwrap_or_default())
}

/// The persisted workspace snapshot, parsed once for a bounded read (no CRDT
/// hydration), or `None` for a graph that has never written one.
pub(crate) fn workspace_snapshot_for_read(graph_dir: &Path) -> Result<Option<Value>, String> {
    let path = crate::paths::workspace_snapshot_path(graph_dir);
    if !path.is_file() {
        return Ok(None);
    }
    crate::storage::read_json(&path).map(Some).map_err(|e| e.to_string())
}

pub(crate) fn workspace_flags_from(snapshot: &Value) -> HashMap<String, bool> {
    snapshot["documents"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let id = entry
                .get("id")
                .or_else(|| entry.get("documentId"))
                .or_else(|| entry.get("document_id"))?
                .as_str()?;
            Some((id.to_string(), workspace_flag(entry)))
        })
        .collect()
}

pub(crate) fn enrich_workspace_documents(
    graph_dir: &Path,
    documents: &mut [Value],
    records: &[DocumentRecord],
) -> Result<(), String> {
    let by_id: HashMap<_, _> = records
        .iter()
        .map(|r| (r.document_id.as_str(), r))
        .collect();
    for entry in documents {
        let id = entry
            .get("id")
            .or_else(|| entry.get("documentId"))
            .or_else(|| entry.get("document_id"))
            .and_then(Value::as_str)
            .ok_or("workspace document has no ID")?;
        // A client can write any key into the workspace map. A row whose id is
        // not a local id names no document body and no derived view, so it
        // keeps only its workspace flag. Resolving it would refuse the id and
        // fail the whole workspace read.
        if crate::ids::validate_local_id(id, "document_id").is_err() {
            DocumentAccess {
                read_only: workspace_flag(entry),
                derived: false,
                read_only_owner: None,
            }
            .insert_into(entry);
            continue;
        }
        let access = resolve(graph_dir, id, by_id.get(id).copied(), workspace_flag(entry))?;
        access.insert_into(entry);
    }
    Ok(())
}
