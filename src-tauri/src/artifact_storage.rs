//! Whose bytes an artifact record keeps, and the record a dropped file gets.
//!
//! "Artifacts — a body outside the house", corrections of 2026-10-23: a
//! record exists where bytes are kept. An attached body OWNS its bytes under
//! `artifacts/{id}/original/`. A file digested at the door (a dropped PDF, an
//! uploaded docx, a markdown drop) keeps its bytes under its document,
//! `documents/{doc}/original/`, and its record BORROWS them: the record is
//! `original-{documentId}`, mode digested, and it dies with the document, not
//! the other way round. Zeta's reader and the document original store do not
//! move; `read_artifact` answers for both kinds through
//! [`resolve_artifact_original_dir`].
//!
//! Documents that kept an original before this run have no record. The
//! navigation list backfills them lazily ([`backfill_borrowed_records`]):
//! idempotent, one hash per legacy file, written into its manifest so the
//! next read is free.

use crate::app_runtime::AppHandle;
use crate::{
    artifact_attach::build_light_index,
    clock::timestamp,
    crdt_engine::workspace_ops::{
        artifact_file_type, ARTIFACT_MODE_DIGESTED, ARTIFACT_STORAGE_BORROWED,
    },
    crdt_projection_flush::flush_graph_projection,
    crdt_queue::{enqueue_crdt_operation, EnqueueCrdtOperationInput},
    document_paths::document_dir,
    document_service::read_workspace_record,
    json_utils::{json_number, json_string},
    original_file_manifest_store::{
        original_manifest_file_path, read_original_manifest, write_original_manifest_hashed,
    },
    original_file_types::OriginalFileManifest,
    paths::{artifact_original_dir, existing_graph_dir, sha256_of_file},
    rdf_service::snapshot_array,
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// The scheme a document's own original carries in `sf_storageKey`.
pub(crate) const DOCUMENT_STORAGE_KEY_PREFIX: &str = "local://documents/";

/// The record a digested-at-the-door document gets: one per document,
/// deterministic, so a replayed ingest upserts instead of doubling and a
/// delete needs no scan.
pub(crate) fn borrowed_artifact_id(document_id: &str) -> String {
    format!("original-{document_id}")
}

/// True when the document's original lives under its own directory (the
/// door's store), as opposed to an artifact's.
pub(crate) fn storage_key_is_document_original(storage_key: &str) -> bool {
    storage_key.starts_with(DOCUMENT_STORAGE_KEY_PREFIX)
}

/// The `workspace.putArtifact` payload for a borrowed record. `order` and
/// `updated_at_ms` are the document's, so the record sits beside it.
pub(crate) fn borrowed_record_payload(
    document_id: &str,
    parent_id: Option<&str>,
    order: f64,
    updated_at_ms: f64,
    storage_key: &str,
    manifest: &OriginalFileManifest,
    original_path: &Path,
) -> Value {
    let index = build_light_index(
        original_path,
        &manifest.filename,
        &manifest.mime_type,
        manifest.size_bytes,
    );
    let mut payload = json!({
        "artifactId": borrowed_artifact_id(document_id),
        "label": manifest.filename,
        "originalFilename": manifest.filename,
        "mimeType": manifest.mime_type,
        "sizeBytes": manifest.size_bytes,
        "fileType": artifact_file_type(&manifest.filename),
        "status": "ready",
        "parentId": parent_id,
        "order": order,
        "updatedAt": updated_at_ms,
        "storageKey": storage_key,
        "mode": ARTIFACT_MODE_DIGESTED,
        "storage": ARTIFACT_STORAGE_BORROWED,
        "ingestedDocId": document_id,
        "source": { "kind": "upload" },
        "index": index,
    });
    if let (Some(object), Some(hash)) = (payload.as_object_mut(), manifest.sha256.as_deref()) {
        object.insert("sha256".into(), json!(hash));
        object.insert("hashedAt".into(), json!(manifest.updated_at));
    }
    payload
}

/// The artifact's record as the workspace snapshot has it, or None.
pub(crate) fn artifact_record(
    app: &AppHandle,
    graph_id: &str,
    artifact_id: &str,
) -> Result<Option<Value>, String> {
    let graph_dir = existing_graph_dir(app, graph_id)?;
    let workspace = read_workspace_record(&graph_dir, graph_id)?;
    Ok(workspace.snapshot.as_ref().and_then(|snapshot| {
        snapshot_array(snapshot, "artifacts")
            .iter()
            .find(|artifact| json_string(artifact.get("id")).as_deref() == Some(artifact_id))
            .cloned()
    }))
}

pub(crate) fn record_is_borrowed(record: &Value) -> bool {
    json_string(record.get("storage")).as_deref() == Some(ARTIFACT_STORAGE_BORROWED)
}

/// Where the artifact's bytes are: its own store, or, for a borrowed record,
/// the original store of the document it was digested into. An owned
/// directory wins when it exists, so a record that lies about borrowing
/// cannot redirect a read; a borrowed record whose document is gone reads
/// as missing, which is the truth.
pub(crate) fn resolve_artifact_original_dir(
    app: &AppHandle,
    graph_id: &str,
    artifact_id: &str,
) -> Result<PathBuf, String> {
    let graph_dir = existing_graph_dir(app, graph_id)?;
    // The record is trusted because the put enforces the borrow link: a
    // borrowed record is `original-{doc}` and names a document that was in
    // the workspace when it was put. A stray `artifacts/original-…`
    // directory never redirects it.
    if let Some(record) = artifact_record(app, graph_id, artifact_id)? {
        if crate::artifact_reference::record_is_referenced(&record) {
            return Err(format!(
                "artifact {artifact_id} is a reference; its bytes are outside the house (read_artifact or the download read them in place)"
            ));
        }
    }
    if artifact_id.starts_with(crate::crdt_engine::workspace_ops::BORROWED_ARTIFACT_ID_PREFIX) {
        let record = artifact_record(app, graph_id, artifact_id)?
            .ok_or_else(|| format!("artifact not found: {artifact_id}"))?;
        if !record_is_borrowed(&record) {
            return Err(format!("artifact {artifact_id} is not a borrowed record"));
        }
        let document_id = json_string(record.get("ingestedDocId"))
            .ok_or_else(|| format!("artifact {artifact_id} borrows an original but names no document"))?;
        let dir = document_dir(&graph_dir, &document_id)?.join("original");
        if dir.join("manifest.json").is_file() {
            return Ok(dir);
        }
        return Err(format!(
            "artifact {artifact_id} borrows the original of document {document_id}, which has none"
        ));
    }
    artifact_original_dir(&graph_dir, artifact_id)
}

/// Originals above this size are recorded without a hash by the backfill
/// (the hash is one read of the file inside a request); the door hashes
/// everything it admits, so only legacy files meet this cap.
pub(crate) const BACKFILL_HASH_CAP_BYTES: u64 = 512 * 1024 * 1024;

fn backfill_done_once(graph_id: &str) -> bool {
    static DONE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    let set = DONE.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()));
    let mut guard = set.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    !guard.insert(graph_id.to_string())
}


/// Refuse an edit of bytes the record does not own: borrowed (a document's
/// original) or external (a reference, outside the house).
pub(crate) fn refuse_if_borrowed(
    app: &AppHandle,
    graph_id: &str,
    artifact_id: &str,
) -> Result<(), String> {
    match artifact_record(app, graph_id, artifact_id)? {
        Some(record) if record_is_borrowed(&record) => Err(format!(
            "artifact {artifact_id} borrows its bytes from a document's original; revise the document, not the record"
        )),
        Some(record) if crate::artifact_reference::record_is_referenced(&record) => Err(format!(
            "artifact {artifact_id} is a reference; its bytes are outside the house and are not edited or digested through the record"
        )),
        _ => Ok(()),
    }
}

/// After an owned record's bytes were rewritten (a revision, a restore, an
/// image edit): the record's hash, size and index follow the store, so
/// `read_artifact` never reports a stale record of our own making.
pub(crate) async fn refresh_owned_record_after_write(
    app: AppHandle,
    graph_id: &str,
    artifact_id: &str,
) -> Result<Value, String> {
    let graph_dir = existing_graph_dir(&app, graph_id)?;
    let original_dir = artifact_original_dir(&graph_dir, artifact_id)?;
    let manifest = read_original_manifest(&original_dir)?;
    let original_path = original_manifest_file_path(&original_dir, &manifest.filename)?;
    let manifest = match manifest.sha256 {
        Some(_) => manifest,
        None => {
            let hash = sha256_of_file(&original_path)?;
            let _flush_guard = crate::cell_durability::write_guard();
            write_original_manifest_hashed(
                &original_dir,
                &original_path,
                manifest.filename.clone(),
                &manifest.mime_type,
                manifest.size_bytes,
                Some(hash),
            )?
        }
    };
    let index = build_light_index(
        &original_path,
        &manifest.filename,
        &manifest.mime_type,
        manifest.size_bytes,
    );
    let record = enqueue_crdt_operation(
        app.clone(),
        EnqueueCrdtOperationInput {
            kind: "workspace.putArtifact".to_string(),
            graph_id: graph_id.to_string(),
            document_id: Some(artifact_id.to_string()),
            payload: json!({
                "artifactId": artifact_id,
                "originalFilename": manifest.filename,
                "mimeType": manifest.mime_type,
                "sizeBytes": manifest.size_bytes,
                "sha256": manifest.sha256,
                "hashedAt": timestamp(),
                "index": index,
            }),
        },
    )
    .await?;
    flush_graph_projection(app, graph_id).await?;
    Ok(record)
}

/// The Files list (Delta's settings page, 2026-10-26): every record as the
/// navigation list has it, plus how many wires point at it and which
/// documents do (the wires' sources and the digested document).
pub(crate) fn artifact_list_with_pointers(
    app: &AppHandle,
    graph_id: &str,
) -> Result<Vec<Value>, String> {
    let (_, _, artifacts) = crate::hosted_navigation_projection::hosted_navigation_parts(app, graph_id)?;
    let graph_dir = existing_graph_dir(app, graph_id)?;
    let workspace = read_workspace_record(&graph_dir, graph_id)?;
    let mut wires_by_target: std::collections::HashMap<String, (usize, std::collections::BTreeSet<String>)> =
        std::collections::HashMap::new();
    if let Some(snapshot) = workspace.snapshot.as_ref() {
        for wire in snapshot_array(snapshot, "wires") {
            let Some(target) = json_string(wire.get("targetArtifactId"))
                .or_else(|| json_string(wire.get("target_artifact_id")))
            else {
                continue;
            };
            let entry = wires_by_target.entry(target).or_default();
            entry.0 += 1;
            if let Some(source) = json_string(wire.get("sourceDocumentId"))
                .or_else(|| json_string(wire.get("source_document_id")))
            {
                entry.1.insert(source);
            }
        }
    }
    Ok(artifacts
        .into_iter()
        .map(|mut artifact| {
            let id = json_string(artifact.get("id")).unwrap_or_default();
            let (wire_count, mut documents) = wires_by_target.remove(&id).unwrap_or_default();
            if let Some(ingested) = json_string(artifact.get("ingestedDocId")) {
                documents.insert(ingested);
            }
            if let Some(object) = artifact.as_object_mut() {
                object.insert("wireCount".into(), json!(wire_count));
                object.insert("documentIds".into(), json!(documents.into_iter().collect::<Vec<_>>()));
            }
            artifact
        })
        .collect())
}

/// One record's standing now: for a reference, re-authorized and re-opened
/// (`ok | stale | missing | denied`); for kept bytes, the manifest beside
/// the record (`ok | stale | missing`). Lazy per row; the list stays cheap.
pub(crate) fn artifact_status(app: &AppHandle, graph_id: &str, artifact_id: &str) -> Result<Value, String> {
    let record = artifact_record(app, graph_id, artifact_id)?
        .ok_or_else(|| format!("artifact not found: {artifact_id}"))?;
    let mode = json_string(record.get("mode"));
    let storage = json_string(record.get("storage"));
    let recorded = json_string(record.get("sha256"));
    if crate::artifact_reference::record_is_referenced(&record) {
        let read = crate::artifact_reference::read_referenced(app, graph_id, &record, 0);
        return Ok(json!({
            "artifactId": artifact_id,
            "mode": mode,
            "storage": storage,
            "state": read.state.name(),
            "sha256": recorded,
            "storedSha256": read.sha256_now,
            "sizeBytes": read.size_bytes,
            "path": read.path,
            "note": read.note,
        }));
    }
    let (state, stored, size, note) = match resolve_artifact_original_dir(app, graph_id, artifact_id)
        .and_then(|dir| read_original_manifest(&dir))
    {
        Ok(manifest) => {
            let stale = matches!((recorded.as_deref(), manifest.sha256.as_deref()), (Some(a), Some(b)) if a != b);
            (
                if stale { "stale" } else { "ok" },
                manifest.sha256.clone(),
                Some(manifest.size_bytes as u64),
                None,
            )
        }
        Err(error) => ("missing", None, None, Some(error)),
    };
    Ok(json!({
        "artifactId": artifact_id,
        "mode": mode,
        "storage": storage,
        "state": state,
        "sha256": recorded,
        "storedSha256": stored,
        "sizeBytes": size,
        "note": note,
    }))
}

/// Make the records the door did not make before 2026-10-23: one borrowed
/// record for every document whose original is kept under the document and
/// that no record claims yet. Returns how many were put. Idempotent; the
/// scan is the workspace snapshot, the only file work is one hash per
/// legacy original, written back into its manifest.
pub(crate) async fn backfill_borrowed_records(
    app: AppHandle,
    graph_id: &str,
) -> Result<usize, String> {
    // Once per graph per process: the list is read often, the legacy set
    // changes only when this code changes, and two concurrent lists must
    // not hash the same files twice.
    if backfill_done_once(graph_id) {
        return Ok(0);
    }
    backfill_borrowed_records_uncached(app, graph_id).await
}

pub(crate) async fn backfill_borrowed_records_uncached(
    app: AppHandle,
    graph_id: &str,
) -> Result<usize, String> {
    let graph_dir = existing_graph_dir(&app, graph_id)?;
    let workspace = read_workspace_record(&graph_dir, graph_id)?;
    let Some(snapshot) = workspace.snapshot.as_ref() else {
        return Ok(0);
    };
    let claimed: std::collections::HashSet<String> = snapshot_array(snapshot, "artifacts")
        .iter()
        .filter_map(|artifact| json_string(artifact.get("ingestedDocId")))
        .collect();
    let mut candidates: Vec<(String, Option<String>, f64, f64, String)> = Vec::new();
    for document in snapshot_array(snapshot, "documents") {
        let Some(document_id) = json_string(document.get("id")) else {
            continue;
        };
        if claimed.contains(&document_id) {
            continue;
        }
        let storage_key = document
            .get("sourceFile")
            .and_then(|source| {
                json_string(source.get("sf_storageKey")).or_else(|| json_string(source.get("storageKey")))
            })
            .unwrap_or_default();
        if !storage_key_is_document_original(&storage_key) {
            continue;
        }
        let parent_id = json_string(document.get("parentId"));
        let order = json_number(document.get("order")).unwrap_or(0.0);
        let updated_at = json_number(document.get("updatedAt")).unwrap_or(order);
        candidates.push((document_id, parent_id, order, updated_at, storage_key));
    }
    if candidates.is_empty() {
        return Ok(0);
    }
    let mut put = 0usize;
    for (document_id, parent_id, order, updated_at, storage_key) in candidates {
        let Ok(original_dir) = document_dir(&graph_dir, &document_id).map(|d| d.join("original")) else {
            continue;
        };
        let Ok(manifest) = read_original_manifest(&original_dir) else {
            continue;
        };
        let Ok(original_path) = original_manifest_file_path(&original_dir, &manifest.filename) else {
            continue;
        };
        if !original_path.is_file() {
            continue;
        }
        // One legacy file's trouble never stops the others (angel review).
        let manifest = match manifest.sha256 {
            Some(_) => manifest,
            None => {
                let too_big = std::fs::metadata(&original_path)
                    .map(|meta| meta.len() > BACKFILL_HASH_CAP_BYTES)
                    .unwrap_or(true);
                if too_big {
                    manifest
                } else {
                    match sha256_of_file(&original_path).and_then(|hash| {
                        let _flush_guard = crate::cell_durability::write_guard();
                        write_original_manifest_hashed(
                            &original_dir,
                            &original_path,
                            manifest.filename.clone(),
                            &manifest.mime_type,
                            manifest.size_bytes,
                            Some(hash),
                        )
                    }) {
                        Ok(hashed) => hashed,
                        Err(error) => {
                            log::warn!("artifacts: backfill could not hash {document_id}: {error}");
                            continue;
                        }
                    }
                }
            }
        };
        let payload = borrowed_record_payload(
            &document_id,
            parent_id.as_deref(),
            order,
            if updated_at > 0.0 { updated_at } else { timestamp().parse().unwrap_or(0.0) },
            &storage_key,
            &manifest,
            &original_path,
        );
        // A refused put (the document went away while we hashed) is the
        // invariant doing its work, not a reason to stop.
        match enqueue_crdt_operation(
            app.clone(),
            EnqueueCrdtOperationInput {
                kind: "workspace.putArtifact".to_string(),
                graph_id: graph_id.to_string(),
                document_id: Some(borrowed_artifact_id(&document_id)),
                payload,
            },
        )
        .await
        {
            Ok(_) => put += 1,
            Err(error) => {
                log::warn!("artifacts: backfill put for {document_id} refused: {error}");
            }
        }
    }
    if put > 0 {
        flush_graph_projection(app, graph_id).await?;
    }
    Ok(put)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::original_file_service::{read_artifact_original_file, read_document_original_file};

    #[test]
    fn a_borrowed_payload_names_the_document_and_carries_its_hash() {
        let dir = std::env::temp_dir().join(format!("garden-borrowed-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("notes.md");
        std::fs::write(&path, b"# Notes").unwrap();
        let manifest = OriginalFileManifest {
            filename: "notes.md".into(),
            mime_type: "text/markdown".into(),
            size_bytes: 7,
            local_path: path.display().to_string(),
            created_at: "1".into(),
            updated_at: "2".into(),
            sha256: Some("ab".repeat(32)),
        };
        let payload = borrowed_record_payload(
            "doc-1",
            Some("f-1"),
            5.0,
            6.0,
            "local://documents/doc-1/original/notes.md",
            &manifest,
            &path,
        );
        assert_eq!(payload["artifactId"], "original-doc-1");
        assert_eq!(payload["storage"], "borrowed");
        assert_eq!(payload["mode"], "digested");
        assert_eq!(payload["ingestedDocId"], "doc-1");
        assert_eq!(payload["sha256"], "ab".repeat(32));
        assert_eq!(payload["hashedAt"], "2");
        assert_eq!(payload["index"]["kind"], "text");
        assert_eq!(payload["index"]["title"], "notes");
        assert_eq!(payload["fileType"], "md");
        assert!(storage_key_is_document_original("local://documents/x/original/a.pdf"));
        assert!(!storage_key_is_document_original("local://artifacts/x/original/a.pdf"));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The door end to end on a scratch profile: a markdown drop makes a
    /// document AND a borrowed record; the record's bytes read from the
    /// document's store; deleting the record spares the document's original
    /// and the backfill brings the record back; deleting the document takes
    /// the record and its wire.
    #[test]
    fn a_dropped_file_gets_a_borrowed_record_that_dies_with_its_document() {
        use crate::crdt_queue::{enqueue_crdt_operation, EnqueueCrdtOperationInput};
        use crate::graph_service::{create_graph_service, CreateGraphInput};
        use base64::Engine;

        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let profile = std::env::temp_dir().join(format!("garden-borrowed-e2e-{nanos}"));
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);
        let graph_id = "borrowed-src";
        create_graph_service(
            &app,
            CreateGraphInput {
                graph_id: Some(graph_id.to_string()),
                title: "Borrowed".to_string(),
                description: None,
                operation_id: None,
            },
        )
        .expect("create graph");
        let enqueue = |kind: &str, document_id: Option<&str>, payload: Value| {
            crate::app_runtime::async_runtime::block_on(enqueue_crdt_operation(
                app.clone(),
                EnqueueCrdtOperationInput {
                    kind: kind.to_string(),
                    graph_id: graph_id.to_string(),
                    document_id: document_id.map(str::to_string),
                    payload,
                },
            ))
            .expect(kind)
        };
        let bytes = b"# Notes\n\nA dropped file keeps its original.";
        let expected_hash = {
            use sha2::Digest;
            format!("{:x}", sha2::Sha256::digest(bytes))
        };
        enqueue(
            "document.ingestMarkdownOriginal",
            Some("d-drop"),
            json!({
                "documentId": "d-drop",
                "filename": "notes.md",
                "mimeType": "text/markdown",
                "markdown": String::from_utf8_lossy(bytes),
                "dataBase64": base64::engine::general_purpose::STANDARD.encode(bytes),
                "sourceFile": { "fileType": "md" },
            }),
        );
        let record = artifact_record(&app, graph_id, "original-d-drop")
            .expect("read record")
            .expect("the drop made a borrowed record");
        assert_eq!(record["storage"], "borrowed");
        assert_eq!(record["mode"], "digested");
        assert_eq!(record["ingestedDocId"], "d-drop");
        assert_eq!(record["sha256"], expected_hash);
        assert_eq!(record["index"]["kind"], "text");
        assert_eq!(
            record["sourceFile"]["sf_storageKey"],
            "local://documents/d-drop/original/notes.md"
        );
        let (manifest, read) =
            read_artifact_original_file(&app, graph_id, "original-d-drop").expect("borrowed bytes");
        assert_eq!(read, bytes);
        assert_eq!(manifest.sha256.as_deref(), Some(expected_hash.as_str()));
        assert!(
            !existing_graph_dir(&app, graph_id)
                .unwrap()
                .join("artifacts")
                .join("original-d-drop")
                .exists(),
            "a borrowed record owns no directory"
        );
        // A revision must not fork bytes the record does not own.
        assert!(refuse_if_borrowed(&app, graph_id, "original-d-drop").is_err());

        // Delete the record: the document's original is untouched, and the
        // next list brings the record back.
        enqueue("workspace.deleteArtifact", Some("original-d-drop"), json!({ "artifactId": "original-d-drop" }));
        assert!(artifact_record(&app, graph_id, "original-d-drop").unwrap().is_none());
        let (_, still) = read_document_original_file(&app, graph_id, "d-drop").expect("original stays");
        assert_eq!(still, bytes);
        let put = crate::app_runtime::async_runtime::block_on(backfill_borrowed_records_uncached(
            app.clone(),
            graph_id,
        ))
        .expect("backfill");
        assert_eq!(put, 1);
        let record = artifact_record(&app, graph_id, "original-d-drop")
            .unwrap()
            .expect("backfilled");
        assert_eq!(record["sha256"], expected_hash);
        let again = crate::app_runtime::async_runtime::block_on(backfill_borrowed_records_uncached(
            app.clone(),
            graph_id,
        ))
        .expect("backfill twice");
        assert_eq!(again, 0, "the backfill is idempotent");
        // Digesting a borrowed record again answers with its document and
        // makes nothing (angel review: it used to make a second document
        // pointing into the first one's store).
        let ingested = crate::app_runtime::async_runtime::block_on(
            crate::artifact_ingest_service::local_ingest_artifact(
                app.clone(),
                graph_id.to_string(),
                "original-d-drop".to_string(),
                "ingest".to_string(),
                None,
                None,
                true,
                true,
                None,
            ),
        )
        .expect("ingest on borrowed");
        assert_eq!(ingested["alreadyIngested"], true);
        assert_eq!(ingested["documentId"], "d-drop");
        let graph_dir = existing_graph_dir(&app, graph_id).unwrap();
        let workspace = read_workspace_record(&graph_dir, graph_id).unwrap();
        let (documents, artifacts) = workspace
            .snapshot
            .as_ref()
            .map(|s| (snapshot_array(s, "documents").len(), snapshot_array(s, "artifacts").len()))
            .unwrap_or((0, 0));
        assert_eq!((documents, artifacts), (1, 1));

        // A wire at the record; then the document goes, and takes both.
        enqueue(
            "document.write",
            Some("d-note"),
            json!({ "documentId": "d-note", "title": "Note", "format": "markdown",
                    "content": "# Note\n\nAbout the drop." }),
        );
        enqueue(
            "workspace.createWire",
            Some("d-note"),
            json!({ "wireId": "w-drop", "sourceDocumentId": "d-note",
                    "targetArtifactId": "original-d-drop", "predicate": "annotates" }),
        );
        let deleted = enqueue("workspace.deleteDocument", Some("d-drop"), json!({ "documentId": "d-drop" }));
        assert_eq!(deleted["deletedArtifactIds"][0], "original-d-drop");
        assert!(artifact_record(&app, graph_id, "original-d-drop").unwrap().is_none());
        let graph_dir = existing_graph_dir(&app, graph_id).unwrap();
        let workspace = read_workspace_record(&graph_dir, graph_id).unwrap();
        let wires = workspace
            .snapshot
            .as_ref()
            .map(|s| snapshot_array(s, "wires").len())
            .unwrap_or(0);
        assert_eq!(wires, 0, "the wire went with the record");
        let put = crate::app_runtime::async_runtime::block_on(backfill_borrowed_records_uncached(
            app.clone(),
            graph_id,
        ))
        .expect("backfill after delete");
        assert_eq!(put, 0, "a deleted document is not resurrected as a record");
        let _ = std::fs::remove_dir_all(profile);
    }
}
