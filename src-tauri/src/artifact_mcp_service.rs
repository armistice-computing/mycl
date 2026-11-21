//! MCP tool handlers for the artifact lifecycle — perceive, type, version, and
//! edit — so the local agent can work with artifacts, not just enumerate them.
//!
//! - `read_artifact`     — perceive an artifact (image → bytes for the agent's
//!                          vision model; text → text; else metadata)
//! - `list_artifact_kinds` — the kind→capability registry (perceive/render/derive)
//! - `list_artifact_revisions` / `restore_artifact_revision` — version control

use crate::app_runtime::AppHandle;
use crate::{
    app_error::{AppError, AppResult},
    artifact_kinds::{kind_from_mime, ARTIFACT_KINDS},
    artifact_revisions::{list_artifact_revisions, restore_artifact_revision},
    hosted_navigation_projection::hosted_navigation_parts,
    json_utils::json_string,
    mcp_utils::{mcp_arg_string, mcp_required_graph_id},
};
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use serde_json::{json, Value};

fn required_artifact_id(args: &Value) -> AppResult<String> {
    mcp_arg_string(args, &["artifactId", "artifact_id"])
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| AppError::validation("artifact_id is required"))
}

fn graph_id(args: &Value) -> AppResult<String> {
    mcp_required_graph_id(args).map_err(AppError::validation)
}

/// Default page of text `read_artifact` returns; an attached export can be
/// hundreds of megabytes, and a page never floods a context.
pub(crate) const READ_ARTIFACT_DEFAULT_MAX_CHARS: usize = 65_536;
/// Above this the tool returns the record and a note, never the bytes: a
/// 400 MB attached export is downloaded, not decoded into a reply.
pub(crate) const READ_ARTIFACT_INLINE_MAX_BYTES: usize = 32 * 1024 * 1024;

/// One page of a text, cut on a character boundary: the page, the offset of
/// the next page (None at the end) and the whole length, all in chars.
pub(crate) fn page_text(
    text: &str,
    offset: usize,
    max_chars: usize,
) -> (String, Option<usize>, usize) {
    let total = text.chars().count();
    let page: String = text.chars().skip(offset).take(max_chars).collect();
    let taken = page.chars().count();
    let next = if offset + taken < total {
        Some(offset + taken)
    } else {
        None
    };
    (page, next, total)
}

fn optional_usize(args: &Value, keys: &[&str]) -> AppResult<Option<usize>> {
    for key in keys {
        if let Some(value) = args.get(*key) {
            if value.is_null() {
                continue;
            }
            return value
                .as_u64()
                .map(|value| Some(value as usize))
                .ok_or_else(|| {
                    AppError::validation(format!("{key} must be a non-negative integer"))
                });
        }
    }
    Ok(None)
}

/// Perceive an artifact and its record. Images return base64 bytes (the
/// frontend tool wrapper turns this into an image block for the agent's
/// vision model); text returns one page of decoded text; everything else
/// returns metadata. The record's mode, hash and light index ride along when
/// the artifact has one (an old record may not).
pub(super) fn mcp_local_read_artifact(app: AppHandle, args: &Value) -> AppResult<Value> {
    let graph_id = graph_id(args)?;
    let offset = optional_usize(args, &["offset"])?.unwrap_or(0);
    let max_chars = optional_usize(args, &["max_chars", "maxChars"])?
        .unwrap_or(READ_ARTIFACT_DEFAULT_MAX_CHARS)
        .max(1);
    // A region read: by the wire that names it (stale, re-anchor) or by a
    // literal region. The artifact comes from the wire when one is given.
    let wire_id = mcp_arg_string(args, &["wire_id", "wireId"]).filter(|value| !value.trim().is_empty());
    let region_arg = args.get("region").filter(|value| !value.is_null()).cloned();
    if wire_id.is_some() || region_arg.is_some() {
        let artifact_id = mcp_arg_string(args, &["artifactId", "artifact_id"]).filter(|value| !value.trim().is_empty());
        return read_artifact_region(&app, &graph_id, artifact_id, wire_id, region_arg, offset, max_chars);
    }
    let artifact_id = required_artifact_id(args)?;
    let record = crate::artifact_storage::artifact_record(&app, &graph_id, &artifact_id)
        .map_err(AppError::storage)?;
    if let Some(record) = record.as_ref().filter(|r| crate::artifact_reference::record_is_referenced(r)) {
        return read_referenced_artifact(&app, &graph_id, &artifact_id, record, offset, max_chars);
    }
    // A bounded window: the inline cap plus one byte, never the whole of a
    // 400 MB export, to page one screen of text.
    let (manifest, bytes, total_len) = crate::original_file_service::read_artifact_original_window(
        &app,
        &graph_id,
        &artifact_id,
        READ_ARTIFACT_INLINE_MAX_BYTES + 1,
    )?;
    let file_type = crate::crdt_engine::workspace_ops::artifact_file_type(&manifest.filename);
    let mut kind = kind_from_mime(&manifest.mime_type, Some(&file_type));
    if kind == "binary" && crate::artifact_reference::looks_like_text(&bytes) {
        kind = "text";
    }
    // Where the bytes are on this computer, for an agent whose own harness
    // can open files (Eschaton, 2026-11-02): a pointer for its tools, not a
    // door through ours. The store sits inside the profile, which Garden's
    // own path reads never enter.
    let local_path = crate::artifact_storage::resolve_artifact_original_dir(&app, &graph_id, &artifact_id)
        .ok()
        .and_then(|dir| crate::original_file_manifest_store::original_manifest_file_path(&dir, &manifest.filename).ok())
        .map(|path| path.to_string_lossy().to_string());
    let base = json!({
        "artifactId": artifact_id,
        "kind": kind,
        "mimeType": manifest.mime_type,
        "filename": manifest.filename,
        "sizeBytes": total_len,
        "localPath": local_path,
    });
    let mut out = base.as_object().cloned().unwrap_or_default();
    // `sha256` is the record's hash, the bytes as the graph saw them at
    // attach; `storedSha256` is the store's, which a revision may have moved.
    // When they differ the record is stale and says so.
    if let Some(hash) = manifest.sha256.as_deref() {
        out.insert("storedSha256".into(), json!(hash));
    }
    if let Ok((_, _, artifacts)) = hosted_navigation_parts(&app, &graph_id) {
        if let Some(record) = artifacts.into_iter().find(|artifact| {
            json_string(artifact.get("id")).as_deref() == Some(artifact_id.as_str())
        }) {
            for key in [
                "mode",
                "storage",
                "sha256",
                "hashedAt",
                "source",
                "index",
                "ingestedDocId",
                "label",
            ] {
                if let Some(value) = record.get(key).filter(|value| !value.is_null()) {
                    out.insert(key.into(), value.clone());
                }
            }
            if let (Some(record_hash), Some(stored)) = (
                json_string(record.get("sha256")),
                manifest.sha256.as_deref(),
            ) {
                out.insert("stale".into(), json!(record_hash != stored));
            }
        }
    }
    let inline_bytes_ok = total_len <= READ_ARTIFACT_INLINE_MAX_BYTES;
    match kind {
        _ if !inline_bytes_ok => {
            out.insert(
                "note".into(),
                json!(format!(
                    "artifact is {} bytes; inline content stops at {} bytes, download it instead",
                    total_len,
                    READ_ARTIFACT_INLINE_MAX_BYTES
                )),
            );
        }
        "image" => {
            out.insert("dataBase64".into(), json!(BASE64_STANDARD.encode(&bytes)));
        }
        // Text and scenes (Excalidraw JSON) return their content as text so the
        // agent can read it structurally, one page at a time.
        "text" | "scene" => {
            let text = String::from_utf8_lossy(&bytes);
            let (page, next_offset, total_chars) = page_text(&text, offset, max_chars);
            out.insert("text".into(), json!(page));
            out.insert("offset".into(), json!(offset));
            out.insert("nextOffset".into(), json!(next_offset));
            out.insert("totalChars".into(), json!(total_chars));
            out.insert("complete".into(), json!(next_offset.is_none()));
        }
        _ => {
            out.insert("note".into(), json!("binary artifact; no inline preview"));
        }
    }
    Ok(Value::Object(out))
}

/// The region a wire names, read now beside what the side store kept:
/// `stale: { file, region }`, both hashes, the current text (paged), and a
/// re-anchor by the stored text when the lines moved. Or a literal
/// `region` on an artifact, with no stored side. Gated by `artifacts.read`
/// like every read of this tool.
#[allow(clippy::too_many_arguments)]
fn read_artifact_region(
    app: &AppHandle,
    graph_id: &str,
    artifact_id: Option<String>,
    wire_id: Option<String>,
    region_arg: Option<Value>,
    offset: usize,
    max_chars: usize,
) -> AppResult<Value> {
    use crate::artifact_regions::{read_region_now, read_stored_region, Region};
    let graph_dir = crate::paths::existing_graph_dir(app, graph_id).map_err(AppError::storage)?;
    let (artifact_id, region, stored, wire_id) = match wire_id {
        Some(wire_id) => {
            let workspace = crate::document_service::read_workspace_record(&graph_dir, graph_id)
                .map_err(AppError::storage)?;
            let wire = crate::workspace_entity_projection::workspace_wires(workspace.snapshot.as_ref())
                .into_iter()
                .find(|wire| json_string(wire.get("id")).as_deref() == Some(wire_id.as_str()))
                .ok_or_else(|| AppError::validation(format!("wire not found: {wire_id}")))?;
            let target = json_string(wire.get("targetArtifactId"))
                .ok_or_else(|| AppError::validation(format!("wire {wire_id} does not point at an artifact")))?;
            if let Some(given) = artifact_id.as_deref() {
                if given != target {
                    return Err(AppError::validation(format!(
                        "wire {wire_id} points at artifact {target}, not {given}"
                    )));
                }
            }
            let literal = region_arg.is_some();
            let region_value = region_arg
                .or_else(|| wire.get("targetRegion").filter(|value| value.is_object()).cloned())
                .ok_or_else(|| {
                    AppError::validation(format!("wire {wire_id} names no region; pass region to read one"))
                })?;
            let region = Region::parse(&region_value).map_err(AppError::validation)?;
            // The side store is trusted only for the wire it names and the
            // artifact that wire points at; anything else is an orphan. A
            // literal region beside a wire id reads now with no stored side
            // (the stored hashes belong to the wire's own region). A file
            // that cannot be read degrades to "missing" rather than failing.
            let stored = if literal {
                None
            } else {
                match read_stored_region(&graph_dir, &wire_id) {
                    Ok(stored) => stored.filter(|stored| stored.artifact_id == target),
                    Err(error) => {
                        log::warn!("artifact region store for {wire_id} unreadable: {error}");
                        None
                    }
                }
            };
            (target, region, stored, Some(wire_id))
        }
        None => {
            let artifact_id = artifact_id.ok_or_else(|| AppError::validation("artifact_id or wire_id is required"))?;
            let region = Region::parse(region_arg.as_ref().expect("checked by caller")).map_err(AppError::validation)?;
            (artifact_id, region, None, None)
        }
    };
    let record = crate::artifact_storage::artifact_record(app, graph_id, &artifact_id)
        .map_err(AppError::storage)?
        .ok_or_else(|| AppError::validation(format!("artifact not found: {artifact_id}")))?;
    let mut out = read_region_now(app, graph_id, &artifact_id, &record, &region, stored.as_ref())
        .map_err(|error| {
            if error.starts_with("denied:") || error.starts_with("missing:") {
                AppError::validation(error)
            } else {
                AppError::internal(error)
            }
        })?;
    out["artifactId"] = json!(artifact_id);
    if let Some(wire_id) = wire_id {
        out["wireId"] = json!(wire_id);
        if stored.is_none() {
            out["regionStore"] = json!("missing");
            out["note"] = json!("no side-store record for this wire; staleness cannot be judged, the region is read as it is now");
        }
    }
    if let Some(text) = out.get("regionText").and_then(Value::as_str).map(str::to_string) {
        let (page, next_offset, total_chars) = page_text(&text, offset, max_chars);
        out["regionText"] = json!(page);
        out["offset"] = json!(offset);
        out["nextOffset"] = json!(next_offset);
        out["totalChars"] = json!(total_chars);
        out["complete"] = json!(next_offset.is_none());
    }
    Ok(out)
}

/// A referenced artifact, read in place: re-authorized and re-opened now,
/// with `state` (ok | stale | missing | denied), the record's hash beside
/// the current one, and one page of text when it is text.
fn read_referenced_artifact(
    app: &AppHandle,
    graph_id: &str,
    artifact_id: &str,
    record: &Value,
    offset: usize,
    max_chars: usize,
) -> AppResult<Value> {
    use crate::artifact_reference::{read_referenced, ReadState};
    let read = read_referenced(app, graph_id, record, READ_ARTIFACT_INLINE_MAX_BYTES + 1);
    let file_type = crate::crdt_engine::workspace_ops::artifact_file_type(&read.filename);
    let mut kind = kind_from_mime(&read.mime_type, Some(&file_type));
    if kind == "binary" && crate::artifact_reference::looks_like_text(&read.bytes) {
        kind = "text";
    }
    let mut out = json!({
        "artifactId": artifact_id,
        "kind": kind,
        "mimeType": read.mime_type,
        "filename": read.filename,
        "localPath": read.path,
        "state": read.state.name(),
        "mode": "referenced",
        "storage": "external",
    })
    .as_object()
    .cloned()
    .unwrap_or_default();
    for key in ["sha256", "hashedAt", "source", "index", "label", "sizeBytes"] {
        if let Some(value) = record.get(key).filter(|value| !value.is_null()) {
            out.insert(key.into(), value.clone());
        }
    }
    if let Some(note) = read.note.as_deref() {
        out.insert("note".into(), json!(note));
    }
    match read.state {
        ReadState::Missing | ReadState::Denied => return Ok(Value::Object(out)),
        ReadState::Stale => {
            out.insert("stale".into(), json!(true));
        }
        ReadState::Ok => {
            out.insert("stale".into(), json!(false));
        }
    }
    out.insert("sizeBytes".into(), json!(read.size_bytes));
    if let Some(now) = read.sha256_now.as_deref() {
        out.insert("storedSha256".into(), json!(now));
    }
    let total_len = read.size_bytes as usize;
    if total_len > READ_ARTIFACT_INLINE_MAX_BYTES {
        out.insert(
            "note".into(),
            json!(format!(
                "artifact is {total_len} bytes; inline content stops at {} bytes, download it instead",
                READ_ARTIFACT_INLINE_MAX_BYTES
            )),
        );
    } else {
        match kind {
            "image" => {
                out.insert("dataBase64".into(), json!(BASE64_STANDARD.encode(&read.bytes)));
            }
            "text" | "scene" => {
                let text = String::from_utf8_lossy(&read.bytes);
                let (page, next_offset, total_chars) = page_text(&text, offset, max_chars);
                out.insert("text".into(), json!(page));
                out.insert("offset".into(), json!(offset));
                out.insert("nextOffset".into(), json!(next_offset));
                out.insert("totalChars".into(), json!(total_chars));
                out.insert("complete".into(), json!(next_offset.is_none()));
            }
            _ => {
                out.insert("note".into(), json!("binary artifact; no inline preview"));
            }
        }
    }
    Ok(Value::Object(out))
}

/// Make a reference to a file under one of the owner's allowed roots.
pub(super) async fn mcp_local_reference_artifact(app: AppHandle, args: &Value) -> AppResult<Value> {
    let graph_id = graph_id(args)?;
    let path = mcp_arg_string(args, &["path", "file_path", "filePath"])
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| AppError::validation("path is required"))?;
    crate::artifact_reference::reference_path(
        app,
        crate::artifact_reference::ReferenceInput {
            graph_id,
            path,
            label: mcp_arg_string(args, &["label"]),
            parent_id: mcp_arg_string(args, &["parent_id", "parentId"]),
        },
    )
    .await
    .map_err(|error| {
        if error.starts_with("denied:") || error.starts_with("missing:") {
            AppError::validation(error)
        } else {
            AppError::internal(error)
        }
    })
}

/// The artifact-kind registry: each kind's RDF class + perceive/render/derive
/// capabilities. Lets the agent discover what it can do with each kind.
pub(super) fn mcp_local_list_artifact_kinds(_app: AppHandle, _args: &Value) -> AppResult<Value> {
    let kinds: Vec<Value> = ARTIFACT_KINDS
        .iter()
        .map(|kind| {
            json!({
                "kind": kind.slug,
                "rdfClass": kind.class,
                "perceive": kind.perceive,
                "render": kind.render,
                "derive": kind.derive,
            })
        })
        .collect();
    Ok(json!({ "kinds": kinds }))
}

pub(super) fn mcp_local_list_artifact_revisions(app: AppHandle, args: &Value) -> AppResult<Value> {
    let graph_id = graph_id(args)?;
    let artifact_id = required_artifact_id(args)?;
    let revisions = list_artifact_revisions(&app, &graph_id, &artifact_id)?;
    Ok(json!({ "revisions": revisions }))
}

pub(super) async fn mcp_local_restore_artifact_revision(
    app: AppHandle,
    args: &Value,
) -> AppResult<Value> {
    let graph_id = graph_id(args)?;
    let artifact_id = required_artifact_id(args)?;
    let revision_id = mcp_arg_string(args, &["revisionId", "revision_id"])
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| AppError::validation("revision_id is required"))?;
    let checkpoint = restore_artifact_revision(&app, &graph_id, &artifact_id, &revision_id)?;
    crate::artifact_storage::refresh_owned_record_after_write(app, &graph_id, &artifact_id)
        .await
        .map_err(AppError::storage)?;
    Ok(json!({ "restored": revision_id, "checkpoint": checkpoint }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_pages_cut_on_characters_and_say_where_the_next_page_is() {
        let text = "héllo wörld";
        let (page, next, total) = page_text(text, 0, 5);
        assert_eq!((page.as_str(), next, total), ("héllo", Some(5), 11));
        let (page, next, _) = page_text(text, 5, 100);
        assert_eq!((page.as_str(), next), (" wörld", None));
        let (page, next, _) = page_text(text, 50, 10);
        assert_eq!((page.as_str(), next), ("", None));
    }

}
