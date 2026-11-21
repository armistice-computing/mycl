//! Attach-only entry for artifacts: the bytes are kept, hashed and lightly
//! indexed, and no block is made. This is the first rung of the ladder in
//! "Artifacts — a body outside the house" (design seed, 2026-10-23):
//! referenced, attached, digested. A person or an agent climbs it on purpose;
//! nothing climbs it by accident.
//!
//! The record is the workspace `artifacts` map entry (`workspace.putArtifact`),
//! so wires can target it and the settings list can show it. The bytes live
//! under `artifacts/{id}/original/` beside a manifest that carries the hash.

use crate::app_runtime::AppHandle;
use crate::{
    artifact_kinds::kind_from_mime,
    clock::timestamp,
    crdt_engine::workspace_ops::artifact_file_type,
    crdt_projection_flush::flush_graph_projection,
    crdt_queue::{enqueue_crdt_operation, EnqueueCrdtOperationInput},
    ids::safe_filename,
    original_file_manifest_store::{stored_original_filename, write_original_manifest_hashed},
    paths::{artifact_original_dir, existing_graph_dir, sha256_of_file},
    storage::{copy_file_atomic, create_dir_all, display_path, remove_file_if_exists},
};
use serde_json::{json, Value};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};
use uuid::Uuid;

/// How a file enters: as bytes only, or as blocks (today's upload job).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EntryMode {
    Attach,
    Digest,
}

impl EntryMode {
    /// `attach` | `digest`, case-insensitive; `attached`/`digested` accepted
    /// as the record's own words. Anything else is refused by name.
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "attach" | "attached" => Ok(Self::Attach),
            "digest" | "digested" | "ingest" => Ok(Self::Digest),
            other => Err(format!("mode must be 'attach' or 'digest' (got '{other}')")),
        }
    }

    /// The default when the door names no mode: digest what the upload job
    /// can parse here (markdown and plain text always; every type when a
    /// parser pool is configured, as before), attach the rest. Before this,
    /// a file no parser handled failed the upload.
    pub(crate) fn default_for(filename: &str, mime_type: &str) -> Self {
        Self::default_for_node(
            filename,
            mime_type,
            crate::crdt_engine::upload_ingest_ops::parser_pool_endpoint().is_some(),
        )
    }

    pub(crate) fn default_for_node(filename: &str, mime_type: &str, has_parser_pool: bool) -> Self {
        if has_parser_pool || digestable_at_the_door(filename, mime_type) {
            Self::Digest
        } else {
            Self::Attach
        }
    }
}

/// Mirrors the markdown/plain-text passthrough in `upload_ingest_ops`.
pub(crate) fn digestable_at_the_door(filename: &str, mime_type: &str) -> bool {
    let lower = filename.to_ascii_lowercase();
    let mime = mime_type.trim().to_ascii_lowercase();
    mime.starts_with("text/markdown")
        || mime.starts_with("text/plain")
        || lower.ends_with(".md")
        || lower.ends_with(".markdown")
        || lower.ends_with(".txt")
}

pub(crate) struct AttachInput {
    pub(crate) graph_id: String,
    pub(crate) filename: String,
    pub(crate) mime_type: String,
    /// A file under the graph's pending-uploads directory; it is moved, not
    /// copied, into the artifact store.
    pub(crate) pending_path: PathBuf,
    pub(crate) size_bytes: usize,
    /// The hash the streaming writer computed, or `None` to hash the file now.
    pub(crate) sha256: Option<String>,
    pub(crate) parent_id: Option<String>,
    pub(crate) label: Option<String>,
}

/// Up to this many entries go into the light index's table of contents.
pub(crate) const INDEX_TOC_CAP: usize = 500;
/// JSON larger than this is listed, not parsed, when building the index. The
/// parse is `serde_json::Value`, about twenty times the bytes in memory.
pub(crate) const INDEX_JSON_PARSE_CAP: u64 = 8 * 1024 * 1024;
/// A zip with more entries than this is not walked; its central directory
/// alone would cost hundreds of megabytes (angel review F5b).
pub(crate) const INDEX_ZIP_ENTRY_CAP: usize = 100_000;
/// Each label in the index is cut here; member names are attacker text.
pub(crate) const INDEX_LABEL_CAP: usize = 200;

/// Attach a pending upload as an artifact: move the bytes into the store,
/// write the hashed manifest, build the light index, put the record and
/// flush the projection. Synchronous; the reply carries the record.
pub(crate) async fn attach_pending_upload(
    app: AppHandle,
    input: AttachInput,
) -> Result<Value, String> {
    let graph_dir = existing_graph_dir(&app, &input.graph_id)?;
    let artifact_id = format!("artifact-{}", Uuid::new_v4().simple());
    let filename = {
        let safe = safe_filename(&input.filename);
        if safe.is_empty() {
            "unnamed".to_string()
        } else {
            safe
        }
    };
    // The record keeps the user's name; the store may use another one.
    let stored_name = stored_original_filename(&filename).to_string();
    let mime_type = if input.mime_type.trim().is_empty() {
        "application/octet-stream".to_string()
    } else {
        input.mime_type.clone()
    };

    // The file work (a hash of up to 500 MiB, a rename, the index) runs on a
    // blocking thread, and the hash is taken before the graph's write lease,
    // so no graph write waits on it (angel review F5c, F5d).
    let (manifest, index) = {
        let app = app.clone();
        let graph_id = input.graph_id.clone();
        let graph_dir = graph_dir.clone();
        let artifact_id = artifact_id.clone();
        let filename = filename.clone();
        let stored_name = stored_name.clone();
        let mime_type = mime_type.clone();
        let pending_path = input.pending_path.clone();
        let size_bytes = input.size_bytes;
        let known_hash = input.sha256.clone();
        tokio::task::spawn_blocking(move || -> Result<_, String> {
            let sha256 = match known_hash {
                Some(hash) => hash,
                None => sha256_of_file(&pending_path)?,
            };
            let original_dir = artifact_original_dir(&graph_dir, &artifact_id)?;
            let stored_path = original_dir.join(&stored_name);
            let manifest = {
                let _lease = crate::crdt_engine::persistence_coordinator::
                    acquire_hot_write_blocking_if_managed(&app, &graph_id)?;
                let _flush_guard = crate::cell_durability::write_guard();
                create_dir_all(&original_dir)?;
                move_into_store(&pending_path, &stored_path)?;
                write_original_manifest_hashed(
                    &original_dir,
                    &stored_path,
                    stored_name.clone(),
                    &mime_type,
                    size_bytes,
                    Some(sha256),
                )?
            };
            let index = build_light_index(&stored_path, &filename, &mime_type, size_bytes);
            Ok((manifest, index))
        })
        .await
        .map_err(|error| format!("attach worker failed: {error}"))??
    };
    let sha256 = manifest.sha256.clone().unwrap_or_default();
    let hashed_at = timestamp();
    let label = input
        .label
        .clone()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| input.filename.clone());

    let payload = json!({
        "artifactId": artifact_id,
        "label": label,
        "originalFilename": filename,
        "mimeType": mime_type,
        "sizeBytes": input.size_bytes,
        "fileType": artifact_file_type(&filename),
        "status": "ready",
        "parentId": input.parent_id,
        "storageKey": format!("local://artifacts/{artifact_id}/original/{stored_name}"),
        "mode": "attached",
        "storage": crate::crdt_engine::workspace_ops::ARTIFACT_STORAGE_OWNED,
        "sha256": sha256,
        "hashedAt": hashed_at,
        "source": { "kind": "upload" },
        "index": index,
    });
    let record = match enqueue_crdt_operation(
        app.clone(),
        EnqueueCrdtOperationInput {
            kind: "workspace.putArtifact".to_string(),
            graph_id: input.graph_id.clone(),
            document_id: Some(artifact_id.clone()),
            payload,
        },
    )
    .await
    {
        Ok(record) => record,
        Err(error) => {
            // A refused put leaves no record, so its bytes go too. A put that
            // is still queued (a timeout, a closed reply channel) is journaled
            // and will land; its bytes must stay (angel review F2).
            if put_failed_for_good(&error) {
                let _ = fs::remove_dir_all(graph_dir.join("artifacts").join(&artifact_id));
            }
            return Err(error);
        }
    };
    flush_graph_projection(app, &input.graph_id).await?;

    Ok(json!({
        "success": true,
        "graphId": input.graph_id,
        "graph_id": input.graph_id,
        "artifactId": artifact_id,
        "artifact_id": artifact_id,
        "mode": "attached",
        "sha256": sha256,
        "sizeBytes": input.size_bytes,
        "mimeType": mime_type,
        "fileType": artifact_file_type(&filename),
        "index": record.get("index").cloned().unwrap_or(Value::Null),
        "artifact": record,
        "autoIngested": false,
        "auto_ingested": false,
    }))
}

/// True when the queue refused or failed the operation, false when it may
/// still apply it (timed out while durably queued, reply channel closed).
pub(crate) fn put_failed_for_good(error: &str) -> bool {
    !crate::crdt_queue::operation_may_still_run(error)
}

/// Rename when the store and the pending directory share a filesystem (they
/// do: both are under the graph), copy and remove otherwise.
fn move_into_store(pending_path: &Path, stored_path: &Path) -> Result<(), String> {
    if fs::rename(pending_path, stored_path).is_ok() {
        return Ok(());
    }
    copy_file_atomic(pending_path, stored_path).map_err(|error| {
        format!(
            "copy {} into the artifact store {}: {error}",
            display_path(pending_path),
            display_path(stored_path)
        )
    })?;
    remove_file_if_exists(pending_path)
        .map_err(|error| format!("remove pending upload after copy: {error}"))?;
    Ok(())
}

/// The light index: what the graph remembers about a body it does not read.
/// Format-specific and cheap by design; `toc` is filled only where a listing
/// costs nothing (zip members, the conversation titles of a chat export).
pub(crate) fn build_light_index(
    path: &Path,
    filename: &str,
    mime_type: &str,
    size_bytes: usize,
) -> Value {
    match fs::File::open(path) {
        Ok(mut file) => build_light_index_from_file(&mut file, filename, mime_type, size_bytes),
        Err(_) => plain_light_index(filename, mime_type, size_bytes),
    }
}

pub(crate) fn plain_light_index(filename: &str, mime_type: &str, size_bytes: usize) -> Value {
    let kind = kind_from_mime(mime_type, Some(&artifact_file_type(filename)));
    let title = Path::new(filename)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .unwrap_or(filename)
        .to_string();
    json!({
        "version": 1,
        "title": title,
        "kind": kind,
        "sizeBytes": size_bytes,
    })
}

/// The light index read through a descriptor the caller already holds, so
/// a referenced file is never opened a second time by name (angel review
/// of step 2: the second open by path is outside the no-follow walk).
pub(crate) fn build_light_index_from_file(
    file: &mut fs::File,
    filename: &str,
    mime_type: &str,
    size_bytes: usize,
) -> Value {
    use std::io::{Seek, SeekFrom};
    let mut index = plain_light_index(filename, mime_type, size_bytes);
    let lower = filename.to_ascii_lowercase();
    let _ = file.seek(SeekFrom::Start(0));
    let extra = if lower.ends_with(".zip") || mime_type.eq_ignore_ascii_case("application/zip") {
        file.try_clone().ok().and_then(zip_index_from)
    } else if lower.ends_with(".json") || mime_type.eq_ignore_ascii_case("application/json") {
        json_titles_from_reader(file, size_bytes as u64)
    } else {
        None
    };
    let _ = file.seek(SeekFrom::Start(0));
    if let (Some(object), Some(Value::Object(extra))) = (index.as_object_mut(), extra) {
        for (key, value) in extra {
            object.insert(key, value);
        }
    }
    index
}


/// A label as the index keeps it: control characters out, cut at the cap.
pub(crate) fn index_label(raw: &str) -> String {
    raw.chars()
        .filter(|c| !c.is_control())
        .take(INDEX_LABEL_CAP)
        .collect()
}

fn toc_value(labels: Vec<String>) -> Value {
    Value::Array(
        labels
            .into_iter()
            .map(|label| json!({ "label": index_label(&label) }))
            .collect(),
    )
}

/// Member names (capped) and, for a chat export, the conversation titles
/// read from `conversations.json` when it is small enough to parse.
fn zip_index_from(file: fs::File) -> Option<Value> {
    let mut archive = zip::ZipArchive::new(file).ok()?;
    let total = archive.len();
    if total > INDEX_ZIP_ENTRY_CAP {
        return Some(json!({ "members": total, "note": "too many entries to list" }));
    }
    let mut members = Vec::new();
    let mut conversations_member: Option<usize> = None;
    for index in 0..total {
        let entry = match archive.by_index(index) {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let name = entry.name().to_string();
        let is_conversations = Path::new(&name)
            .file_name()
            .and_then(|value| value.to_str())
            .map(|value| value.eq_ignore_ascii_case("conversations.json"))
            .unwrap_or(false);
        if conversations_member.is_none()
            && is_conversations
            && entry.size() <= INDEX_JSON_PARSE_CAP
        {
            conversations_member = Some(index);
        }
        if members.len() < INDEX_TOC_CAP {
            members.push(name);
        }
    }
    let mut extra = json!({ "members": total, "toc": toc_value(members) });
    if let Some(index) = conversations_member {
        if let Ok(entry) = archive.by_index(index) {
            // The declared size is the archive's word, not ours: read at most
            // the cap plus one byte, and drop an entry that lied (F1).
            let mut bytes = Vec::new();
            let read = entry.take(INDEX_JSON_PARSE_CAP + 1).read_to_end(&mut bytes);
            if read.is_ok() && bytes.len() as u64 <= INDEX_JSON_PARSE_CAP {
                if let Some((count, titles)) = conversation_titles(&bytes) {
                    extra["conversations"] = json!(count);
                    extra["toc"] = toc_value(titles);
                }
            }
        }
    }
    Some(extra)
}

fn json_titles_from_reader(file: &mut fs::File, length: u64) -> Option<Value> {
    if length > INDEX_JSON_PARSE_CAP {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(INDEX_JSON_PARSE_CAP + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > INDEX_JSON_PARSE_CAP {
        return None;
    }
    let (count, titles) = conversation_titles(&bytes)?;
    Some(json!({ "conversations": count, "toc": toc_value(titles) }))
}

/// A chat export is an array of objects with a `title` (ChatGPT) or a `name`
/// (Claude). Returns the count and the first titles, capped.
pub(crate) fn conversation_titles(bytes: &[u8]) -> Option<(usize, Vec<String>)> {
    let value: Value = serde_json::from_slice(bytes).ok()?;
    let items = value.as_array()?;
    if items.is_empty() || !items.iter().all(Value::is_object) {
        return None;
    }
    let titles: Vec<String> = items
        .iter()
        .take(INDEX_TOC_CAP)
        .map(|item| {
            item.get("title")
                .or_else(|| item.get("name"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|title| !title.is_empty())
                .unwrap_or("(untitled)")
                .to_string()
        })
        .collect();
    Some((items.len(), titles))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn modes_parse_by_name_and_default_by_parser() {
        assert_eq!(EntryMode::parse("attach").unwrap(), EntryMode::Attach);
        assert_eq!(EntryMode::parse(" Digest ").unwrap(), EntryMode::Digest);
        assert!(EntryMode::parse("reference").is_err());
        assert_eq!(
            EntryMode::default_for_node("notes.md", "application/octet-stream", false),
            EntryMode::Digest
        );
        assert_eq!(
            EntryMode::default_for_node("export.zip", "application/zip", false),
            EntryMode::Attach
        );
        // A PDF through the upload route has no parser without a pool; it
        // attaches, and ingest_artifact digests it through the local engines.
        // A pool-configured node keeps digesting it, as before (F7).
        assert_eq!(
            EntryMode::default_for_node("paper.pdf", "application/pdf", false),
            EntryMode::Attach
        );
        assert_eq!(
            EntryMode::default_for_node("paper.pdf", "application/pdf", true),
            EntryMode::Digest
        );
        assert!(put_failed_for_good(
            "workspace.putArtifact: mode must be one of"
        ));
        assert!(!put_failed_for_good(
            crate::crdt_queue::CRDT_OPERATION_STILL_QUEUED_ERROR
        ));
        assert!(!put_failed_for_good(crate::crdt_queue::CRDT_ADMISSION_UNCERTAIN));
        assert!(!put_failed_for_good("CRDT operation response channel closed"));
        assert_eq!(index_label("a\u{0}b\nc"), "abc");
        assert_eq!(index_label(&"x".repeat(500)).len(), INDEX_LABEL_CAP);
    }

    #[test]
    fn a_chat_export_zip_is_indexed_by_members_and_titles() {
        let dir = std::env::temp_dir().join(format!("sophia-attach-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("export.zip");
        {
            let file = fs::File::create(&path).unwrap();
            let mut writer = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default();
            writer.start_file("conversations.json", options).unwrap();
            writer
                .write_all(br#"[{"title":"On wires"},{"name":"Second"},{"title":"  "}]"#)
                .unwrap();
            writer.start_file("user.json", options).unwrap();
            writer.write_all(b"{}").unwrap();
            writer.finish().unwrap();
        }
        let size = fs::metadata(&path).unwrap().len() as usize;
        let index = build_light_index(&path, "export.zip", "application/zip", size);
        assert_eq!(index["version"], 1);
        assert_eq!(index["title"], "export");
        assert_eq!(index["kind"], "binary");
        assert_eq!(index["members"], 2);
        assert_eq!(index["conversations"], 3);
        let labels: Vec<&str> = index["toc"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["label"].as_str().unwrap())
            .collect();
        assert_eq!(labels, vec!["On wires", "Second", "(untitled)"]);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_zip_that_lies_about_a_member_size_is_not_read_past_the_cap() {
        let dir = std::env::temp_dir().join(format!("sophia-attach-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bomb.zip");
        {
            let file = fs::File::create(&path).unwrap();
            let mut writer = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            writer.start_file("conversations.json", options).unwrap();
            // Well past the cap once inflated; deflate makes it tiny on disk.
            let zeros = vec![b' '; 1024 * 1024];
            for _ in 0..(INDEX_JSON_PARSE_CAP / (1024 * 1024) + 2) {
                writer.write_all(&zeros).unwrap();
            }
            writer.finish().unwrap();
        }
        let size = fs::metadata(&path).unwrap().len() as usize;
        let index = build_light_index(&path, "bomb.zip", "application/zip", size);
        assert_eq!(index["members"], 1);
        assert!(index.get("conversations").is_none(), "{index}");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_unknown_binary_gets_only_the_plain_index() {
        let dir = std::env::temp_dir().join(format!("sophia-attach-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("blob.bin");
        fs::write(&path, b"\x00\x01\x02").unwrap();
        let index = build_light_index(&path, "blob.bin", "application/octet-stream", 3);
        assert_eq!(index["kind"], "binary");
        assert!(index.get("toc").is_none());
        assert!(index.get("members").is_none());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn conversation_titles_refuse_what_is_not_a_list_of_objects() {
        assert!(conversation_titles(b"{\"title\":\"x\"}").is_none());
        assert!(conversation_titles(b"[1,2]").is_none());
        assert!(conversation_titles(b"[]").is_none());
        let (count, titles) = conversation_titles(b"[{\"name\":\"a\"}]").unwrap();
        assert_eq!((count, titles), (1, vec!["a".to_string()]));
    }
}
