//! Server-owned read-only projection registration, independent of workspace flags.
use serde_json::{json, Value};
use std::path::Path;

fn is_registered(graph_dir: &Path, document_id: &str) -> Result<bool, String> {
    crate::ids::validate_local_id(document_id, "document_id")?;
    let marker = graph_dir
        .join("narrative/derived-views")
        .join(format!("{document_id}.json"));
    if marker.is_file() {
        let value: Value = crate::storage::read_json(&marker).map_err(|e| e.to_string())?;
        if value["schemaVersion"] != 1 || value["documentId"] != document_id {
            return Err("invalid derived Geist view registration".into());
        }
        return Ok(true);
    }
    Ok(false)
}

pub(crate) fn from_capabilities(graph_dir: &Path, document_id: &str, capabilities: &[String]) -> Result<bool, String> {
    Ok(is_registered(graph_dir, document_id)? || capabilities.iter().any(|cap| derived_capability(cap)))
}

fn derived_capability(cap: &str) -> bool {
    matches!(
        cap,
        "narrative.local.song"
            | "narrative.local.archive"
            | "narrative.local.diary"
            | "memory.local.archive"
            | "salience.local.importance"
    )
}

pub(crate) fn is_derived(graph_dir: &Path, document_id: &str) -> Result<bool, String> {
    if is_registered(graph_dir, document_id)? { return Ok(true); }
    // Recognize already-published views from before registration existed.
    let record = graph_dir
        .join("documents")
        .join(document_id)
        .join("document.json");
    if !record.is_file() {
        return Ok(false);
    }
    let value: Value = crate::storage::read_json(&record).map_err(|e| e.to_string())?;
    Ok(value["capabilities"].as_array().is_some_and(|caps| {
        caps.iter().any(|cap| cap.as_str().is_some_and(derived_capability))
    }))
}

pub(crate) fn require_editable(graph_dir: &Path, document_id: &str) -> Result<(), String> {
    if is_derived(graph_dir, document_id)? {
        return Err(
            "Derived Geist views are read-only; change the native Geist store through its tools"
                .into(),
        );
    }
    Ok(())
}

pub(crate) fn register(graph_dir: &Path, document_id: &str) -> Result<(), String> {
    crate::ids::validate_local_id(document_id, "document_id")?;
    let dir = graph_dir.join("narrative/derived-views");
    crate::storage::create_dir_all(&dir)?;
    crate::storage::write_json(
        &dir.join(format!("{document_id}.json")),
        &json!({"schemaVersion":1,"documentId":document_id}),
    )
    .map_err(|e| e.to_string())
}

pub(crate) fn state_path_is_derived(path: &Path) -> Result<bool, String> {
    state_path_is_derived_impl(path, false)
}

/// Hot-path check: ordinary documents need one small marker stat, not a full
/// document.json parse on every keystroke. Legacy capabilities are read at open.
pub(crate) fn state_path_is_registered(path: &Path) -> Result<bool, String> {
    state_path_is_derived_impl(path, true)
}

fn state_path_is_derived_impl(path: &Path, registered_only: bool) -> Result<bool, String> {
    let Some(doc_dir) = path.parent() else {
        return Ok(false);
    };
    let Some(documents) = doc_dir.parent() else {
        return Ok(false);
    };
    if documents.file_name().and_then(|s| s.to_str()) != Some("documents") {
        return Ok(false);
    }
    let Some(ydocs) = documents.parent() else {
        return Ok(false);
    };
    if ydocs.file_name().and_then(|s| s.to_str()) != Some("ydocs") {
        return Ok(false);
    }
    let graph_dir = ydocs.parent().ok_or("missing graph directory")?;
    let id = doc_dir
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or("invalid document path")?;
    if registered_only
        && !graph_dir
            .join("narrative/derived-views")
            .join(format!("{id}.json"))
            .exists()
    {
        return Ok(false);
    }
    is_derived(graph_dir, id)
}

pub(crate) fn require_editable_state_path(path: &Path) -> Result<(), String> {
    if state_path_is_derived(path)? {
        return Err("Derived Geist views are read-only".into());
    }
    Ok(())
}
