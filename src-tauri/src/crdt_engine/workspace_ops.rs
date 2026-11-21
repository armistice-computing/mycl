//! Workspace structure operations (folders, documents, wires, artifacts)
//! for headless cells. Port of the workspace.* handlers in
//! frontend/src/native/native-local-runtime.ts and the workspace Y.Doc
//! materialization in frontend/src/native/workspace-materialization.ts.
//!
//! Layering:
//! - doc-level mutators (`write_workspace_document`, `update_workspace_folder`,
//!   ...) operate on a `yrs::TransactionMut` and mirror the TS handlers'
//!   Y.Map writes, error messages, and return shapes exactly;
//! - `materialize_workspace_snapshot_json` ports materializeWorkspaceYDoc
//!   (the workspace.json snapshot shape the frontend writes);
//! - `apply` orchestrates: resolve graph dir → mutate the hosted room doc
//!   (broadcasts to y-websocket clients + persists update-v1.bin) → run the
//!   same persistence the `save_workspace` Tauri command performs (snapshot
//!   JSON + RDF materialization + graph touch).

use crate::app_runtime::AppHandle;
use crate::crdt_engine::rooms::{Room, RoomRegistry};
use crate::crdt_queue::CrdtOperation;
use serde_json::{json, Map as JsonMap, Value};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
#[cfg(feature = "desktop")]
use tauri::Manager;
use yrs::{
    Any, Doc, Map as YMap, MapPrelim, MapRef, Out, ReadTxn, Transact, TransactionMut, WriteTxn,
};

use super::executor::{ApplyOperationError, ApplyOperationResult};

// ─────────────────────────────────────────────────────────────────────────────
// Entry points
// ─────────────────────────────────────────────────────────────────────────────

pub(crate) async fn apply(app: &AppHandle, operation: &CrdtOperation) -> Result<Value, String> {
    apply_classified(app, operation)
        .await
        .map_err(ApplyOperationError::into_message)
}

pub(crate) async fn apply_classified(
    app: &AppHandle,
    operation: &CrdtOperation,
) -> ApplyOperationResult<Value> {
    let mut hot_committed = false;
    let outcome: Result<Value, String> = async {
        let graph_id = operation.graph_id.clone();
        let graph_dir = crate::graph_paths::existing_graph_dir(app, &graph_id)?;
        let room = workspace_room(app, &graph_id, &graph_dir).await?;
        let payload = &operation.payload;
        let recovered = payload
            .get(crate::crdt_queue::RECOVERED_OPERATION_PAYLOAD_KEY)
            .and_then(Value::as_bool)
            .unwrap_or(false);

        macro_rules! commit_hot {
            ($future:expr) => {{
                let value = $future.await?;
                hot_committed = true;
                value
            }};
        }

        let result = match operation.kind.as_str() {
        "workspace.createDocument" => {
            // The executor already refuses a derived Geist view id before
            // this point (require_editable, terminal).
            refuse_uncreatable_document(&graph_dir, payload)?;
            let value = commit_hot!(update_workspace_doc_checked(&room, |_doc, txn| {
                write_workspace_document(txn, payload)
            }));
            let document_id = value
                .get("documentId")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    "workspace.createDocument result is missing documentId".to_string()
                })?
                .to_string();
            let title = value
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| string_payload(payload, "title"))
                .unwrap_or_else(|| "Untitled".to_string());
            // The workspace entry and the document body are one resumable
            // lifecycle operation. We deliberately provision after the hot
            // Y.Doc commit: a crash here leaves a genuine workspace ghost that
            // recovery replays under the same operation ID, and
            // create_document_with_lease idempotently completes the missing
            // manifest/persistence tail. A body is never inferred from an
            // arbitrary WebSocket map edit; only this explicit queued command
            // crosses the namespace/body boundary.
            crate::document_service::create_document_with_lease(
                app.clone(),
                crate::document_types::CreateDocumentInput {
                    graph_id: graph_id.clone(),
                    document_id: Some(document_id),
                    title,
                },
            )?;
            value
        }
        "workspace.updateDocument" => {
            let document_id = operation
                .document_id
                .clone()
                .or_else(|| string_payload(payload, "documentId"))
                .ok_or_else(|| "update document operation is missing documentId".to_string())?;
            commit_hot!(
                update_workspace_doc_checked(&room, |_doc, txn| {
                    update_workspace_document(txn, &document_id, payload)
                })
            )
        }
        "workspace.deleteDocument" => {
            let document_id = operation
                .document_id
                .clone()
                .or_else(|| string_payload(payload, "documentId"))
                .ok_or_else(|| "delete document operation is missing documentId".to_string())?;
            if crate::ids::validate_local_id(&document_id, "document_id").is_err() {
                // A client can write any key into the workspace map. A row
                // whose id is not a local id names no document and has no
                // body, so deleting it removes only the row. An id with no
                // row is refused before anything is touched: the body delete
                // after the commit would refuse it, and that refusal was
                // retried.
                let row_id = document_id.clone();
                let present = room
                    .with_doc(move |doc| document_exists_in_workspace(doc, &row_id))
                    .await;
                if !present && !recovered {
                    return Err(format!(
                        "document_id contains invalid characters, and no workspace row has the id {document_id:?}"
                    ));
                }
                if present {
                    commit_hot!(update_workspace_doc_checked(&room, |_doc, txn| {
                        txn.get_or_insert_map("documents").remove(txn, &document_id);
                        Ok(())
                    }));
                }
                json!({ "documentId": document_id, "deleted": true, "removedStrayRowIds": [document_id] })
            } else {
                let value = commit_hot!(update_workspace_doc_checked(&room, |_doc, txn| {
                    let documents = txn.get_or_insert_map("documents");
                    documents.remove(txn, &document_id);
                    let removed_artifacts = cascade_document_delete_to_artifacts(txn, &document_id);
                    Ok(json!({
                        "documentId": document_id,
                        "deleted": true,
                        "deletedArtifactIds": removed_artifacts,
                    }))
                }));
                crate::document_delete_service::delete_document_for_operation(
                    app.clone(),
                    graph_id.clone(),
                    document_id,
                    Some(&operation.operation_id),
                )?;
                value
            }
        }
        "workspace.createFolder" => {
            commit_hot!(update_workspace_doc_checked(&room, |_doc, txn| {
                write_workspace_folder(txn, payload)
            }))
        }
        "workspace.updateFolder" => {
            let folder_id = operation
                .document_id
                .clone()
                .or_else(|| string_payload(payload, "folderId"))
                .ok_or_else(|| "update folder operation is missing folderId".to_string())?;
            commit_hot!(
                update_workspace_doc_checked(&room, |_doc, txn| {
                    update_workspace_folder(txn, &folder_id, payload)
                })
            )
        }
        "workspace.deleteFolder" => {
            let folder_id = operation
                .document_id
                .clone()
                .or_else(|| string_payload(payload, "folderId"))
                .ok_or_else(|| "delete folder operation is missing folderId".to_string())?;
            let value = object_payload(payload);
            let cascade = boolean_value(value.get("cascade"), false);
            let hard = boolean_value(value.get("hard"), true);
            let recovered_folder_still_exists = if recovered && hard && cascade {
                let folder_id = folder_id.clone();
                room.with_doc(move |doc| folder_exists_in_workspace(doc, &folder_id))
                    .await
            } else {
                false
            };
            if hard && cascade && (!recovered || recovered_folder_still_exists) {
                // workspace.json is the recovery plan for a hard cascade once
                // the hot Y.Doc no longer contains its descendants. Capture
                // every prior hot-only child before the destructive mutation.
                // Recovery can begin before the original operation ever ran;
                // when the folder is still hot, refresh and validate the plan
                // so a second crash is recoverable too. When it is already
                // absent, never overwrite the preserved pre-delete evidence.
                persist_workspace(app, &graph_id, &graph_dir, &room, &operation.operation_id)
                    .await?;
                if recovered_folder_still_exists
                    && recovered_folder_delete_targets(&graph_dir, &graph_id, &folder_id)?.is_none()
                {
                    return Err(format!(
                        "recovered hard folder cascade snapshot does not contain {folder_id}"
                    ));
                }
            }
            let (folder_existed, mut child_documents, mut child_artifacts, mut deleted_folders) =
                commit_hot!(update_workspace_doc_checked(&room, |_doc, txn| {
                    delete_workspace_folder_in_doc(txn, &folder_id, cascade)
                }));
            if recovered && !folder_existed && hard && cascade {
                if let Some((documents, artifacts, folders)) =
                    recovered_folder_delete_targets(&graph_dir, &graph_id, &folder_id)?
                {
                    child_documents = documents;
                    child_artifacts = artifacts;
                    deleted_folders = folders;
                }
            }
            let (child_documents, stray_rows) = split_stray_rows(child_documents);
            if hard {
                for document_id in &child_documents {
                    crate::document_delete_service::delete_document_for_operation(
                        app.clone(),
                        graph_id.clone(),
                        document_id.clone(),
                        Some(&operation.operation_id),
                    )?;
                }
            }
            json!({
                "id": folder_id,
                "folderId": folder_id,
                "graphId": graph_id,
                "status": "deleted",
                "cascade": cascade,
                "hard": hard,
                "deletedDocumentIds": child_documents,
                "removedStrayRowIds": stray_rows,
                "deletedArtifactIds": child_artifacts,
                "deletedFolderIds": deleted_folders,
            })
        }
        "workspace.moveFolder" => {
            let value = object_payload(payload);
            let folder_id = string_value(pick(&value, &["folderId", "folder_id", "id"]))
                .ok_or_else(|| "move folder operation is missing folderId".to_string())?;
            let update_payload = json!({
                "newParentId": pick(&value, &["newParentId", "new_parent_id", "parentId", "parent_id"])
                    .cloned()
                    .unwrap_or(Value::Null),
                "newOrder": pick(&value, &["newOrder", "new_order", "order"])
                    .cloned()
                    .unwrap_or(Value::Null),
                "updatedAt": value.get("updatedAt").cloned().unwrap_or(Value::Null),
            });
            commit_hot!(update_workspace_doc_checked(&room, |_doc, txn| {
                update_workspace_folder(txn, &folder_id, &update_payload)
            }))
        }
        "workspace.moveDocuments" => {
            commit_hot!(update_workspace_doc_checked(&room, |_doc, txn| {
                move_workspace_documents(txn, payload)
            }))
        }
        "workspace.putArtifact" => {
            let value = object_payload(payload);
            let artifact_id = operation
                .document_id
                .clone()
                .or_else(|| string_value(pick(&value, &["artifactId", "artifact_id", "id"])))
                .ok_or_else(|| "put artifact operation is missing artifactId".to_string())?;
            // A path on the record is a claim on the owner's roots; it is
            // checked here, at put time, not only when the reference was made.
            if let Some(path) = value
                .get("source")
                .filter(|source| source.get("kind").and_then(Value::as_str) == Some("path"))
                .and_then(|source| source.get("path"))
                .and_then(Value::as_str)
                .filter(|path| {
                    // A put that re-sends the record's own path (a relabel,
                    // a move) is not a new claim; only a changed path is
                    // checked, so a moved or revoked file can still be
                    // relabeled. Reads re-authorize on their own.
                    let stored = crate::artifact_storage::artifact_record(app, &graph_id, &artifact_id)
                        .ok()
                        .flatten()
                        .and_then(|record| {
                            record
                                .get("source")
                                .and_then(|source| source.get("path"))
                                .and_then(Value::as_str)
                                .map(str::to_string)
                        });
                    stored.as_deref() != Some(*path)
                })
            {
                let authorized = crate::artifact_roots::authorize(app, &graph_id, Path::new(path))
                    .map_err(|denial| format!("workspace.putArtifact: source.path {denial}"))?;
                if authorized.canonical != Path::new(path) {
                    return Err(format!(
                        "workspace.putArtifact: source.path must be canonical ({} resolves to {})",
                        path,
                        authorized.canonical.display()
                    ));
                }
            }
            commit_hot!(update_workspace_doc_checked(&room, |_doc, txn| {
                put_workspace_artifact(txn, &graph_id, &artifact_id, &value)
            }))
        }
        crate::vault_import::KIND => {
            let accepted = crate::vault_import::decode_operation(operation)?;
            let claim = accepted.claim()?;
            let previous = room.with_doc(|doc| {
                let txn = doc.transact();
                let prior = crate::vault_adoption::preflight(&txn, &claim)?;
                if prior.is_none() {
                    validate_folder_parent_graph(&read_folder_entities(&txn))?;
                    if let Some(parent) = accepted.publication()["parentId"].as_str() {
                        if !txn.get_map("folders").is_some_and(|folders| matches!(folders.get(&txn, parent), Some(Out::YMap(_)))) {
                            return Err("import parent folder no longer exists".to_string());
                        }
                    }
                }
                Ok(prior)
            }).await?;
            if previous.is_none() {
                crate::vault_import::publish_artifact_bytes(app, &accepted).await?;
            }
            let value = object_payload(accepted.publication());
            let artifact_id = accepted.artifact_id();
            let result = commit_hot!(update_workspace_doc_checked(&room, |_, txn| {
                crate::vault_adoption::apply_once(txn, &claim, |txn| {
                    put_workspace_artifact(txn, &graph_id, &artifact_id, &value).map(|_| ())
                })
            }));
            json!({"artifactId":result.artifact_id,"replayed":result.replayed,"historicalApplication":true})
        }
        "workspace.deleteArtifact" => {
            let value = object_payload(payload);
            let artifact_id = operation
                .document_id
                .clone()
                .or_else(|| string_value(pick(&value, &["artifactId", "artifact_id", "id"])))
                .ok_or_else(|| "delete artifact operation is missing artifactId".to_string())?;
            let removed_wires = commit_hot!(update_workspace_doc_checked(&room, |_doc, txn| {
                delete_workspace_artifact_in_doc(txn, &artifact_id, recovered)
            }));
            // The wires' region files go now (the persist's sweep would
            // also catch them, after its grace).
            for wire_id in &removed_wires {
                crate::artifact_regions::remove_stored_region(&graph_dir, wire_id)?;
            }
            json!({ "id": artifact_id, "graphId": graph_id, "status": "deleted" })
        }
        "workspace.createWire" => {
            let mut value = object_payload(payload);
            let source_id = operation
                .document_id
                .clone()
                .or_else(|| string_value(pick(&value, &["sourceDocumentId", "source_document_id"])))
                .ok_or_else(|| "create wire operation is missing source documentId".to_string())?;
            // A region is validated against the artifact's bytes and
            // captured BEFORE the commit, through the gate every reader
            // uses; `fileHash` is set here, never taken from the client.
            // This operation is the one write path for wires, so the
            // invariant lives here and the REST route, the MCP tool and a
            // raw operation all meet it (design seed, step 3).
            let capture = prepare_wire_region(app, &graph_id, &mut value).await?;
            let data = commit_hot!(update_workspace_doc_checked(&room, |_doc, txn| {
                    create_wire_in_doc(txn, &graph_dir, &graph_id, &source_id, &value, false, !recovered)
                }));
            let wire_id = data
                .get("wireId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if let Some(capture) = capture {
                // After the commit: the side store never describes a wire
                // that does not exist. A crash between the two leaves a
                // wire without its file, which `read_artifact` reports.
                crate::artifact_regions::write_stored_region(&graph_dir, &capture.stored)?;
            }
            create_hosted_wire_response(&graph_id, &wire_id, &data)
        }
        "workspace.refreshWire" => {
            let value = object_payload(payload);
            let wire_id = string_value(pick(&value, &["wireId", "wire_id", "id"]))
                .ok_or_else(|| "refresh wire operation is missing wireId".to_string())?;
            let updated_at_ms = finite_number(value.get("updatedAt")).ok_or_else(|| {
                "workspace.refreshWire: updatedAt is required — normalize_payload_ids must inject it from enqueueTimestamp (A2 item 11)"
                    .to_string()
            })?;
            let data = commit_hot!(update_workspace_doc_checked(&room, |_doc, txn| {
                    refresh_workspace_wire_in_doc(
                        txn,
                        &graph_dir,
                        &graph_id,
                        &wire_id,
                        updated_at_ms,
                    )
                }));
            create_hosted_wire_response(&graph_id, &wire_id, &data)
        }
        "workspace.deleteWire" => {
            let value = object_payload(payload);
            let wire_id = string_value(pick(&value, &["wireId", "wire_id", "id"]))
                .ok_or_else(|| "delete wire operation is missing wireId".to_string())?;
            commit_hot!(update_workspace_doc_checked(&room, |_doc, txn| {
                let wires = txn.get_or_insert_map("wires");
                if !wires.contains_key(&*txn, &wire_id) {
                    if recovered {
                        return Ok(());
                    }
                    return Err(format!("wire not found: {wire_id}"));
                }
                wires.remove(txn, &wire_id);
                Ok(())
            }));
            // The region side store goes with the wire (the persist's sweep
            // would catch it; this makes the delete immediate).
            crate::artifact_regions::remove_stored_region(&graph_dir, &wire_id)?;
            json!({ "wireId": wire_id, "deleted": true })
        }
        other => return Err(format!("unsupported workspace CRDT operation: {other}")),
        };

        persist_workspace(app, &graph_id, &graph_dir, &room, &operation.operation_id).await?;
        if operation.kind == crate::vault_import::KIND {
            let accepted = crate::vault_import::decode_operation(operation)?;
            if let Err(error) = crate::vault_import::release_prepared_bytes(app, &accepted) {
                log::warn!("captured artifact staged-byte cleanup deferred: {error}");
            }
        }
        Ok(result)
    }
    .await;

    outcome.map_err(|error| {
        if hot_committed {
            ApplyOperationError::retryable_after_hot_commit(error)
        } else {
            ApplyOperationError::terminal(error)
        }
    })
}

/// Upsert a workspace document entry (equivalent of the TS
/// `createWorkspaceDocument` call). Reused by document.write so a written
/// document also appears in the workspace tree. Payload carries documentId,
/// title, parentId, order, updatedAt, readOnly (camelCase, as injected by
/// normalize_payload_ids).
pub(crate) async fn upsert_document_entry(
    app: &AppHandle,
    graph_id: &str,
    payload: &Value,
    operation_id: &str,
) -> Result<Value, String> {
    let graph_dir = crate::graph_paths::existing_graph_dir(app, graph_id)?;
    let room = workspace_room(app, graph_id, &graph_dir).await?;
    let result =
        update_workspace_doc_checked(&room, |_doc, txn| write_workspace_document(txn, payload))
            .await?;
    persist_workspace(app, graph_id, &graph_dir, &room, operation_id).await?;
    Ok(result)
}

/// Server-written wires (the dream cycle's dreamtOf), all in one workspace
/// transaction. Each item is (source document id, payload with wireId,
/// updatedAt, predicate, targetDocumentId and optional block ids). A family
/// with a lifetime is stamped with its expiry.
pub(crate) async fn create_server_wires(
    app: &AppHandle,
    graph_id: &str,
    wires: &[(String, JsonMap<String, Value>)],
    operation_id: &str,
) -> Result<usize, String> {
    if wires.is_empty() {
        return Ok(0);
    }
    let graph_dir = crate::graph_paths::existing_graph_dir(app, graph_id)?;
    let room = workspace_room(app, graph_id, &graph_dir).await?;
    let created = update_workspace_doc_checked(&room, |_doc, txn| {
        for (source_id, payload) in wires {
            create_wire_in_doc(txn, &graph_dir, graph_id, source_id, payload, true, false)?;
        }
        Ok(wires.len())
    })
    .await?;
    persist_workspace(app, graph_id, &graph_dir, &room, operation_id).await?;
    Ok(created)
}

/// Delete for good, not tombstone, every wire `select` picks, in one
/// workspace transaction. Expired and server-cleared wires leave the graph
/// entirely, so nothing accumulates (Eschaton, 2026-10-19). `select` sees
/// each wire as the snapshot materializes it, extra fields under `extra`.
pub(crate) async fn remove_wires_where(
    app: &AppHandle,
    graph_id: &str,
    operation_id: &str,
    select: impl Fn(&Value) -> bool,
) -> Result<usize, String> {
    let graph_dir = crate::graph_paths::existing_graph_dir(app, graph_id)?;
    let room = workspace_room(app, graph_id, &graph_dir).await?;
    let pick = |wires: Vec<Value>| -> Vec<String> {
        wires
            .iter()
            .filter(|wire| select(wire))
            .filter_map(|wire| wire.get("id").and_then(Value::as_str).map(str::to_string))
            .collect()
    };
    // Look first: a sweep that finds nothing must not write the document.
    if room.with_doc(|doc| pick(read_wire_entities(&doc.transact()))).await.is_empty() {
        return Ok(0);
    }
    let doomed = update_workspace_doc_checked(&room, |_doc, txn| {
        let doomed = pick(read_wire_entities(&*txn));
        let wires = txn.get_or_insert_map("wires");
        for id in &doomed {
            wires.remove(txn, id);
        }
        Ok(doomed)
    })
    .await?;
    for id in &doomed {
        crate::artifact_regions::remove_stored_region(&graph_dir, id)?;
    }
    let removed = doomed.len();
    if removed > 0 {
        persist_workspace(app, graph_id, &graph_dir, &room, operation_id).await?;
    }
    Ok(removed)
}

pub(crate) async fn workspace_room(
    app: &AppHandle,
    graph_id: &str,
    graph_dir: &Path,
) -> Result<Arc<Room>, String> {
    let registry = app.state::<RoomRegistry>();
    registry
        .get_or_create(
            &format!("workspace:{graph_id}"),
            crate::ydoc_paths::workspace_ydoc_state_path(graph_dir),
        )
        .await
}

/// Run a workspace mutation only when the complete folder-parent graph is
/// currently acyclic. The validation shares the room's single transaction
/// with the mutation, so another writer cannot close a cycle between a
/// separate preflight read and the checked write. Validating the whole graph
/// also prevents a mutation of one healthy subtree from silently blessing or
/// deleting around an unrelated closed cycle introduced by raw CRDT sync.
async fn update_workspace_doc_checked<T, F>(room: &Room, mutate: F) -> Result<T, String>
where
    F: FnOnce(&Doc, &mut TransactionMut<'_>) -> Result<T, String>,
{
    room.update_doc(|doc, txn| {
        validate_folder_parent_graph(&read_folder_entities(&*txn))?;
        mutate(doc, txn)
    })
    .await
}

/// Same downstream persistence as the `save_workspace` Tauri command:
/// materialized workspace.json snapshot, RDF projection, graph touch.
/// (The authoritative update-v1.bin is already persisted by the room.)
pub(crate) async fn persist_workspace(
    app: &AppHandle,
    graph_id: &str,
    graph_dir: &Path,
    room: &Arc<Room>,
    operation_id: &str,
) -> Result<bool, String> {
    let _projection_guard = room.lock_projection_flush().await;
    if !room.needs_projection_flush() {
        return Ok(false);
    }
    let (projection_epoch, snapshot) = materialize_workspace(room, graph_id).await?;
    persist_materialized_workspace(graph_id, graph_dir, &snapshot)?;
    sync_workspace_document_titles(app, graph_id, graph_dir, &snapshot, operation_id).await?;
    room.mark_projection_persisted(projection_epoch);
    Ok(true)
}

/// Complete (or repair) the normal persistence tail for a source-synchronized
/// workspace update. A retry whose sidecar and snapshot already agree repairs
/// RDF without touching graph revision; the first genuinely changed snapshot
/// follows the ordinary persistence path exactly once.
pub(crate) async fn ensure_workspace_source_effect_persisted(
    app: &AppHandle,
    graph_id: &str,
    graph_dir: &Path,
    room: &Arc<Room>,
    operation_id: &str,
) -> Result<bool, String> {
    let _projection_guard = room.lock_projection_flush().await;
    let (projection_epoch, snapshot) = materialize_workspace(room, graph_id).await?;
    let snapshot_path = crate::ydoc_paths::workspace_snapshot_path(graph_dir);
    let stored_matches = snapshot_path.is_file()
        && crate::storage::read_json::<Value>(&snapshot_path)
            .is_ok_and(|stored| stored == snapshot);
    if stored_matches {
        let store = crate::rdf_service::open_graph_store(graph_dir)?;
        crate::rdf_service::reconcile_workspace_snapshot(&store, graph_id, &snapshot)?;
        room.mark_projection_persisted(projection_epoch);
        return Ok(false);
    }
    persist_materialized_workspace(graph_id, graph_dir, &snapshot)?;
    sync_workspace_document_titles(app, graph_id, graph_dir, &snapshot, operation_id).await?;
    room.mark_projection_persisted(projection_epoch);
    Ok(true)
}

/// Rebuild only the disposable workspace snapshot/RDF faces from the Y.Doc.
///
/// Unlike a normal persistence flush this must not advance graph content
/// revision or synchronize document titles: projection replay is not a new
/// user mutation. Advancing `dcterms:modified` during a rebuild makes an
/// unchanged source set produce a different projection on every replay.
pub(crate) async fn rebuild_workspace_projection(
    graph_id: &str,
    graph_dir: &Path,
    room: &Arc<Room>,
) -> Result<bool, String> {
    let _projection_guard = room.lock_projection_flush().await;
    if !room.needs_projection_flush() {
        return Ok(false);
    }
    let (projection_epoch, snapshot) = materialize_workspace(room, graph_id).await?;
    {
        let _durability_guard = crate::cell_durability::write_guard();
        crate::storage::write_json(
            &crate::ydoc_paths::workspace_snapshot_path(graph_dir),
            &snapshot,
        )?;
        let store = crate::rdf_service::open_graph_store(graph_dir)?;
        crate::rdf_service::reconcile_workspace_snapshot(&store, graph_id, &snapshot)?;
    }
    room.mark_projection_persisted(projection_epoch);
    Ok(true)
}

pub(super) async fn materialize_workspace(
    room: &Room,
    graph_id: &str,
) -> Result<(u64, Value), String> {
    let (epoch, snapshot) = room
        .with_doc_version(|doc| materialize_workspace_snapshot_json(graph_id, doc))
        .await;
    Ok((epoch, snapshot?))
}

pub(super) fn persist_materialized_workspace(
    graph_id: &str,
    graph_dir: &Path,
    snapshot: &Value,
) -> Result<(), String> {
    let _durability_guard = crate::cell_durability::write_guard();
    crate::storage::write_json(
        &crate::ydoc_paths::workspace_snapshot_path(graph_dir),
        snapshot,
    )?;
    // Every path that removes a wire (the delete tool, the artifact,
    // document and folder cascades, the dream sweeps, an archive import
    // dropping references) ends in this persist, so the side store of
    // artifact regions is swept here: a file whose wire is gone goes too.
    crate::artifact_regions::sweep_orphans(graph_dir, snapshot);
    let store = crate::rdf_service::open_graph_store(graph_dir)?;
    crate::rdf_service::reconcile_workspace_snapshot(&store, graph_id, snapshot)?;
    crate::graph_record_store::touch_graph_content_revision(graph_dir)
        .map_err(crate::app_error::AppError::message)?;
    Ok(())
}

async fn sync_workspace_document_titles(
    app: &AppHandle,
    graph_id: &str,
    graph_dir: &Path,
    snapshot: &Value,
    operation_id: &str,
) -> Result<(), String> {
    let registry = app.state::<RoomRegistry>();
    let documents = snapshot
        .get("documents")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut stray_row_ids: Vec<&str> = Vec::new();
    for document in documents {
        let Some(document_id) = document.get("id").and_then(Value::as_str) else {
            continue;
        };
        // A client can write any key into the workspace map. A row whose id is
        // not a local id names no document body, so it has no title to sync.
        // A refusal here failed every later workspace write of the graph after
        // its hot commit, and each write was then retried.
        if crate::ids::validate_local_id(document_id, "document_id").is_err() {
            stray_row_ids.push(document_id);
            continue;
        }
        let Some(title) = document
            .get("title")
            .and_then(Value::as_str)
            .filter(|title| !title.is_empty())
        else {
            continue;
        };
        let manifest =
            crate::document_paths::document_dir(graph_dir, document_id)?.join("document.json");
        if !manifest.is_file() {
            // Document creation remains owned by its content channel. The
            // desktop analogue creates an empty record here, but doing that in
            // a cell would race a not-yet-arrived document websocket update.
            continue;
        }
        let tombstoned =
            crate::document_tombstone_store::document_is_tombstoned(graph_dir, document_id)?;
        // Reading normally backfills a legacy inline Y.Doc into update-v1.bin.
        // A trusted recreation keeps its tombstone until this workspace write
        // is durable, so inspect only the freshly-written manifest there and
        // never route through hydration while the marker still exists.
        if tombstoned {
            let record_title = crate::storage::read_json::<Value>(&manifest)
                .map_err(crate::app_error::AppError::message)?
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if record_title == title {
                continue;
            }
            return Err(format!(
                "tombstoned recreation title mismatch for {document_id}"
            ));
        }
        let record = crate::document_record_store::read_document_record(graph_dir, &manifest)?;
        if record.title == title {
            continue;
        }
        // Invariant: the workspace snapshot is the title authority, and this
        // sweep must NEVER hydrate a document room to enforce it. A hydrated
        // room here used to be retained forever by the registry, so a fresh
        // cell's first workspace flush mass-decoded every mismatched
        // document's full update history and OOMed. Titles for cold rooms
        // reconcile via the cold manifest now; room-internal state needs no
        // reconcile because the document Y.Doc holds no title — a lazily
        // opened room's next flush re-reads the title from the live workspace
        // room or this manifest (flush_ops::live_workspace_title →
        // stored_document_title).
        if let Some(room) = registry
            .peek(&format!("doc:{graph_id}:{document_id}"))
            .await
        {
            super::document_ops::sync_room_document_title(
                app,
                graph_id,
                document_id,
                title,
                &room,
                operation_id,
            )
            .await?;
            continue;
        }
        super::document_ops::sync_cold_document_title(app, &record, title, operation_id)?;
    }
    warn_stray_workspace_rows_once(graph_id, &stray_row_ids);
    Ok(())
}

/// Log each stray workspace row id once per process. The title sweep runs on
/// every workspace persist, so a warning per sweep would repeat on each write.
fn warn_stray_workspace_rows_once(graph_id: &str, stray_row_ids: &[&str]) {
    use std::sync::{Mutex, OnceLock};
    static WARNED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    if stray_row_ids.is_empty() {
        return;
    }
    let Ok(mut warned) = WARNED.get_or_init(|| Mutex::new(HashSet::new())).lock() else {
        return;
    };
    let fresh: Vec<&str> = stray_row_ids
        .iter()
        .copied()
        .filter(|id| warned.insert(format!("{graph_id}\u{0}{id}")))
        .collect();
    if !fresh.is_empty() {
        log::warn!(
            "[workspace] title sweep skipped {} row(s) of graph {graph_id} whose id is not a local id: {fresh:?}",
            fresh.len()
        );
    }
}

/// Read a non-empty document title directly from the authoritative workspace
/// Y.Doc. Shared by live reads and projection flushes so both use the same
/// title precedence.
pub(crate) fn document_title_in_workspace(doc: &Doc, document_id: &str) -> Option<String> {
    let txn = doc.transact();
    let documents = txn.get_map("documents")?;
    let Out::YMap(entry) = documents.get(&txn, document_id)? else {
        return None;
    };
    match entry.get(&txn, "title") {
        Some(Out::Any(Any::String(title))) if !title.is_empty() => Some(title.to_string()),
        _ => None,
    }
}

/// Membership guard for document-room connection. Unlike the title helper,
/// this checks the Y.Map entry itself: an untitled/partially initialized
/// workspace document is still an authorized document identity.
pub(crate) fn document_exists_in_workspace(doc: &Doc, document_id: &str) -> bool {
    let txn = doc.transact();
    txn.get_map("documents")
        .is_some_and(|documents| documents.contains_key(&txn, document_id))
}

fn folder_exists_in_workspace(doc: &Doc, folder_id: &str) -> bool {
    let txn = doc.transact();
    txn.get_map("folders")
        .is_some_and(|folders| folders.contains_key(&txn, folder_id))
}

// ─────────────────────────────────────────────────────────────────────────────
// Doc-level mutators (ports of the TS handler bodies)
// ─────────────────────────────────────────────────────────────────────────────

/// The refusals `document_service::create_document_with_lease` would make of
/// a `workspace.createDocument`, made before its workspace entry is committed.
/// After that commit any failure is kept as a tail to retry, so a refusal that
/// can never pass held the operation, and every later write to the graph
/// behind it: a title over `ids::MAX_TITLE_CHARS`, an id that is not a local
/// id, a deleted document's id. Without this check the commit had also already
/// overwritten an existing row's title, parent and order. Reads the payload as
/// `write_workspace_document` does, so the title checked is the one the body
/// is created with.
fn refuse_uncreatable_document(graph_dir: &Path, payload: &Value) -> Result<(), String> {
    let value = object_payload(payload);
    crate::ids::normalize_title(&non_empty_or(string_value(value.get("title")), "Untitled"))?;
    let Some(document_id) = string_value(pick(&value, &["documentId", "document_id", "id"])) else {
        return Ok(()); // `write_workspace_document` refuses it before the commit
    };
    crate::document_service::refuse_uncreatable_document_id(graph_dir, &document_id)
}

/// A folder delete's child rows, split: documents (local ids; a hard delete
/// deletes their bodies after the commit) and stray rows (any other key a
/// client wrote into the workspace map). A stray row names no document and has
/// no body, so it is only removed, and reported as `removedStrayRowIds`.
fn split_stray_rows(ids: Vec<String>) -> (Vec<String>, Vec<String>) {
    ids.into_iter()
        .partition(|id| crate::ids::validate_local_id(id, "document_id").is_ok())
}

/// Port of writeWorkspaceDocument (native-local-runtime.ts:1316).
pub(super) fn write_workspace_document(
    txn: &mut TransactionMut<'_>,
    payload: &Value,
) -> Result<Value, String> {
    let value = object_payload(payload);
    let document_id = string_value(pick(&value, &["documentId", "document_id", "id"]))
        .ok_or_else(|| "workspace.createDocument: documentId is required".to_string())?;
    let title = non_empty_or(string_value(value.get("title")), "Untitled");
    let parent_id = string_value(pick(&value, &["parentId", "parent_id"]));
    let order = finite_number(value.get("order")).ok_or_else(|| {
        "workspace.createDocument: order is required — normalize_payload_ids must inject it from enqueueTimestamp (A2 item 10)"
            .to_string()
    })?;
    let now = finite_number(value.get("updatedAt")).ok_or_else(|| {
        "workspace.createDocument: updatedAt is required — normalize_payload_ids must inject it from enqueueTimestamp (A2 item 11)"
            .to_string()
    })?;
    let documents = txn.get_or_insert_map("documents");
    let existed = documents.get(&*txn, &document_id).is_some();
    let document_map = child_map(&documents, txn, &document_id);
    // `preserveParent` / `preserveOrder` (set by document.write when the
    // caller named no placement) keep an EXISTING entry's parent and order;
    // a new entry is filed with the payload's values as before.
    let preserve_parent = existed && boolean_value(value.get("preserveParent"), false);
    let preserve_order = existed
        && boolean_value(value.get("preserveOrder"), false)
        && map_number(&*txn, &document_map, "order").is_some();
    // Read before this upsert may replace it: healing an impossible
    // createdAt uses the row's original filing order.
    let prior_order = map_number(&*txn, &document_map, "order");
    document_map.insert(txn, "title", title.as_str());
    if !preserve_parent {
        document_map.insert(txn, "parentId", opt_string_any(parent_id.clone()));
    }
    document_map.insert(txn, "section", "documents");
    if !preserve_order {
        document_map.insert(txn, "order", Any::Number(order));
    }
    let created_at = healed_created_at(
        map_number(&*txn, &document_map, "createdAt"),
        prior_order,
        now,
    )
    .unwrap_or(now);
    document_map.insert(txn, "createdAt", Any::Number(created_at));
    document_map.insert(txn, "updatedAt", Any::Number(now));
    document_map.insert(
        txn,
        "readOnly",
        boolean_value(pick(&value, &["readOnly", "read_only"]), false),
    );
    if value.contains_key("description") {
        let description = string_value(value.get("description")).unwrap_or_default();
        let described_at =
            finite_number(pick(&value, &["describedAt", "described_at"])).unwrap_or(now);
        document_map.insert(txn, "description", description.as_str());
        document_map.insert(txn, "describedAt", Any::Number(described_at));
    }
    let source_file = pick(&value, &["sourceFile", "source_file"])
        .map(object_payload)
        .unwrap_or_default();
    let storage_key = string_value(
        pick(&value, &["sf_storageKey", "storageKey"])
            .or_else(|| pick(&source_file, &["storageKey", "sf_storageKey"])),
    );
    let original_filename = string_value(
        pick(
            &value,
            &[
                "sf_originalFilename",
                "originalFilename",
                "original_filename",
            ],
        )
        .or_else(|| {
            pick(
                &source_file,
                &[
                    "originalFilename",
                    "original_filename",
                    "sf_originalFilename",
                ],
            )
        }),
    );
    let mime_type = string_value(
        pick(&value, &["sf_mimeType", "mimeType", "mime_type"])
            .or_else(|| pick(&source_file, &["mimeType", "mime_type", "sf_mimeType"])),
    );
    let size_bytes = finite_number(
        pick(&value, &["sf_sizeBytes", "sizeBytes", "size_bytes"])
            .or_else(|| pick(&source_file, &["sizeBytes", "size_bytes", "sf_sizeBytes"])),
    );
    let file_type = string_value(
        pick(&value, &["sf_fileType", "fileType", "file_type"])
            .or_else(|| pick(&source_file, &["fileType", "file_type", "sf_fileType"])),
    );
    if let Some(storage_key) = storage_key {
        document_map.insert(txn, "sf_storageKey", storage_key.as_str());
    }
    if let Some(original_filename) = original_filename {
        document_map.insert(txn, "sf_originalFilename", original_filename.as_str());
    }
    if let Some(mime_type) = mime_type {
        document_map.insert(txn, "sf_mimeType", mime_type.as_str());
    }
    if let Some(size_bytes) = size_bytes {
        document_map.insert(txn, "sf_sizeBytes", Any::Number(size_bytes));
    }
    if let Some(file_type) = file_type {
        document_map.insert(txn, "sf_fileType", file_type.as_str());
    }
    let parent_id = if preserve_parent {
        map_string(&*txn, &document_map, "parentId")
    } else {
        parent_id
    };
    let order = if preserve_order {
        map_number(&*txn, &document_map, "order").unwrap_or(order)
    } else {
        order
    };
    Ok(json!({
        "documentId": document_id,
        "id": document_id,
        "title": title,
        "parentId": opt_string_json(parent_id),
        "order": json_number(order),
    }))
}

/// Port of updateWorkspaceDocument (native-local-runtime.ts:1414).
pub(super) fn update_workspace_document(
    txn: &mut TransactionMut<'_>,
    document_id: &str,
    payload: &Value,
) -> Result<Value, String> {
    let value = object_payload(payload);
    let documents = txn.get_or_insert_map("documents");
    let document_map = match documents.get(&*txn, document_id) {
        Some(Out::YMap(map)) => map,
        _ => return Err(format!("document not found: {document_id}")),
    };
    let now = finite_number(value.get("updatedAt")).ok_or_else(|| {
        "workspace.updateDocument: updatedAt is required — normalize_payload_ids must inject it from enqueueTimestamp (A2 item 11)"
            .to_string()
    })?;
    if value.contains_key("title") {
        let title = non_empty_or(string_value(value.get("title")), "Untitled");
        document_map.insert(txn, "title", title.as_str());
    }
    if value.contains_key("parentId") || value.contains_key("parent_id") {
        let parent_id = string_value(pick(&value, &["parentId", "parent_id"]));
        document_map.insert(txn, "parentId", opt_string_any(parent_id));
    }
    if value.contains_key("order") {
        if let Some(order) = finite_number(value.get("order")) {
            document_map.insert(txn, "order", Any::Number(order));
        }
    }
    if value.contains_key("readOnly") || value.contains_key("read_only") {
        document_map.insert(
            txn,
            "readOnly",
            boolean_value(pick(&value, &["readOnly", "read_only"]), false),
        );
    }
    if value.contains_key("description") {
        let description = string_value(value.get("description")).unwrap_or_default();
        let described_at =
            finite_number(pick(&value, &["describedAt", "described_at"])).unwrap_or(now);
        document_map.insert(txn, "description", description.as_str());
        document_map.insert(txn, "describedAt", Any::Number(described_at));
    }
    document_map.insert(txn, "updatedAt", Any::Number(now));
    let description = if document_map.contains_key(&*txn, "description") {
        Value::String(map_string(&*txn, &document_map, "description").unwrap_or_default())
    } else {
        Value::Null
    };
    Ok(json!({
        "documentId": document_id,
        "id": document_id,
        "title": map_string(&*txn, &document_map, "title").unwrap_or_else(|| "Untitled".to_string()),
        "parentId": opt_string_json(map_string(&*txn, &document_map, "parentId")),
        "order": json_number(map_number(&*txn, &document_map, "order").unwrap_or(0.0)),
        "readOnly": map_bool(&*txn, &document_map, "readOnly", false),
        "description": description,
        "describedAt": map_number(&*txn, &document_map, "describedAt")
            .map(json_number)
            .unwrap_or(Value::Null),
        "updatedAt": json_number(now),
    }))
}

/// Port of writeWorkspaceFolder (native-local-runtime.ts:1385).
pub(super) fn write_workspace_folder(
    txn: &mut TransactionMut<'_>,
    payload: &Value,
) -> Result<Value, String> {
    let value = object_payload(payload);
    let folder_id = string_value(pick(&value, &["folderId", "folder_id", "id"]))
        .ok_or_else(|| "workspace.createFolder: folderId is required".to_string())?;
    let name = non_empty_or(
        string_value(pick(&value, &["name", "label"])),
        "Untitled Folder",
    );
    let parent_id = string_value(pick(&value, &["parentId", "parent_id"]));
    let section = if value.get("section").and_then(Value::as_str) == Some("artifacts") {
        "artifacts"
    } else {
        "documents"
    };
    let order = finite_number(value.get("order")).ok_or_else(|| {
        "workspace.createFolder: order is required — normalize_payload_ids must inject it from enqueueTimestamp (A2 item 10)"
            .to_string()
    })?;
    let folders = txn.get_or_insert_map("folders");
    let folder_map = child_map(&folders, txn, &folder_id);
    folder_map.insert(txn, "name", name.as_str());
    folder_map.insert(txn, "parentId", opt_string_any(parent_id.clone()));
    folder_map.insert(txn, "section", section);
    folder_map.insert(txn, "order", Any::Number(order));
    Ok(json!({
        "folderId": folder_id,
        "id": folder_id,
        "name": name,
        "parentId": opt_string_json(parent_id),
        "section": section,
        "order": json_number(order),
    }))
}

/// Port of updateWorkspaceFolder (native-local-runtime.ts:1491); also serves
/// moveWorkspaceFolder, which delegates with a normalized payload.
fn update_workspace_folder(
    txn: &mut TransactionMut<'_>,
    folder_id: &str,
    payload: &Value,
) -> Result<Value, String> {
    let value = object_payload(payload);
    let folders = txn.get_or_insert_map("folders");
    let folder_map = match folders.get(&*txn, folder_id) {
        Some(Out::YMap(map)) => map,
        _ => return Err(format!("folder not found: {folder_id}")),
    };
    let now = finite_number(value.get("updatedAt")).ok_or_else(|| {
        "workspace.updateFolder: updatedAt is required — normalize_payload_ids must inject it from enqueueTimestamp (A2 item 11)"
            .to_string()
    })?;
    if value.contains_key("name") || value.contains_key("label") {
        let name = non_empty_or(
            string_value(pick(&value, &["name", "label"])),
            "Untitled Folder",
        );
        folder_map.insert(txn, "name", name.as_str());
    }
    if value.contains_key("parentId")
        || value.contains_key("parent_id")
        || value.contains_key("newParentId")
        || value.contains_key("new_parent_id")
    {
        let parent_id = string_value(pick(
            &value,
            &["newParentId", "new_parent_id", "parentId", "parent_id"],
        ));
        let mut seen_parent_ids: HashSet<String> = HashSet::new();
        let mut ancestor_id = parent_id.clone();
        while let Some(current) = ancestor_id {
            if current == folder_id {
                return Err("folder cannot be moved under itself or its descendant".to_string());
            }
            if seen_parent_ids.contains(&current) {
                return Err("folder parent chain contains a cycle".to_string());
            }
            seen_parent_ids.insert(current.clone());
            let ancestor_map = match folders.get(&*txn, &current) {
                Some(Out::YMap(map)) => map,
                _ => return Err(format!("parent folder not found: {current}")),
            };
            ancestor_id = map_string(&*txn, &ancestor_map, "parentId");
        }
        folder_map.insert(txn, "parentId", opt_string_any(parent_id));
    }
    if value.contains_key("order")
        || value.contains_key("newOrder")
        || value.contains_key("new_order")
    {
        if let Some(order) = finite_number(pick(&value, &["newOrder", "new_order", "order"])) {
            folder_map.insert(txn, "order", Any::Number(order));
        }
    }
    folder_map.insert(txn, "updatedAt", Any::Number(now));
    Ok(json!({
        "folderId": folder_id,
        "id": folder_id,
        "name": map_string(&*txn, &folder_map, "name").unwrap_or_else(|| "Untitled Folder".to_string()),
        "parentId": opt_string_json(map_string(&*txn, &folder_map, "parentId")),
        "section": map_string(&*txn, &folder_map, "section").unwrap_or_else(|| "documents".to_string()),
        "order": json_number(map_number(&*txn, &folder_map, "order").unwrap_or(0.0)),
        "updatedAt": json_number(now),
    }))
}

/// Port of the transact body of deleteWorkspaceFolder
/// (native-local-runtime.ts:1560). Returns
/// (childDocuments, childArtifacts, deletedFolders); when the folder is
/// already absent it short-circuits gracefully with empty lists (replay).
fn delete_workspace_folder_in_doc(
    txn: &mut TransactionMut<'_>,
    folder_id: &str,
    cascade: bool,
) -> Result<(bool, Vec<String>, Vec<String>, Vec<String>), String> {
    let folders = txn.get_or_insert_map("folders");
    let documents = txn.get_or_insert_map("documents");
    let artifacts = txn.get_or_insert_map("artifacts");
    if !folders.contains_key(&*txn, folder_id) {
        return Ok((false, Vec::new(), Vec::new(), Vec::new()));
    }
    let mut child_documents: Vec<String> = Vec::new();
    let mut child_artifacts: Vec<String> = Vec::new();
    let mut deleted_folders: Vec<String> = Vec::new();
    let mut traversal = FolderTraversalGuard::default();
    collect_folder_children(
        &*txn,
        &folders,
        &documents,
        &artifacts,
        folder_id,
        &mut deleted_folders,
        &mut child_documents,
        &mut child_artifacts,
        &mut traversal,
    )?;
    if (!child_documents.is_empty() || !child_artifacts.is_empty() || !deleted_folders.is_empty())
        && !cascade
    {
        return Err("folder is not empty; pass cascade=true to delete children".to_string());
    }
    for document_id in &child_documents {
        documents.remove(txn, document_id);
    }
    for artifact_id in &child_artifacts {
        artifacts.remove(txn, artifact_id);
        remove_wires_targeting_artifact(txn, artifact_id);
    }
    for child_folder_id in &deleted_folders {
        folders.remove(txn, child_folder_id);
    }
    folders.remove(txn, folder_id);
    // A child document's borrowed record may live in another folder; it goes
    // with the document wherever it sits.
    let mut child_artifacts = child_artifacts;
    for document_id in &child_documents {
        for removed in cascade_document_delete_to_artifacts(txn, document_id) {
            if !child_artifacts.contains(&removed) {
                child_artifacts.push(removed);
            }
        }
    }
    Ok((true, child_documents, child_artifacts, deleted_folders))
}

#[derive(Default)]
struct FolderTraversalGuard {
    active: HashSet<String>,
    visited: HashSet<String>,
}

impl FolderTraversalGuard {
    /// Enter one folder in a checked depth-first traversal. `Ok(false)` means
    /// an already-completed node; encountering an active node is a parent
    /// cycle and must abort before any destructive write occurs.
    fn enter(&mut self, folder_id: &str) -> Result<bool, String> {
        if self.active.contains(folder_id) {
            return Err(format!(
                "workspace folder parent cycle detected at {folder_id}"
            ));
        }
        if self.visited.contains(folder_id) {
            return Ok(false);
        }
        self.active.insert(folder_id.to_string());
        Ok(true)
    }

    fn leave(&mut self, folder_id: &str) {
        self.active.remove(folder_id);
        self.visited.insert(folder_id.to_string());
    }
}

#[allow(clippy::too_many_arguments)]
fn collect_folder_children<T: ReadTxn>(
    txn: &T,
    folders: &MapRef,
    documents: &MapRef,
    artifacts: &MapRef,
    parent_id: &str,
    deleted_folders: &mut Vec<String>,
    child_documents: &mut Vec<String>,
    child_artifacts: &mut Vec<String>,
    traversal: &mut FolderTraversalGuard,
) -> Result<(), String> {
    if !traversal.enter(parent_id)? {
        return Ok(());
    }
    // Sorted key iteration for deterministic result ordering (the TS version
    // follows yjs-internal map order, which is unspecified).
    let mut folder_ids: Vec<String> = folders.keys(txn).map(str::to_string).collect();
    folder_ids.sort();
    for child_folder_id in folder_ids {
        let Some(Out::YMap(folder_map)) = folders.get(txn, &child_folder_id) else {
            continue;
        };
        if map_string(txn, &folder_map, "parentId").as_deref() != Some(parent_id) {
            continue;
        }
        collect_folder_children(
            txn,
            folders,
            documents,
            artifacts,
            &child_folder_id,
            deleted_folders,
            child_documents,
            child_artifacts,
            traversal,
        )?;
        deleted_folders.push(child_folder_id);
    }
    let mut document_ids: Vec<String> = documents.keys(txn).map(str::to_string).collect();
    document_ids.sort();
    for document_id in document_ids {
        let Some(Out::YMap(document_map)) = documents.get(txn, &document_id) else {
            continue;
        };
        if map_string(txn, &document_map, "parentId").as_deref() == Some(parent_id) {
            child_documents.push(document_id);
        }
    }
    let mut artifact_ids: Vec<String> = artifacts.keys(txn).map(str::to_string).collect();
    artifact_ids.sort();
    for artifact_id in artifact_ids {
        let Some(Out::YMap(artifact_map)) = artifacts.get(txn, &artifact_id) else {
            continue;
        };
        // A borrowed record is not a child of a folder: it goes with its
        // document, wherever that document is (and a drifted parentId
        // must neither block an empty-folder delete nor lose wires).
        if map_string(txn, &artifact_map, "storage").as_deref() == Some(ARTIFACT_STORAGE_BORROWED) {
            continue;
        }
        if map_string(txn, &artifact_map, "parentId").as_deref() == Some(parent_id) {
            child_artifacts.push(artifact_id);
        }
    }
    traversal.leave(parent_id);
    Ok(())
}

type FolderDeleteTargets = (Vec<String>, Vec<String>, Vec<String>);

fn recovered_folder_delete_targets(
    graph_dir: &Path,
    graph_id: &str,
    folder_id: &str,
) -> Result<Option<FolderDeleteTargets>, String> {
    let snapshot_path = crate::ydoc_paths::workspace_snapshot_path(graph_dir);
    let metadata = std::fs::metadata(&snapshot_path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            format!(
                "recovered hard folder cascade is missing workspace snapshot {}",
                snapshot_path.display()
            )
        } else {
            format!(
                "stat recovered workspace snapshot {}: {error}",
                snapshot_path.display()
            )
        }
    })?;
    if !metadata.is_file() {
        return Err(format!(
            "recovered workspace snapshot is not a file: {}",
            snapshot_path.display()
        ));
    }
    let snapshot: Value =
        crate::storage::read_json(&snapshot_path).map_err(|error| error.to_string())?;
    if snapshot.get("graphId").and_then(Value::as_str) != Some(graph_id) {
        return Err(format!(
            "recovered workspace snapshot graphId does not match {graph_id}"
        ));
    }
    folder_delete_targets_from_snapshot(&snapshot, folder_id)
}

fn snapshot_parent_map(
    snapshot: &Value,
    field: &str,
    id_label: &str,
) -> Result<HashMap<String, Option<String>>, String> {
    let entries = snapshot
        .get(field)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("recovered workspace snapshot is missing {field} array"))?;
    let mut parents = HashMap::new();
    for entry in entries {
        let object = entry
            .as_object()
            .ok_or_else(|| format!("recovered workspace {field} entry is not an object"))?;
        let id = object
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("recovered workspace {field} entry is missing id"))?;
        // Any key a client wrote can be here. Ids and parents are compared as
        // strings only, and no path is made of them: a child document's body
        // is deleted only when its id is a local id (`split_stray_rows`).
        // Refusing a stray key here failed the recovered cascade after its
        // hot commit, and that refusal was retried.
        if id.is_empty() {
            return Err(format!(
                "recovered workspace {field} entry: {id_label} cannot be empty"
            ));
        }
        let parent = match object.get("parentId") {
            None | Some(Value::Null) => None,
            Some(Value::String(parent)) if !parent.is_empty() => Some(parent.clone()),
            Some(_) => {
                return Err(format!(
                    "recovered workspace {field} entry {id} has invalid parentId"
                ));
            }
        };
        if parents.insert(id.to_string(), parent).is_some() {
            return Err(format!(
                "recovered workspace {field} contains duplicate id {id}"
            ));
        }
    }
    Ok(parents)
}

fn folder_delete_targets_from_snapshot(
    snapshot: &Value,
    folder_id: &str,
) -> Result<Option<FolderDeleteTargets>, String> {
    if folder_id.is_empty() {
        return Err("folder_id cannot be empty".to_string());
    }
    let folders = snapshot_parent_map(snapshot, "folders", "folder_id")?;
    if !folders.contains_key(folder_id) {
        return Ok(None);
    }
    let documents = snapshot_parent_map(snapshot, "documents", "document_id")?;
    let artifacts = snapshot_parent_map(snapshot, "artifacts", "artifact_id")?;
    let mut child_documents = Vec::new();
    let mut child_artifacts = Vec::new();
    let mut deleted_folders = Vec::new();
    let mut active = HashSet::new();
    let mut visited = HashSet::new();
    collect_snapshot_folder_children(
        folder_id,
        &folders,
        &documents,
        &artifacts,
        &mut active,
        &mut visited,
        &mut deleted_folders,
        &mut child_documents,
        &mut child_artifacts,
    )?;
    Ok(Some((child_documents, child_artifacts, deleted_folders)))
}

#[allow(clippy::too_many_arguments)]
fn collect_snapshot_folder_children(
    parent_id: &str,
    folders: &HashMap<String, Option<String>>,
    documents: &HashMap<String, Option<String>>,
    artifacts: &HashMap<String, Option<String>>,
    active: &mut HashSet<String>,
    visited: &mut HashSet<String>,
    deleted_folders: &mut Vec<String>,
    child_documents: &mut Vec<String>,
    child_artifacts: &mut Vec<String>,
) -> Result<(), String> {
    if !active.insert(parent_id.to_string()) {
        return Err(format!(
            "recovered workspace folder cycle reaches {parent_id}"
        ));
    }
    if visited.contains(parent_id) {
        return Err(format!(
            "recovered workspace folder {parent_id} is reachable more than once"
        ));
    }

    let mut child_folders = folders
        .iter()
        .filter_map(|(id, parent)| (parent.as_deref() == Some(parent_id)).then_some(id.clone()))
        .collect::<Vec<_>>();
    child_folders.sort();
    for child_folder in child_folders {
        collect_snapshot_folder_children(
            &child_folder,
            folders,
            documents,
            artifacts,
            active,
            visited,
            deleted_folders,
            child_documents,
            child_artifacts,
        )?;
        deleted_folders.push(child_folder);
    }

    let mut direct_documents = documents
        .iter()
        .filter_map(|(id, parent)| (parent.as_deref() == Some(parent_id)).then_some(id.clone()))
        .collect::<Vec<_>>();
    direct_documents.sort();
    child_documents.extend(direct_documents);
    let mut direct_artifacts = artifacts
        .iter()
        .filter_map(|(id, parent)| (parent.as_deref() == Some(parent_id)).then_some(id.clone()))
        .collect::<Vec<_>>();
    direct_artifacts.sort();
    child_artifacts.extend(direct_artifacts);

    active.remove(parent_id);
    visited.insert(parent_id.to_string());
    Ok(())
}

/// Earliest creation time a row can plausibly carry (2015-01-01, epoch ms).
const MIN_PLAUSIBLE_CREATED_AT_MS: f64 = 1_420_070_400_000.0;
const DAY_MS: f64 = 24.0 * 60.0 * 60.0 * 1000.0;

/// A row's creation time, healed when it is impossible. Some rows imported
/// from cloud-1 carry createdAt values 1000 times too large (years ~25000 to
/// ~50000). A value more than a day after `now` is replaced by the row's
/// `order` when that reads as a plausible past instant (rows are ordered by
/// their creation time at filing), else by `now`. None when there is no
/// createdAt; a sane value comes back unchanged.
pub(crate) fn healed_created_at(created_at: Option<f64>, order: Option<f64>, now: f64) -> Option<f64> {
    let created_at = created_at?;
    if created_at <= now + DAY_MS {
        return Some(created_at);
    }
    let from_order = order.filter(|order| {
        order.is_finite() && *order >= MIN_PLAUSIBLE_CREATED_AT_MS && *order <= now
    });
    let healed = from_order.unwrap_or(now);
    log::warn!("workspace: an impossible createdAt ({created_at}) became {healed}");
    Some(healed)
}

/// Port of moveWorkspaceDocuments (native-local-runtime.ts:1793).
fn move_workspace_documents(
    txn: &mut TransactionMut<'_>,
    payload: &Value,
) -> Result<Value, String> {
    let value = object_payload(payload);
    let document_ids = string_array_value(value.get("documentIds"));
    let parent_id = string_value(value.get("parentId"));
    let base_order = finite_number(value.get("order")).ok_or_else(|| {
        "workspace.moveDocuments: order is required — normalize_payload_ids must inject it from enqueueTimestamp (A2 item 10)"
            .to_string()
    })?;
    let updated_at = finite_number(value.get("updatedAt")).ok_or_else(|| {
        "workspace.moveDocuments: updatedAt is required — normalize_payload_ids must inject it from enqueueTimestamp (A2 item 11)"
            .to_string()
    })?;
    let documents = txn.get_or_insert_map("documents");
    let mut moved: Vec<String> = Vec::new();
    let mut missing: Vec<String> = Vec::new();
    for (index, document_id) in document_ids.iter().enumerate() {
        let document_map = match documents.get(&*txn, document_id) {
            Some(Out::YMap(map)) => map,
            _ => {
                missing.push(document_id.clone());
                continue;
            }
        };
        let existing_created = map_number(&*txn, &document_map, "createdAt");
        if let Some(healed) = healed_created_at(
            existing_created,
            map_number(&*txn, &document_map, "order"),
            updated_at,
        ) {
            if Some(healed) != existing_created {
                document_map.insert(txn, "createdAt", Any::Number(healed));
            }
        }
        document_map.insert(txn, "parentId", opt_string_any(parent_id.clone()));
        document_map.insert(txn, "order", Any::Number(base_order + index as f64));
        document_map.insert(txn, "updatedAt", Any::Number(updated_at));
        moved.push(document_id.clone());
        // The borrowed record sits beside its document.
        let artifacts = txn.get_or_insert_map("artifacts");
        if let Some(Out::YMap(record)) = artifacts.get(&*txn, &borrowed_artifact_id_for(document_id)) {
            if map_string(&*txn, &record, "storage").as_deref() == Some(ARTIFACT_STORAGE_BORROWED) {
                record.insert(txn, "parentId", opt_string_any(parent_id.clone()));
                record.insert(txn, "order", Any::Number(base_order + index as f64));
            }
        }
    }
    Ok(json!({ "moved": moved, "missing": missing, "parentId": opt_string_json(parent_id) }))
}

/// Port of putWorkspaceArtifact (native-local-runtime.ts:1884). Returns the
/// hosted artifact response shape.
pub(crate) fn put_workspace_artifact(
    txn: &mut TransactionMut<'_>,
    graph_id: &str,
    artifact_id: &str,
    value: &JsonMap<String, Value>,
) -> Result<Value, String> {
    let artifacts = txn.get_or_insert_map("artifacts");
    let updated_at_ms = finite_number(value.get("updatedAt")).ok_or_else(|| {
        "workspace.putArtifact: updatedAt is required — normalize_payload_ids must inject it from enqueueTimestamp (A2 item 11)"
            .to_string()
    })?;
    let now = epoch_ms_to_iso(updated_at_ms);
    // Refusals come before the map is touched: a Yrs transaction has no
    // rollback, so a refused put must not leave a stub record behind.
    let mode = match string_value(value.get("mode")) {
        Some(mode) => Some(artifact_mode_name(&mode)?),
        None => None,
    };
    let storage = match string_value(value.get("storage")) {
        Some(storage) => Some(artifact_storage_name(&storage)?),
        None => None,
    };
    // The borrow link is a server-side invariant, not a convention (angel
    // review of step 1.5): a borrowed record is `original-{doc}`, names the
    // document whose original it borrows, that document exists, and no
    // later put may re-point, clear or un-borrow it. An id with the
    // `original-` prefix is borrowed or refused.
    let (existing_storage, existing_ingested) = match artifacts.get(&*txn, artifact_id) {
        Some(Out::YMap(map)) => (
            map_string(&*txn, &map, "storage"),
            map_string(&*txn, &map, "ingestedDocId"),
        ),
        _ => (None, None),
    };
    if existing_storage.as_deref() == Some(ARTIFACT_STORAGE_BORROWED)
        && storage.as_deref() == Some(ARTIFACT_STORAGE_OWNED)
    {
        return Err(format!(
            "workspace.putArtifact: {artifact_id} borrows its bytes from a document; it cannot become owned"
        ));
    }
    let effective_storage = storage.clone().or(existing_storage.clone());
    let requested_ingested = string_value(pick(value, &["ingestedDocId", "ingested_doc_id"]));
    let borrowed_document = if effective_storage.as_deref() == Some(ARTIFACT_STORAGE_BORROWED) {
        let document_id = match (requested_ingested.clone(), existing_ingested.clone()) {
            (Some(requested), Some(existing)) if requested != existing => {
                return Err(format!(
                    "workspace.putArtifact: {artifact_id} borrows the original of {existing}; it cannot be re-pointed at {requested}"
                ))
            }
            (Some(requested), _) => requested,
            (None, Some(existing)) => existing,
            (None, None) => {
                return Err(format!(
                    "workspace.putArtifact: a borrowed record needs ingestedDocId (the document whose original it borrows)"
                ))
            }
        };
        if artifact_id != borrowed_artifact_id_for(&document_id) {
            return Err(format!(
                "workspace.putArtifact: a borrowed record for {document_id} must be named {}",
                borrowed_artifact_id_for(&document_id)
            ));
        }
        if !txn.get_or_insert_map("documents").contains_key(&*txn, &document_id) {
            return Err(format!(
                "workspace.putArtifact: {artifact_id} borrows the original of {document_id}, which is not in the workspace"
            ));
        }
        Some(document_id)
    } else {
        if artifact_id.starts_with(BORROWED_ARTIFACT_ID_PREFIX) {
            return Err(format!(
                "workspace.putArtifact: {artifact_id} is a borrowed record's name; put it with storage borrowed"
            ));
        }
        None
    };
    let sha256 = match string_value(value.get("sha256")) {
        Some(hash) => {
            let hash = hash.trim().to_ascii_lowercase();
            if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(
                    "workspace.putArtifact: sha256 must be 64 lowercase hex characters".to_string(),
                );
            }
            Some(hash)
        }
        None => None,
    };
    for (key, cap) in [("source", ARTIFACT_SOURCE_BYTES_CAP), ("index", ARTIFACT_INDEX_BYTES_CAP)] {
        if let Some(object) = value.get(key).filter(|v| v.is_object()) {
            let bytes = serde_json::to_vec(object).map(|v| v.len()).unwrap_or(usize::MAX);
            if bytes > cap {
                return Err(format!(
                    "workspace.putArtifact: {key} is {bytes} bytes; the cap is {cap}"
                ));
            }
        }
    }
    if let Some(source) = value.get("source").filter(|v| v.is_object()) {
        let kind = string_value(source.get("kind")).unwrap_or_default();
        if kind != "upload" && kind != "import" && kind != "path" {
            return Err(format!(
                "workspace.putArtifact: source.kind '{kind}' is not accepted on the record (upload, import, path)"
            ));
        }
    }
    // A reference is one thing said three ways, and the three must agree:
    // mode referenced, storage external, source.kind path. The path itself
    // is checked against the owner's roots by the operation handler before
    // this runs (`apply_classified`), so a raw put cannot assert a
    // reference to a path the owner never granted.
    {
        let (existing_mode, existing_kind) = match artifacts.get(&*txn, artifact_id) {
            Some(Out::YMap(map)) => (
                map_string(&*txn, &map, "mode"),
                map_value(&*txn, &map, "source")
                    .get("kind")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            ),
            _ => (None, None),
        };
        let effective_mode = mode.clone().or(existing_mode);
        let effective_kind = value
            .get("source")
            .and_then(|source| source.get("kind"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .or(existing_kind);
        let existing_storage_now = match artifacts.get(&*txn, artifact_id) {
            Some(Out::YMap(map)) => map_string(&*txn, &map, "storage"),
            _ => None,
        };
        let effective_storage_now = string_value(value.get("storage"))
            .map(|s| s.trim().to_ascii_lowercase())
            .or(existing_storage_now);
        let is_ref = effective_mode.as_deref() == Some(ARTIFACT_MODE_REFERENCED);
        let is_path = effective_kind.as_deref() == Some("path");
        let is_external = effective_storage_now.as_deref() == Some(ARTIFACT_STORAGE_EXTERNAL);
        if is_ref != is_path || is_ref != is_external {
            return Err(format!(
                "workspace.putArtifact: a reference is mode referenced, storage external and source.kind path together (got mode {}, storage {}, source.kind {})",
                effective_mode.as_deref().unwrap_or("none"),
                effective_storage_now.as_deref().unwrap_or("none"),
                effective_kind.as_deref().unwrap_or("none"),
            ));
        }
    }
    let artifact_map = match artifacts.get(&*txn, artifact_id) {
        Some(Out::YMap(map)) => map,
        _ => {
            let map = artifacts.insert(txn, artifact_id, MapPrelim::default());
            map.insert(txn, "createdAt", now.as_str());
            map
        }
    };
    let label = non_empty_or(
        string_value(pick(value, &["label", "name", "title"])),
        artifact_id,
    );
    let original_filename = string_value(pick(value, &["originalFilename", "original_filename"]))
        .unwrap_or_else(|| label.clone());
    let mime_type = string_value(pick(value, &["mimeType", "mime_type"]));
    let size_bytes = finite_number(pick(value, &["sizeBytes", "size_bytes", "size"]));
    let file_type = string_value(pick(value, &["fileType", "file_type"]))
        .unwrap_or_else(|| artifact_file_type(&original_filename));
    artifact_map.insert(txn, "name", label.as_str());
    artifact_map.insert(
        txn,
        "parentId",
        opt_string_any(string_value(pick(value, &["parentId", "parent_id"]))),
    );
    let order = finite_number(value.get("order")).ok_or_else(|| {
        "workspace.putArtifact: order is required — normalize_payload_ids must inject it from enqueueTimestamp (A2 item 10)"
            .to_string()
    })?;
    artifact_map.insert(txn, "order", Any::Number(order));
    artifact_map.insert(txn, "fileType", file_type.as_str());
    let status = string_value(value.get("status"))
        .or_else(|| map_string(&*txn, &artifact_map, "status"))
        .unwrap_or_else(|| "uploading".to_string());
    artifact_map.insert(txn, "status", status.as_str());
    artifact_map.insert(
        txn,
        "errorMessage",
        opt_string_any(string_value(pick(
            value,
            &["errorMessage", "error_message"],
        ))),
    );
    artifact_map.insert(txn, "mimeType", opt_string_any(mime_type.clone()));
    artifact_map.insert(txn, "size", opt_number_any(size_bytes));
    artifact_map.insert(
        txn,
        "ingestedDocId",
        opt_string_any(string_value(pick(
            value,
            &["ingestedDocId", "ingested_doc_id"],
        ))),
    );
    artifact_map.insert(
        txn,
        "sf_storageKey",
        opt_string_any(string_value(pick(value, &["storageKey", "storage_key"]))),
    );
    artifact_map.insert(txn, "sf_originalFilename", original_filename.as_str());
    artifact_map.insert(txn, "sf_mimeType", opt_string_any(mime_type));
    artifact_map.insert(txn, "sf_sizeBytes", opt_number_any(size_bytes));
    artifact_map.insert(txn, "sf_fileType", file_type.as_str());
    if let Some(scene_projection) =
        pick(value, &["sceneProjection", "scene_projection"]).filter(|v| v.is_object())
    {
        artifact_map.insert(txn, "sceneProjection", json_to_any(scene_projection));
    }
    if let Some(scene_projection_text) = string_value(pick(
        value,
        &["sceneProjectionText", "scene_projection_text"],
    )) {
        artifact_map.insert(txn, "sceneProjectionText", scene_projection_text.as_str());
    }
    if let Some(scene_projected_at) =
        string_value(pick(value, &["sceneProjectedAt", "scene_projected_at"]))
    {
        artifact_map.insert(txn, "sceneProjectedAt", scene_projected_at.as_str());
    }
    // The body-outside-the-house fields (design seed 2026-10-23): a mode on
    // the ladder, the hash the graph saw, where the bytes came from, and a
    // light index. All optional; an old record keeps what it has.
    if let Some(mode) = mode {
        artifact_map.insert(txn, "mode", mode.as_str());
    }
    if let Some(storage) = storage {
        artifact_map.insert(txn, "storage", storage.as_str());
    }
    if let Some(document_id) = borrowed_document {
        artifact_map.insert(txn, "storage", ARTIFACT_STORAGE_BORROWED);
        artifact_map.insert(txn, "ingestedDocId", document_id.as_str());
    }
    if let Some(sha256) = sha256 {
        artifact_map.insert(txn, "sha256", sha256.as_str());
    }
    if let Some(hashed_at) = string_value(pick(value, &["hashedAt", "hashed_at"])) {
        artifact_map.insert(txn, "hashedAt", hashed_at.as_str());
    }
    if let Some(source) = value.get("source").filter(|v| v.is_object()) {
        artifact_map.insert(txn, "source", json_to_any(source));
    }
    if let Some(index) = value.get("index").filter(|v| v.is_object()) {
        artifact_map.insert(txn, "index", json_to_any(index));
    }
    artifact_map.insert(txn, "updatedAt", now.as_str());
    let data = artifact_data_from_map(&*txn, &artifact_map);
    Ok(create_hosted_artifact_response(
        graph_id,
        artifact_id,
        &data,
    ))
}

/// Returns the ids of the wires removed with the artifact, so the caller
/// can unlink their region files at once.
fn delete_workspace_artifact_in_doc(
    txn: &mut TransactionMut<'_>,
    artifact_id: &str,
    allow_missing: bool,
) -> Result<Vec<String>, String> {
    let artifacts = txn.get_or_insert_map("artifacts");
    if !artifacts.contains_key(&*txn, artifact_id) {
        if allow_missing {
            return Ok(Vec::new());
        }
        return Err(format!("artifact not found: {artifact_id}"));
    }
    let storage = match artifacts.get(&*txn, artifact_id) {
        Some(Out::YMap(map)) => map_string(&*txn, &map, "storage"),
        _ => None,
    };
    artifacts.remove(txn, artifact_id);
    // A deleted artifact takes its wires with it, as a deleted document does:
    // removed for good, never tombstoned (Eschaton, 2026-10-19).
    let removed_wires = remove_wires_targeting_artifact(txn, artifact_id);
    // An OWNED record's bytes die with it. A document digested from them
    // keeps its blocks but loses its sf_* back-pointers: there is no original
    // to open any more, and a stale pointer would send the reader to a hole.
    // A BORROWED record's bytes belong to its document and are untouched.
    // The record's ingestedDocId is not the authority here (a loopback
    // ingest clears it): every document whose pointer is under the dead
    // store loses the pointer.
    if storage.as_deref() != Some(ARTIFACT_STORAGE_BORROWED) {
        let documents = txn.get_or_insert_map("documents");
        let prefix = format!("local://artifacts/{artifact_id}/");
        let pointing: Vec<MapRef> = documents
            .iter(&*txn)
            .filter_map(|(_, value)| match value {
                Out::YMap(document_map) => map_string(&*txn, &document_map, "sf_storageKey")
                    .filter(|key| key.starts_with(&prefix))
                    .map(|_| document_map),
                _ => None,
            })
            .collect();
        for document_map in pointing {
            for field in SOURCE_FILE_FIELDS {
                document_map.remove(txn, field);
            }
        }
    }
    Ok(removed_wires)
}

/// Every wire that points at the artifact (as a target or as a scene) goes
/// with it. Returns their ids.
pub(super) fn remove_wires_targeting_artifact(txn: &mut TransactionMut<'_>, artifact_id: &str) -> Vec<String> {
    let wires = txn.get_or_insert_map("wires");
    let orphaned: Vec<String> = wires
        .iter(&*txn)
        .filter_map(|(wire_id, value)| match value {
            Out::YMap(wire_map) => {
                let targets = map_string(&*txn, &wire_map, "targetArtifactId").as_deref()
                    == Some(artifact_id)
                    || map_string(&*txn, &wire_map, "sceneArtifactId").as_deref()
                        == Some(artifact_id);
                targets.then(|| wire_id.to_string())
            }
            _ => None,
        })
        .collect();
    for wire_id in &orphaned {
        wires.remove(txn, wire_id);
    }
    orphaned
}

/// A deleted document takes its BORROWED record (the one whose bytes live
/// under the document's own original store) and that record's wires with
/// it. An OWNED record that was digested into the document keeps its bytes
/// and drops back to `attached`: the blocks are gone, the body is not.
/// Returns the ids of the records removed. (Design seed, corrections
/// 2026-10-23: "Owned or borrowed bytes".)
pub(super) fn cascade_document_delete_to_artifacts(
    txn: &mut TransactionMut<'_>,
    document_id: &str,
) -> Vec<String> {
    let artifacts = txn.get_or_insert_map("artifacts");
    let mut borrowed: Vec<String> = Vec::new();
    let mut owned: Vec<String> = Vec::new();
    for (artifact_id, value) in artifacts.iter(&*txn) {
        let Out::YMap(map) = value else { continue };
        if map_string(&*txn, &map, "ingestedDocId").as_deref() != Some(document_id) {
            continue;
        }
        if map_string(&*txn, &map, "storage").as_deref() == Some(ARTIFACT_STORAGE_BORROWED) {
            borrowed.push(artifact_id.to_string());
        } else {
            owned.push(artifact_id.to_string());
        }
    }
    borrowed.sort();
    for artifact_id in &borrowed {
        artifacts.remove(txn, artifact_id);
        remove_wires_targeting_artifact(txn, artifact_id);
    }
    for artifact_id in owned {
        if let Some(Out::YMap(map)) = artifacts.get(&*txn, &artifact_id) {
            map.remove(txn, "ingestedDocId");
            map.insert(txn, "mode", ARTIFACT_MODE_ATTACHED);
            if map_string(&*txn, &map, "status").as_deref() == Some("ingested") {
                map.insert(txn, "status", "ready");
            }
        }
    }
    borrowed
}

/// Put an artifact record from inside an operation handler that already holds
/// the graph: the same room, the same persistence step as
/// `upsert_document_entry`, no second queue hop.
pub(crate) async fn upsert_artifact_entry(
    app: &AppHandle,
    graph_id: &str,
    artifact_id: &str,
    payload: &Value,
    operation_id: &str,
) -> Result<Value, String> {
    let graph_dir = crate::graph_paths::existing_graph_dir(app, graph_id)?;
    let room = workspace_room(app, graph_id, &graph_dir).await?;
    let value = object_payload(payload);
    let result = update_workspace_doc_checked(&room, |_doc, txn| {
        put_workspace_artifact(txn, graph_id, artifact_id, &value)
    })
    .await?;
    persist_workspace(app, graph_id, &graph_dir, &room, operation_id).await?;
    Ok(result)
}

/// The three rungs of the ladder, by their record names.
pub(crate) const ARTIFACT_MODES: &[&str] = &["referenced", "attached", "digested"];
pub(crate) const ARTIFACT_MODE_ATTACHED: &str = "attached";
pub(crate) const ARTIFACT_MODE_DIGESTED: &str = "digested";
pub(crate) const ARTIFACT_MODE_REFERENCED: &str = "referenced";
/// Whose bytes a record keeps: its own, under `artifacts/{id}/original/`
/// (they die with the record), or a document's, under
/// `documents/{id}/original/` (they die with the document). Absent on a
/// record made before 2026-10-23 means owned.
/// `external`: a reference; the bytes are outside the house and never
/// copied.
pub(crate) const ARTIFACT_STORAGES: &[&str] = &["owned", "borrowed", "external"];
pub(crate) const ARTIFACT_STORAGE_OWNED: &str = "owned";
pub(crate) const ARTIFACT_STORAGE_BORROWED: &str = "borrowed";
pub(crate) const ARTIFACT_STORAGE_EXTERNAL: &str = "external";
pub(crate) const BORROWED_ARTIFACT_ID_PREFIX: &str = "original-";

/// The one name a borrowed record may have.
pub(crate) fn borrowed_artifact_id_for(document_id: &str) -> String {
    format!("{BORROWED_ARTIFACT_ID_PREFIX}{document_id}")
}

pub(crate) fn artifact_storage_name(value: &str) -> Result<String, String> {
    let name = value.trim().to_ascii_lowercase();
    if ARTIFACT_STORAGES.contains(&name.as_str()) {
        Ok(name)
    } else {
        Err(format!(
            "workspace.putArtifact: storage must be one of {} (got '{name}')",
            ARTIFACT_STORAGES.join(", ")
        ))
    }
}
/// Serialized caps on the record's `source` and `index`, which ride in every
/// navigation list and workspace snapshot (angel review F3).
pub(crate) const ARTIFACT_SOURCE_BYTES_CAP: usize = 16 * 1024;
pub(crate) const ARTIFACT_INDEX_BYTES_CAP: usize = 256 * 1024;

/// Accept the record words and the door's verbs; refuse anything else so a
/// typo never becomes a fourth mode.
pub(crate) fn artifact_mode_name(value: &str) -> Result<String, String> {
    let name = match value.trim().to_ascii_lowercase().as_str() {
        "referenced" | "reference" => "referenced",
        "attached" | "attach" => "attached",
        "digested" | "digest" | "ingested" => "digested",
        other => {
            return Err(format!(
                "workspace.putArtifact: mode must be one of {} (got '{other}')",
                ARTIFACT_MODES.join(", ")
            ))
        }
    };
    Ok(name.to_string())
}

/// Port of artifactDataFromMap (native-local-runtime.ts:1863).
fn artifact_data_from_map<T: ReadTxn>(txn: &T, artifact_map: &MapRef) -> Value {
    let pick_map = |keys: &[&str]| -> Value {
        for key in keys {
            let value = map_value(txn, artifact_map, key);
            if !value.is_null() {
                return value;
            }
        }
        Value::Null
    };
    json!({
        "name": map_value(txn, artifact_map, "name"),
        "parentId": map_value(txn, artifact_map, "parentId"),
        "order": map_value(txn, artifact_map, "order"),
        "fileType": map_value(txn, artifact_map, "fileType"),
        "status": map_value(txn, artifact_map, "status"),
        "errorMessage": map_value(txn, artifact_map, "errorMessage"),
        "storageKey": pick_map(&["sf_storageKey", "storageKey"]),
        "originalFilename": pick_map(&["sf_originalFilename", "originalFilename"]),
        "mimeType": pick_map(&["mimeType", "sf_mimeType"]),
        "sizeBytes": pick_map(&["sf_sizeBytes", "sizeBytes", "size"]),
        "ingestedDocId": map_value(txn, artifact_map, "ingestedDocId"),
        "mode": map_value(txn, artifact_map, "mode"),
        "storage": map_value(txn, artifact_map, "storage"),
        "sha256": map_value(txn, artifact_map, "sha256"),
        "hashedAt": map_value(txn, artifact_map, "hashedAt"),
        "source": map_value(txn, artifact_map, "source"),
        "index": map_value(txn, artifact_map, "index"),
        "sceneProjection": map_value(txn, artifact_map, "sceneProjection"),
        "sceneProjectionText": map_value(txn, artifact_map, "sceneProjectionText"),
        "sceneProjectedAt": map_value(txn, artifact_map, "sceneProjectedAt"),
        "createdAt": map_value(txn, artifact_map, "createdAt"),
        "updatedAt": map_value(txn, artifact_map, "updatedAt"),
    })
}

/// Port of createHostedArtifactResponse (native-local-runtime.ts:1833).
fn create_hosted_artifact_response(graph_id: &str, artifact_id: &str, data: &Value) -> Value {
    let get = |key: &str| data.get(key).cloned().unwrap_or(Value::Null);
    let pick_data = |keys: &[&str]| -> Value {
        for key in keys {
            let value = get(key);
            if !value.is_null() {
                return value;
            }
        }
        Value::Null
    };
    let label = string_value(Some(&pick_data(&["name", "label"])))
        .unwrap_or_else(|| artifact_id.to_string());
    let original_filename = string_value(Some(&pick_data(&[
        "originalFilename",
        "sf_originalFilename",
    ])))
    .unwrap_or_else(|| label.clone());
    let mut response = JsonMap::new();
    response.insert("entityType".into(), json!("artifact"));
    response.insert("id".into(), json!(artifact_id));
    response.insert("graphId".into(), json!(graph_id));
    response.insert("label".into(), json!(label));
    response.insert(
        "parentId".into(),
        opt_string_json(string_value(Some(&get("parentId")))),
    );
    response.insert(
        "order".into(),
        json_number(finite_number(Some(&get("order"))).unwrap_or(0.0)),
    );
    response.insert(
        "fileType".into(),
        json!(string_value(Some(&pick_data(&["fileType", "sf_fileType"])))
            .unwrap_or_else(|| artifact_file_type(&original_filename))),
    );
    response.insert(
        "status".into(),
        json!(string_value(Some(&get("status"))).unwrap_or_else(|| "uploading".to_string())),
    );
    response.insert(
        "errorMessage".into(),
        opt_string_json(string_value(Some(&get("errorMessage")))),
    );
    response.insert(
        "storageKey".into(),
        opt_string_json(string_value(Some(&pick_data(&[
            "storageKey",
            "sf_storageKey",
        ])))),
    );
    response.insert("originalFilename".into(), json!(original_filename));
    response.insert(
        "mimeType".into(),
        opt_string_json(string_value(Some(&pick_data(&["mimeType", "sf_mimeType"])))),
    );
    response.insert(
        "sizeBytes".into(),
        finite_number(Some(&pick_data(&["sizeBytes", "size", "sf_sizeBytes"])))
            .map(json_number)
            .unwrap_or(Value::Null),
    );
    response.insert(
        "ingestedDocId".into(),
        opt_string_json(string_value(Some(&get("ingestedDocId")))),
    );
    for key in ["mode", "storage", "sha256", "hashedAt", "source", "index"] {
        let value = get(key);
        if !value.is_null() {
            response.insert(key.into(), value);
        }
    }
    // recordValue: only include when it is an object (TS leaves it undefined
    // otherwise, which JSON.stringify omits).
    let scene_projection = get("sceneProjection");
    if scene_projection.is_object() {
        response.insert("sceneProjection".into(), scene_projection);
    }
    response.insert(
        "sceneProjectionText".into(),
        opt_string_json(string_value(Some(&get("sceneProjectionText")))),
    );
    response.insert(
        "sceneProjectedAt".into(),
        opt_string_json(string_value(Some(&get("sceneProjectedAt")))),
    );
    response.insert("createdAt".into(), get("createdAt"));
    response.insert("updatedAt".into(), get("updatedAt"));
    Value::Object(response)
}

// ─────────────────────────────────────────────────────────────────────────────
// Wires
// ─────────────────────────────────────────────────────────────────────────────

/// The predicate a new wire will store, or why it can't be one. Validates
/// exactly what is stored, the trimmed name (a stored " supports" would pass a
/// trimmed check and then be left out of RDF); absent means `isWiredTo`. Also
/// used by the MCP tool to check a whole batch before any wire is created.
/// A predicate a tool, the interface or a sync may write: a usable name, and
/// not one the server keeps to itself (dreamtOf).
pub(crate) fn validated_wire_predicate(raw: Option<String>) -> Result<String, String> {
    let predicate = validated_wire_predicate_name(raw)?;
    if crate::wire_predicates::WireFamily::of(&predicate).server_written() {
        return Err(format!(
            "{predicate} wires are written only by the dream cycle; choose a judgment predicate \
             (supports, contradicts, qualifies, flowsInto, relatedTo, …)"
        ));
    }
    Ok(predicate)
}

fn validated_wire_predicate_name(raw: Option<String>) -> Result<String, String> {
    let Some(raw) = raw else {
        return Ok("isWiredTo".to_string());
    };
    let predicate = raw.trim().to_string();
    let uri = crate::rdf_workspace_terms::wire_predicate_uri(&predicate);
    if predicate.is_empty() || !crate::rdf::is_writable_iri(&uri) {
        return Err(format!(
            "predicate {raw:?} is not a usable name; use one word (supports, contradicts, qualifies, \
             grounds, requires, enables, exemplifies, flowsInto, produces, intensifiesWith, contains, \
             precedes, consequenceOf, relatedTo) or a full IRI with no spaces"
        ));
    }
    Ok(predicate)
}

/// Port of createWorkspaceWire (native-local-runtime.ts:2027). Returns the
/// wire `data` object (camelCase) plus a `wireId` field; the caller builds the
/// hosted response from it.
fn create_workspace_wire_in_doc(
    txn: &mut TransactionMut<'_>,
    graph_dir: &Path,
    graph_id: &str,
    source_id: &str,
    value: &JsonMap<String, Value>,
) -> Result<Value, String> {
    create_wire_in_doc(txn, graph_dir, graph_id, source_id, value, false, false)
}

/// Regions on a wire to an artifact (step 3): parse `targetRegion`, refuse
/// it without an artifact target or for an artifact that does not exist,
/// read the region through the storage gate (owned, borrowed, or a
/// reference under the roots), and put the server's `fileHash` and the
/// normalized region on the payload. Returns the side-store record to
/// write after the commit. A client-supplied `fileHash` is dropped.
async fn prepare_wire_region(
    app: &AppHandle,
    graph_id: &str,
    value: &mut JsonMap<String, Value>,
) -> Result<Option<crate::artifact_regions::Capture>, String> {
    value.remove("fileHash");
    value.remove("file_hash");
    let region_value = pick(value, &["targetRegion", "target_region"])
        .filter(|region| !region.is_null())
        .cloned();
    let Some(region_value) = region_value else {
        return Ok(None);
    };
    let artifact_id = string_value(pick(value, &["targetArtifactId", "target_artifact_id"]))
        .ok_or_else(|| "workspace.createWire: targetRegion requires an artifact target".to_string())?;
    if let Some(target_graph) = string_value(pick(value, &["targetGraphId", "target_graph_id"])) {
        if target_graph != graph_id {
            return Err("workspace.createWire: a region names an artifact in this graph; targetGraphId must be this graph".to_string());
        }
    }
    let wire_id = string_value(pick(value, &["wireId", "wire_id", "id"]))
        .ok_or_else(|| "workspace.createWire: wireId is required".to_string())?;
    let region = crate::artifact_regions::Region::parse(&region_value)
        .map_err(|error| format!("workspace.createWire: targetRegion: {error}"))?;
    let record = crate::artifact_storage::artifact_record(app, graph_id, &artifact_id)?
        .ok_or_else(|| format!("workspace.createWire: artifact not found: {artifact_id}"))?;
    let worker_app = app.clone();
    let worker_graph = graph_id.to_string();
    let capture = tokio::task::spawn_blocking(move || {
        crate::artifact_regions::capture(&worker_app, &worker_graph, &wire_id, &artifact_id, &record, region)
    })
    .await
    .map_err(|error| format!("region capture worker failed: {error}"))?
    .map_err(|error| format!("workspace.createWire: targetRegion: {error}"))?;
    value.remove("target_region");
    value.insert("targetRegion".into(), capture.region.to_json());
    if let Some(hash) = capture.file_hash.as_deref() {
        value.insert("fileHash".into(), json!(hash));
    }
    Ok(Some(capture))
}

fn artifact_exists<T: ReadTxn>(txn: &T, artifact_id: &str) -> bool {
    txn.get_map("artifacts")
        .and_then(|artifacts| artifacts.get(txn, artifact_id))
        .is_some_and(|value| matches!(value, Out::YMap(_)))
}

/// `server` lets a server-written family (dreamtOf) through; only the
/// direct server paths below pass it, never the CRDT operation.
/// `artifact_must_exist` refuses a wire to an artifact the workspace does
/// not hold (the CRDT operation asks for it; a recovered replay does not,
/// since the artifact may have gone since).
fn create_wire_in_doc(
    txn: &mut TransactionMut<'_>,
    graph_dir: &Path,
    graph_id: &str,
    source_id: &str,
    value: &JsonMap<String, Value>,
    server: bool,
    artifact_must_exist: bool,
) -> Result<Value, String> {
    let target_document_id = string_value(pick(value, &["targetDocumentId", "target_document_id"]));
    let target_artifact_id = string_value(pick(value, &["targetArtifactId", "target_artifact_id"]));
    if target_document_id.is_some() == target_artifact_id.is_some() {
        return Err(
            "workspace.createWire: exactly one of targetDocumentId/target_document_id or targetArtifactId/target_artifact_id is required"
                .to_string(),
        );
    }
    let target_graph_id = string_value(pick(value, &["targetGraphId", "target_graph_id"]))
        .unwrap_or_else(|| graph_id.to_string());
    let wire_id = string_value(pick(value, &["wireId", "wire_id", "id"]))
        .ok_or_else(|| "workspace.createWire: wireId is required".to_string())?;
    let updated_at_ms = finite_number(value.get("updatedAt")).ok_or_else(|| {
        "workspace.createWire: updatedAt is required — normalize_payload_ids must inject it from enqueueTimestamp (A2 item 11)"
            .to_string()
    })?;
    let now = epoch_ms_to_iso(updated_at_ms);
    let raw_predicate = string_value(value.get("predicate"));
    let predicate = if server { validated_wire_predicate_name(raw_predicate) } else { validated_wire_predicate(raw_predicate) }
        .map_err(|error| format!("workspace.createWire: {error}"))?;
    let lifetime_ms = crate::wire_predicates::WireFamily::of(&predicate).lifetime_ms();
    let source_block_id = string_value(pick(value, &["sourceBlockId", "source_block_id"]));
    let target_block_id = string_value(pick(value, &["targetBlockId", "target_block_id"]));
    if target_artifact_id.is_some() && target_block_id.is_some() {
        return Err(
            "workspace.createWire: targetBlockId/target_block_id requires a document target, not an artifact target"
                .to_string(),
        );
    }
    let target_region = pick(value, &["targetRegion", "target_region"])
        .filter(|region| region.is_object())
        .cloned();
    if target_region.is_some() && target_artifact_id.is_none() {
        return Err("workspace.createWire: targetRegion requires an artifact target".to_string());
    }
    // An artifact lives in one graph's workspace; a wire to another
    // graph's artifact has nothing to check against and is refused.
    if artifact_must_exist && target_artifact_id.is_some() && target_graph_id != graph_id {
        return Err("workspace.createWire: an artifact target must be in this graph".to_string());
    }
    let source_title = lookup_document_title(&*txn, graph_dir, source_id);
    let source_snippet = document_snippet(graph_dir, source_id, source_block_id.as_deref());
    let (target_title, target_snippet) = if target_graph_id == graph_id {
        if let Some(target_document_id) = target_document_id.as_deref() {
            (
                lookup_document_title(&*txn, graph_dir, target_document_id),
                document_snippet(graph_dir, target_document_id, target_block_id.as_deref()),
            )
        } else if let Some(target_artifact_id) = target_artifact_id.as_deref() {
            // A wire to an artifact the workspace does not hold was written
            // without complaint before step 3 (the XOR above only checked
            // the kinds); now it is refused like a bad predicate.
            if artifact_must_exist && !artifact_exists(&*txn, target_artifact_id) {
                return Err(format!("workspace.createWire: artifact not found: {target_artifact_id}"));
            }
            (lookup_artifact_title(&*txn, target_artifact_id), None)
        } else {
            (None, None)
        }
    } else {
        (None, None)
    };

    let mut data = JsonMap::new();
    data.insert("sourceDocumentId".into(), json!(source_id));
    data.insert("sourceBlockId".into(), opt_string_json(source_block_id));
    data.insert(
        "sourceMarkId".into(),
        opt_string_json(string_value(pick(
            value,
            &["sourceMarkId", "source_mark_id"],
        ))),
    );
    data.insert("targetGraphId".into(), json!(target_graph_id));
    data.insert(
        "targetDocumentId".into(),
        opt_string_json(target_document_id),
    );
    data.insert(
        "targetArtifactId".into(),
        opt_string_json(target_artifact_id),
    );
    data.insert("targetBlockId".into(), opt_string_json(target_block_id));
    data.insert("targetRegion".into(), target_region.unwrap_or(Value::Null));
    data.insert(
        "fileHash".into(),
        opt_string_json(string_value(pick(value, &["fileHash", "file_hash"]))),
    );
    data.insert(
        "targetMarkId".into(),
        opt_string_json(string_value(pick(
            value,
            &["targetMarkId", "target_mark_id"],
        ))),
    );
    data.insert("predicate".into(), json!(predicate));
    data.insert(
        "bidirectional".into(),
        json!(value.get("bidirectional").map(js_truthy).unwrap_or(false)),
    );
    data.insert("sourceTitle".into(), opt_string_json(source_title));
    data.insert("sourceSnippet".into(), opt_string_json(source_snippet));
    data.insert("targetTitle".into(), opt_string_json(target_title));
    data.insert("targetSnippet".into(), opt_string_json(target_snippet));
    data.insert(
        "sceneGraphId".into(),
        opt_string_json(string_value(pick(
            value,
            &["sceneGraphId", "scene_graph_id"],
        ))),
    );
    data.insert(
        "sceneArtifactId".into(),
        opt_string_json(string_value(pick(
            value,
            &["sceneArtifactId", "scene_artifact_id"],
        ))),
    );
    data.insert(
        "sceneElementId".into(),
        opt_string_json(string_value(pick(
            value,
            &["sceneElementId", "scene_element_id"],
        ))),
    );
    data.insert(
        "sceneSourceElementId".into(),
        opt_string_json(string_value(pick(
            value,
            &["sceneSourceElementId", "scene_source_element_id"],
        ))),
    );
    data.insert(
        "sceneTargetElementId".into(),
        opt_string_json(string_value(pick(
            value,
            &["sceneTargetElementId", "scene_target_element_id"],
        ))),
    );
    data.insert(
        "sceneStableKey".into(),
        opt_string_json(string_value(pick(
            value,
            &["sceneStableKey", "scene_stable_key"],
        ))),
    );
    data.insert("snapshotAt".into(), json!(now));
    data.insert("createdAt".into(), json!(now));
    if let Some(lifetime) = lifetime_ms {
        data.insert("expiresAt".into(), json!(epoch_ms_to_iso(updated_at_ms + lifetime as f64)));
    }

    let wires = txn.get_or_insert_map("wires");
    let wire_map = wires.insert(txn, wire_id.as_str(), MapPrelim::default());
    for (key, wire_value) in &data {
        if wire_value.is_null() {
            continue; // TS skips null/undefined entries
        }
        wire_map.insert(txn, key.as_str(), json_to_any(wire_value));
    }

    data.insert("wireId".into(), json!(wire_id));
    Ok(Value::Object(data))
}

/// Port of refreshWorkspaceWire (native-local-runtime.ts:2081). Returns the
/// refreshed wireSnapshotData (camelCase).
fn refresh_workspace_wire_in_doc(
    txn: &mut TransactionMut<'_>,
    graph_dir: &Path,
    graph_id: &str,
    wire_id: &str,
    updated_at_ms: f64,
) -> Result<Value, String> {
    let wires = txn.get_or_insert_map("wires");
    let wire_map = match wires.get(&*txn, wire_id) {
        Some(Out::YMap(map)) => map,
        _ => return Err(format!("wire not found: {wire_id}")),
    };
    let source_document_id = map_string(&*txn, &wire_map, "sourceDocumentId");
    let target_graph_id =
        map_string(&*txn, &wire_map, "targetGraphId").unwrap_or_else(|| graph_id.to_string());
    let target_document_id = map_string(&*txn, &wire_map, "targetDocumentId");
    let target_artifact_id = map_string(&*txn, &wire_map, "targetArtifactId");
    let source_block_id = map_string(&*txn, &wire_map, "sourceBlockId");
    let target_block_id = map_string(&*txn, &wire_map, "targetBlockId");
    let (source_title, source_snippet) = match source_document_id.as_deref() {
        Some(source_id) => (
            lookup_document_title(&*txn, graph_dir, source_id),
            document_snippet(graph_dir, source_id, source_block_id.as_deref()),
        ),
        None => (None, None),
    };
    wire_map.insert(txn, "sourceTitle", opt_string_any(source_title));
    wire_map.insert(txn, "sourceSnippet", opt_string_any(source_snippet));
    if target_graph_id == graph_id {
        if let Some(target_id) = target_document_id.as_deref() {
            let target_title = lookup_document_title(&*txn, graph_dir, target_id);
            let target_snippet = document_snippet(graph_dir, target_id, target_block_id.as_deref());
            wire_map.insert(txn, "targetTitle", opt_string_any(target_title));
            wire_map.insert(txn, "targetSnippet", opt_string_any(target_snippet));
        } else if let Some(target_id) = target_artifact_id.as_deref() {
            let target_title = lookup_artifact_title(&*txn, target_id);
            wire_map.insert(txn, "targetTitle", opt_string_any(target_title));
            wire_map.insert(txn, "targetSnippet", opt_string_any(None));
        }
    }
    wire_map.insert(txn, "snapshotAt", epoch_ms_to_iso(updated_at_ms).as_str());
    Ok(wire_snapshot_data(&*txn, &wire_map))
}

/// Port of wireSnapshotData (native-local-runtime.ts:2007).
fn wire_snapshot_data<T: ReadTxn>(txn: &T, wire_map: &MapRef) -> Value {
    let mut data = JsonMap::new();
    for key in [
        "sourceDocumentId",
        "sourceBlockId",
        "sourceMarkId",
        "targetGraphId",
        "targetDocumentId",
        "targetArtifactId",
        "targetBlockId",
        "targetMarkId",
        "predicate",
        "bidirectional",
        "sourceTitle",
        "sourceSnippet",
        "targetTitle",
        "targetSnippet",
        "snapshotAt",
        "createdAt",
        "targetRegion",
        "fileHash",
    ] {
        data.insert(key.to_string(), map_value(txn, wire_map, key));
    }
    Value::Object(data)
}

/// Port of createHostedWireResponse (native-local-runtime.ts:1979).
fn create_hosted_wire_response(graph_id: &str, wire_id: &str, data: &Value) -> Value {
    let get = |key: &str| data.get(key).cloned().unwrap_or(Value::Null);
    let predicate =
        string_value(Some(&get("predicate"))).unwrap_or_else(|| "isWiredTo".to_string());
    let trimmed_predicate = {
        let trimmed = predicate.trim();
        if trimmed.is_empty() {
            "isWiredTo".to_string()
        } else {
            trimmed.to_string()
        }
    };
    json!({
        "id": wire_id,
        "source_graph_id": graph_id,
        "source_document_id": string_value(Some(&get("sourceDocumentId"))).unwrap_or_default(),
        "source_block_id": opt_string_json(string_value(Some(&get("sourceBlockId")))),
        "source_mark_id": opt_string_json(string_value(Some(&get("sourceMarkId")))),
        "target_graph_id": string_value(Some(&get("targetGraphId"))).unwrap_or_else(|| graph_id.to_string()),
        "target_document_id": opt_string_json(string_value(Some(&get("targetDocumentId")))),
        "target_artifact_id": opt_string_json(string_value(Some(&get("targetArtifactId")))),
        "target_block_id": opt_string_json(string_value(Some(&get("targetBlockId")))),
        "target_mark_id": opt_string_json(string_value(Some(&get("targetMarkId")))),
        "predicate": crate::rdf_workspace_terms::wire_predicate_uri(&trimmed_predicate),
        "predicate_label": crate::wire_predicates::predicate_label(&trimmed_predicate),
        "bidirectional": js_truthy(&get("bidirectional")),
        "target_region": get("targetRegion"),
        "file_hash": opt_string_json(string_value(Some(&get("fileHash")))),
        "target_title": opt_string_json(string_value(Some(&get("targetTitle")))),
        "target_snippet": opt_string_json(string_value(Some(&get("targetSnippet")))),
        "source_title": opt_string_json(string_value(Some(&get("sourceTitle")))),
        "source_snippet": opt_string_json(string_value(Some(&get("sourceSnippet")))),
        "snapshot_at": opt_string_json(
            string_value(Some(&get("snapshotAt"))).or_else(|| string_value(Some(&get("createdAt")))),
        ),
        "created_at": opt_string_json(string_value(Some(&get("createdAt")))),
    })
}

/// Port of documentTitle (native-local-runtime.ts:1958): workspace Y.Doc
/// entry title first (filesystem store equivalent), then the document.json
/// record on disk.
fn lookup_document_title<T: ReadTxn>(
    txn: &T,
    graph_dir: &Path,
    document_id: &str,
) -> Option<String> {
    if let Some(documents) = txn.get_map("documents") {
        if let Some(Out::YMap(document_map)) = documents.get(txn, document_id) {
            if let Some(title) = map_string(txn, &document_map, "title") {
                return Some(title);
            }
        }
    }
    let record = document_record_json(graph_dir, document_id)?;
    string_value(record.get("title"))
}

fn lookup_artifact_title<T: ReadTxn>(txn: &T, artifact_id: &str) -> Option<String> {
    let artifacts = txn.get_map("artifacts")?;
    let Out::YMap(artifact_map) = artifacts.get(txn, artifact_id)? else {
        return None;
    };
    map_string(txn, &artifact_map, "name")
}

/// Port of documentSnippet (native-local-runtime.ts:1964), reading the
/// document.json record (the runtime's in-memory record cache equivalent).
fn document_snippet(graph_dir: &Path, document_id: &str, block_id: Option<&str>) -> Option<String> {
    let record = document_record_json(graph_dir, document_id)?;
    let empty: Vec<Value> = Vec::new();
    let blocks = record
        .get("blocks")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    if let Some(block_id) = block_id {
        let block = blocks.iter().map(object_payload).find(|item| {
            string_value(pick(item, &["id", "blockId", "block_id"])).as_deref() == Some(block_id)
        });
        if let Some(block) = block {
            if let Some(content) = string_value(pick(&block, &["content", "text"])) {
                return Some(js_slice(&content, 240));
            }
        }
    }
    let first_block = blocks
        .iter()
        .map(object_payload)
        .find(|item| string_value(pick(item, &["content", "text"])).is_some());
    let fallback = first_block
        .and_then(|block| string_value(pick(&block, &["content", "text"])))
        .or_else(|| string_value(record.get("body")));
    fallback.map(|text| js_slice(&text, 240))
}

pub(crate) fn document_record_json(graph_dir: &Path, document_id: &str) -> Option<Value> {
    let manifest_path = graph_dir
        .join("documents")
        .join(document_id)
        .join("document.json");
    if !manifest_path.is_file() {
        return None;
    }
    crate::storage::read_json::<Value>(&manifest_path).ok()
}

// ─────────────────────────────────────────────────────────────────────────────
// Snapshot materialization (port of workspace-materialization.ts)
// ─────────────────────────────────────────────────────────────────────────────

const SOURCE_FILE_FIELDS: &[&str] = &[
    "sf_storageKey",
    "sf_originalFilename",
    "sf_mimeType",
    "sf_sizeBytes",
    "sf_fileType",
];

const WIRE_FIELDS: &[&str] = &[
    "sourceDocumentId",
    "sourceBlockId",
    "targetGraphId",
    "targetDocumentId",
    "targetArtifactId",
    "targetBlockId",
    "sourceMarkId",
    "targetMarkId",
    "predicate",
    "bidirectional",
    "inverseOf",
    "createdAt",
    "deletedAt",
    "_tombstonedAt",
    "sourceTitle",
    "targetTitle",
    "sourceSnippet",
    "targetSnippet",
    "sceneGraphId",
    "sceneArtifactId",
    "sceneElementId",
    "sceneSourceElementId",
    "sceneTargetElementId",
    "sceneStableKey",
    "targetRegion",
    "fileHash",
];

/// Port of materializeWorkspaceYDoc (workspace-materialization.ts:120).
pub fn materialize_workspace_snapshot_json(graph_id: &str, doc: &Doc) -> Result<Value, String> {
    let txn = doc.transact();
    let folders = read_folder_entities(&txn);
    let documents = read_document_entities(&txn);
    let artifacts = read_artifact_entities(&txn);
    let wires = read_wire_entities(&txn);
    let ui = read_ui(&txn);
    let active_wires = wires
        .iter()
        .filter(|wire| {
            !js_truthy(wire.get("deletedAt").unwrap_or(&Value::Null))
                && !js_truthy(wire.get("tombstonedAt").unwrap_or(&Value::Null))
        })
        .count();
    let tree_documents = build_tree("documents", &folders, &documents, &[])?;
    let tree_artifacts = build_tree("artifacts", &folders, &[], &artifacts)?;
    Ok(json!({
        "schemaVersion": 1,
        "graphId": graph_id,
        "materializedAt": crate::clock::epoch_millis() as u64,
        "folders": folders,
        "documents": documents,
        "artifacts": artifacts,
        "wires": wires,
        "ui": ui,
        "tree": {
            "documents": tree_documents,
            "artifacts": tree_artifacts,
        },
        "counts": {
            "folders": folders.len(),
            "documents": documents.len(),
            "artifacts": artifacts.len(),
            "wires": wires.len(),
            "activeWires": active_wires,
        },
    }))
}

fn root_map_entries<T: ReadTxn>(txn: &T, name: &str) -> Vec<(String, MapRef)> {
    let Some(map) = txn.get_map(name) else {
        return Vec::new();
    };
    let mut entries: Vec<(String, MapRef)> = map
        .iter(txn)
        .filter_map(|(key, out)| match out {
            Out::YMap(child) => Some((key.to_string(), child)),
            _ => None,
        })
        .collect();
    entries.sort_by(|(a, _), (b, _)| a.cmp(b));
    entries
}

fn read_folder_entities<T: ReadTxn>(txn: &T) -> Vec<Value> {
    let mut folders: Vec<Value> = Vec::new();
    for (folder_id, folder_map) in root_map_entries(txn, "folders") {
        let mut entity = JsonMap::new();
        entity.insert("id".into(), json!(folder_id));
        entity.insert("type".into(), json!("folder"));
        entity.insert(
            "name".into(),
            json!(map_string(txn, &folder_map, "name").unwrap_or_else(|| folder_id.clone())),
        );
        entity.insert(
            "parentId".into(),
            entity_id_value(&map_value(txn, &folder_map, "parentId")),
        );
        entity.insert(
            "section".into(),
            json!(section_value(&map_value(txn, &folder_map, "section"))),
        );
        entity.insert(
            "order".into(),
            json_number(map_number(txn, &folder_map, "order").unwrap_or(0.0)),
        );
        insert_extra_fields(
            txn,
            &folder_map,
            &mut entity,
            &["name", "parentId", "section", "order"],
        );
        folders.push(Value::Object(entity));
    }
    sort_entities(&mut folders);
    folders
}

fn read_document_entities<T: ReadTxn>(txn: &T) -> Vec<Value> {
    let known: Vec<&str> = [
        "title",
        "parentId",
        "section",
        "order",
        "createdAt",
        "updatedAt",
        "readOnly",
    ]
    .iter()
    .chain(SOURCE_FILE_FIELDS.iter())
    .copied()
    .collect();
    let mut documents: Vec<Value> = Vec::new();
    for (document_id, document_map) in root_map_entries(txn, "documents") {
        let mut entity = JsonMap::new();
        entity.insert("id".into(), json!(document_id));
        entity.insert("type".into(), json!("document"));
        entity.insert(
            "title".into(),
            json!(map_string(txn, &document_map, "title").unwrap_or_else(|| "Untitled".to_string())),
        );
        entity.insert(
            "parentId".into(),
            entity_id_value(&map_value(txn, &document_map, "parentId")),
        );
        entity.insert("section".into(), json!("documents"));
        entity.insert(
            "order".into(),
            json_number(map_number(txn, &document_map, "order").unwrap_or(0.0)),
        );
        entity.insert(
            "createdAt".into(),
            map_number(txn, &document_map, "createdAt")
                .map(json_number)
                .unwrap_or(Value::Null),
        );
        entity.insert(
            "updatedAt".into(),
            map_number(txn, &document_map, "updatedAt")
                .map(json_number)
                .unwrap_or(Value::Null),
        );
        entity.insert(
            "readOnly".into(),
            json!(js_truthy(&map_value(txn, &document_map, "readOnly"))),
        );
        insert_source_file(txn, &document_map, &mut entity);
        insert_extra_fields(txn, &document_map, &mut entity, &known);
        documents.push(Value::Object(entity));
    }
    sort_entities(&mut documents);
    documents
}

fn read_artifact_entities<T: ReadTxn>(txn: &T) -> Vec<Value> {
    let known: Vec<&str> = [
        "name",
        "parentId",
        "mimeType",
        "status",
        "errorMessage",
        "order",
        "size",
        "ingestedDocId",
        "sceneProjection",
        "sceneProjectionText",
        "sceneProjectedAt",
        "createdAt",
        "updatedAt",
        "fileType",
    ]
    .iter()
    .chain(SOURCE_FILE_FIELDS.iter())
    .copied()
    .collect();
    let mut artifacts: Vec<Value> = Vec::new();
    for (artifact_id, artifact_map) in root_map_entries(txn, "artifacts") {
        let mut entity = JsonMap::new();
        entity.insert("id".into(), json!(artifact_id));
        entity.insert("type".into(), json!("artifact"));
        entity.insert(
            "name".into(),
            json!(map_string(txn, &artifact_map, "name").unwrap_or_else(|| artifact_id.clone())),
        );
        entity.insert(
            "parentId".into(),
            entity_id_value(&map_value(txn, &artifact_map, "parentId")),
        );
        entity.insert("section".into(), json!("artifacts"));
        entity.insert(
            "order".into(),
            json_number(map_number(txn, &artifact_map, "order").unwrap_or(0.0)),
        );
        if let Some(file_type) = map_string(txn, &artifact_map, "fileType")
            .or_else(|| map_string(txn, &artifact_map, "sf_fileType"))
        {
            entity.insert("fileType".into(), json!(file_type));
        }
        entity.insert(
            "mimeType".into(),
            json!(map_string(txn, &artifact_map, "mimeType")
                .unwrap_or_else(|| "application/octet-stream".to_string())),
        );
        entity.insert(
            "status".into(),
            json!(map_string(txn, &artifact_map, "status").unwrap_or_else(|| "ready".to_string())),
        );
        entity.insert(
            "errorMessage".into(),
            opt_string_json(map_string(txn, &artifact_map, "errorMessage")),
        );
        if let Some(size) = map_number(txn, &artifact_map, "size") {
            entity.insert("size".into(), json_number(size));
        }
        if let Some(ingested_doc_id) = map_string(txn, &artifact_map, "ingestedDocId") {
            entity.insert("ingestedDocId".into(), json!(ingested_doc_id));
        }
        for key in ["mode", "storage", "sha256", "hashedAt"] {
            if let Some(value) = map_string(txn, &artifact_map, key) {
                entity.insert(key.into(), json!(value));
            }
        }
        for key in ["source", "index"] {
            let value = map_value(txn, &artifact_map, key);
            if value.is_object() {
                entity.insert(key.into(), value);
            }
        }
        let scene_projection = map_value(txn, &artifact_map, "sceneProjection");
        if scene_projection.is_object() {
            entity.insert("sceneProjection".into(), scene_projection);
        }
        entity.insert(
            "sceneProjectionText".into(),
            opt_string_json(map_string(txn, &artifact_map, "sceneProjectionText")),
        );
        entity.insert(
            "sceneProjectedAt".into(),
            string_or_number_value(&map_value(txn, &artifact_map, "sceneProjectedAt")),
        );
        entity.insert(
            "createdAt".into(),
            string_or_number_value(&map_value(txn, &artifact_map, "createdAt")),
        );
        entity.insert(
            "updatedAt".into(),
            string_or_number_value(&map_value(txn, &artifact_map, "updatedAt")),
        );
        insert_source_file(txn, &artifact_map, &mut entity);
        insert_extra_fields(txn, &artifact_map, &mut entity, &known);
        artifacts.push(Value::Object(entity));
    }
    sort_entities(&mut artifacts);
    artifacts
}

fn read_wire_entities<T: ReadTxn>(txn: &T) -> Vec<Value> {
    let mut wires: Vec<Value> = Vec::new();
    for (wire_id, wire_map) in root_map_entries(txn, "wires") {
        let mut wire = JsonMap::new();
        wire.insert("id".into(), json!(wire_id));
        wire.insert(
            "sourceDocumentId".into(),
            entity_id_value(&map_value(txn, &wire_map, "sourceDocumentId")),
        );
        wire.insert(
            "sourceBlockId".into(),
            entity_id_value(&map_value(txn, &wire_map, "sourceBlockId")),
        );
        wire.insert(
            "targetGraphId".into(),
            opt_string_json(map_string(txn, &wire_map, "targetGraphId")),
        );
        wire.insert(
            "targetDocumentId".into(),
            entity_id_value(&map_value(txn, &wire_map, "targetDocumentId")),
        );
        wire.insert(
            "targetArtifactId".into(),
            entity_id_value(&map_value(txn, &wire_map, "targetArtifactId")),
        );
        wire.insert(
            "targetBlockId".into(),
            entity_id_value(&map_value(txn, &wire_map, "targetBlockId")),
        );
        wire.insert(
            "sourceMarkId".into(),
            entity_id_value(&map_value(txn, &wire_map, "sourceMarkId")),
        );
        wire.insert(
            "targetMarkId".into(),
            entity_id_value(&map_value(txn, &wire_map, "targetMarkId")),
        );
        wire.insert(
            "predicate".into(),
            opt_string_json(map_string(txn, &wire_map, "predicate")),
        );
        wire.insert(
            "bidirectional".into(),
            json!(js_truthy(&map_value(txn, &wire_map, "bidirectional"))),
        );
        wire.insert(
            "inverseOf".into(),
            entity_id_value(&map_value(txn, &wire_map, "inverseOf")),
        );
        wire.insert(
            "createdAt".into(),
            string_or_number_value(&map_value(txn, &wire_map, "createdAt")),
        );
        wire.insert(
            "deletedAt".into(),
            string_or_number_value(&map_value(txn, &wire_map, "deletedAt")),
        );
        wire.insert(
            "tombstonedAt".into(),
            string_or_number_value(&map_value(txn, &wire_map, "_tombstonedAt")),
        );
        for key in [
            "sourceTitle",
            "targetTitle",
            "sourceSnippet",
            "targetSnippet",
        ] {
            wire.insert(
                key.to_string(),
                opt_string_json(map_string(txn, &wire_map, key)),
            );
        }
        wire.insert(
            "sceneGraphId".into(),
            opt_string_json(map_string(txn, &wire_map, "sceneGraphId")),
        );
        wire.insert(
            "sceneArtifactId".into(),
            entity_id_value(&map_value(txn, &wire_map, "sceneArtifactId")),
        );
        for key in [
            "sceneElementId",
            "sceneSourceElementId",
            "sceneTargetElementId",
            "sceneStableKey",
        ] {
            wire.insert(
                key.to_string(),
                opt_string_json(map_string(txn, &wire_map, key)),
            );
        }
        // Step 3: the region a wire names on an artifact and the file's
        // hash at wiring time. Public fields (neither is file text).
        let region = map_value(txn, &wire_map, "targetRegion");
        wire.insert(
            "targetRegion".into(),
            if region.is_object() { region } else { Value::Null },
        );
        wire.insert(
            "fileHash".into(),
            opt_string_json(map_string(txn, &wire_map, "fileHash")),
        );
        insert_extra_fields(txn, &wire_map, &mut wire, WIRE_FIELDS);
        wires.push(Value::Object(wire));
    }
    wires.sort_by(|left, right| {
        left.get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .cmp(right.get("id").and_then(Value::as_str).unwrap_or_default())
    });
    wires
}

fn read_ui<T: ReadTxn>(txn: &T) -> Value {
    let Some(ui) = txn.get_map("ui") else {
        return json!({ "expandedFolders": [], "dreamingEnabled": false });
    };
    let expanded = map_value(txn, &ui, "expandedFolders");
    let expanded_folders: Vec<String> = expanded
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|item| match item {
                    Value::String(text) => text.clone(),
                    Value::Null => "null".to_string(),
                    other => string_value(Some(other)).unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default();
    json!({
        "expandedFolders": expanded_folders,
        "dreamingEnabled": js_truthy(&map_value(txn, &ui, "dreamingEnabled")),
    })
}

fn insert_source_file<T: ReadTxn>(txn: &T, map: &MapRef, entity: &mut JsonMap<String, Value>) {
    let mut source_file = JsonMap::new();
    for field in SOURCE_FILE_FIELDS {
        let value = map_value(txn, map, field);
        if scalar_value(&value).is_some() {
            source_file.insert(field.to_string(), value);
        }
    }
    if !source_file.is_empty() {
        entity.insert("sourceFile".into(), Value::Object(source_file));
    }
}

fn insert_extra_fields<T: ReadTxn>(
    txn: &T,
    map: &MapRef,
    entity: &mut JsonMap<String, Value>,
    known: &[&str],
) {
    let mut extra = JsonMap::new();
    let mut keys: Vec<String> = map.keys(txn).map(str::to_string).collect();
    keys.sort();
    for key in keys {
        if known.contains(&key.as_str()) {
            continue;
        }
        let value = map_value(txn, map, &key);
        if scalar_value(&value).is_some() {
            extra.insert(key, value);
        }
    }
    if !extra.is_empty() {
        entity.insert("extra".into(), Value::Object(extra));
    }
}

/// Port of buildTree (workspace-materialization.ts:301).
fn validate_folder_parent_graph(folders: &[Value]) -> Result<(), String> {
    let folder_ids = folders
        .iter()
        .filter_map(|folder| folder.get("id").and_then(Value::as_str))
        .map(str::to_string)
        .collect::<HashSet<_>>();
    let mut children_by_parent = HashMap::<String, Vec<String>>::new();
    for folder in folders {
        let Some(folder_id) = folder.get("id").and_then(Value::as_str) else {
            continue;
        };
        let Some(parent_id) = folder
            .get("parentId")
            .and_then(Value::as_str)
            .filter(|parent_id| folder_ids.contains(*parent_id))
        else {
            continue;
        };
        children_by_parent
            .entry(parent_id.to_string())
            .or_default()
            .push(folder_id.to_string());
    }
    for children in children_by_parent.values_mut() {
        children.sort();
    }

    fn visit(
        folder_id: &str,
        children_by_parent: &HashMap<String, Vec<String>>,
        traversal: &mut FolderTraversalGuard,
    ) -> Result<(), String> {
        if !traversal.enter(folder_id)? {
            return Ok(());
        }
        for child_id in children_by_parent
            .get(folder_id)
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            visit(child_id, children_by_parent, traversal)?;
        }
        traversal.leave(folder_id);
        Ok(())
    }

    let mut ordered_ids = folder_ids.into_iter().collect::<Vec<_>>();
    ordered_ids.sort();
    let mut traversal = FolderTraversalGuard::default();
    for folder_id in ordered_ids {
        if !traversal.visited.contains(&folder_id) {
            visit(&folder_id, &children_by_parent, &mut traversal)?;
        }
    }
    Ok(())
}

fn build_tree(
    section: &str,
    folders: &[Value],
    documents: &[Value],
    artifacts: &[Value],
) -> Result<Vec<Value>, String> {
    // Validate the complete parent graph before selecting a section. Closed
    // or cross-section cycles have no visible root and would otherwise be
    // silently omitted from the snapshot that hard-cascade recovery trusts.
    validate_folder_parent_graph(folders)?;
    let section_folders: Vec<&Value> = folders
        .iter()
        .filter(|folder| folder.get("section").and_then(Value::as_str) == Some(section))
        .collect();
    let folder_ids: HashSet<&str> = section_folders
        .iter()
        .filter_map(|folder| folder.get("id").and_then(Value::as_str))
        .collect();
    let visible_parent = |entity: &Value| -> Option<String> {
        entity
            .get("parentId")
            .and_then(Value::as_str)
            .filter(|parent| folder_ids.contains(parent))
            .map(str::to_string)
    };

    let mut folders_by_parent: HashMap<Option<String>, Vec<Value>> = HashMap::new();
    for folder in &section_folders {
        folders_by_parent
            .entry(visible_parent(folder))
            .or_default()
            .push((*folder).clone());
    }
    let mut documents_by_parent: HashMap<Option<String>, Vec<Value>> = HashMap::new();
    if section == "documents" {
        for document in documents {
            documents_by_parent
                .entry(visible_parent(document))
                .or_default()
                .push(document.clone());
        }
    }
    let mut artifacts_by_parent: HashMap<Option<String>, Vec<Value>> = HashMap::new();
    if section == "artifacts" {
        for artifact in artifacts {
            artifacts_by_parent
                .entry(visible_parent(artifact))
                .or_default()
                .push(artifact.clone());
        }
    }

    fn folder_children(
        parent_id: &str,
        folders_by_parent: &HashMap<Option<String>, Vec<Value>>,
        documents_by_parent: &HashMap<Option<String>, Vec<Value>>,
        artifacts_by_parent: &HashMap<Option<String>, Vec<Value>>,
        traversal: &mut FolderTraversalGuard,
    ) -> Result<Vec<Value>, String> {
        if !traversal.enter(parent_id)? {
            return Ok(Vec::new());
        }
        let key = Some(parent_id.to_string());
        let mut children: Vec<Value> = Vec::new();
        for folder in folders_by_parent.get(&key).into_iter().flatten() {
            let mut node = folder.clone();
            let folder_id = node
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let nested = folder_children(
                &folder_id,
                folders_by_parent,
                documents_by_parent,
                artifacts_by_parent,
                traversal,
            )?;
            if let Some(object) = node.as_object_mut() {
                object.insert("children".into(), Value::Array(nested));
            }
            children.push(node);
        }
        for document in documents_by_parent.get(&key).into_iter().flatten() {
            children.push(document.clone());
        }
        for artifact in artifacts_by_parent.get(&key).into_iter().flatten() {
            children.push(artifact.clone());
        }
        sort_entities(&mut children);
        traversal.leave(parent_id);
        Ok(children)
    }

    let mut roots: Vec<Value> = Vec::new();
    let mut traversal = FolderTraversalGuard::default();
    for folder in folders_by_parent.get(&None).cloned().unwrap_or_default() {
        let mut node = folder.clone();
        let folder_id = node
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let nested = folder_children(
            &folder_id,
            &folders_by_parent,
            &documents_by_parent,
            &artifacts_by_parent,
            &mut traversal,
        )?;
        if let Some(object) = node.as_object_mut() {
            object.insert("children".into(), Value::Array(nested));
        }
        roots.push(node);
    }

    // Parent cycles have no root, so validate every disconnected component as
    // well as the visible roots. This shares the same active/visited contract
    // as destructive cascade collection and turns malformed persisted Y.Docs
    // into an ordinary error instead of unbounded recursion.
    let mut all_folder_ids = section_folders
        .iter()
        .filter_map(|folder| folder.get("id").and_then(Value::as_str))
        .map(str::to_string)
        .collect::<Vec<_>>();
    all_folder_ids.sort();
    for folder_id in all_folder_ids {
        if !traversal.visited.contains(&folder_id) {
            let _ = folder_children(
                &folder_id,
                &folders_by_parent,
                &documents_by_parent,
                &artifacts_by_parent,
                &mut traversal,
            )?;
        }
    }
    for document in documents_by_parent.get(&None).into_iter().flatten() {
        roots.push(document.clone());
    }
    for artifact in artifacts_by_parent.get(&None).into_iter().flatten() {
        roots.push(artifact.clone());
    }
    sort_entities(&mut roots);
    Ok(roots)
}

fn sort_entities(entities: &mut [Value]) {
    entities.sort_by(|left, right| {
        let left_order = left.get("order").and_then(Value::as_f64).unwrap_or(0.0);
        let right_order = right.get("order").and_then(Value::as_f64).unwrap_or(0.0);
        left_order
            .partial_cmp(&right_order)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                left.get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .cmp(right.get("id").and_then(Value::as_str).unwrap_or_default())
            })
    });
}

fn section_value(value: &Value) -> &'static str {
    if value.as_str() == Some("artifacts") {
        "artifacts"
    } else {
        "documents"
    }
}

/// Port of entityIdValue (workspace-materialization.ts:383):
/// `/#(?:block|mark)-([^:#]+)$/` then `/:(?:folder|artifact|doc|wire):([^:]+)$/`,
/// falling back to the raw string.
fn entity_id_value(value: &Value) -> Value {
    let Some(raw) = string_value(Some(value)) else {
        return Value::Null;
    };
    if let Some(hash_index) = raw.rfind('#') {
        let tail = &raw[hash_index + 1..];
        for prefix in ["block-", "mark-"] {
            if let Some(rest) = tail.strip_prefix(prefix) {
                if !rest.is_empty() && !rest.contains(':') && !rest.contains('#') {
                    return json!(rest);
                }
            }
        }
    }
    if let Some(colon_index) = raw.rfind(':') {
        let tail = &raw[colon_index + 1..];
        let head = &raw[..colon_index];
        if !tail.is_empty()
            && ["folder", "artifact", "doc", "wire"]
                .iter()
                .any(|kind| head.ends_with(&format!(":{kind}")))
        {
            return json!(tail);
        }
    }
    json!(raw)
}

fn scalar_value(value: &Value) -> Option<&Value> {
    match value {
        Value::String(_) | Value::Number(_) | Value::Bool(_) => Some(value),
        _ => None,
    }
}

fn string_or_number_value(value: &Value) -> Value {
    match value {
        Value::String(_) | Value::Number(_) => value.clone(),
        _ => Value::Null,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Payload helpers (ports of the TS coercion helpers)
// ─────────────────────────────────────────────────────────────────────────────

fn object_payload(value: &Value) -> JsonMap<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

fn string_payload(payload: &Value, key: &str) -> Option<String> {
    string_value(payload.as_object().and_then(|object| object.get(key)))
}

/// First key present with a non-null value (mirrors a JS `a ?? b ?? …` chain
/// over object properties).
fn pick<'a>(map: &'a JsonMap<String, Value>, keys: &[&str]) -> Option<&'a Value> {
    for key in keys {
        if let Some(value) = map.get(*key) {
            if !value.is_null() {
                return Some(value);
            }
        }
    }
    None
}

/// Port of stringValue/nullableStringValue: '' and null → None; scalars are
/// stringified with JS semantics. (Objects/arrays return None rather than
/// "[object Object]".)
pub(super) fn string_value(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::Null => None,
        Value::String(text) if text.is_empty() => None,
        Value::String(text) => Some(text.clone()),
        Value::Bool(flag) => Some(flag.to_string()),
        Value::Number(number) => Some(js_number_string(number)),
        _ => None,
    }
}

fn js_number_string(number: &serde_json::Number) -> String {
    if let Some(integer) = number.as_i64() {
        return integer.to_string();
    }
    if let Some(unsigned) = number.as_u64() {
        return unsigned.to_string();
    }
    let float = number.as_f64().unwrap_or(0.0);
    if float.fract() == 0.0 && float.abs() < 9.007_199_254_740_992e15 {
        (float as i64).to_string()
    } else {
        float.to_string()
    }
}

/// Port of finiteNumber: Number(value) when finite, else None.
fn finite_number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64().filter(|n| n.is_finite()),
        Value::String(text) if text.is_empty() => None,
        Value::String(text) => text.trim().parse::<f64>().ok().filter(|n| n.is_finite()),
        Value::Bool(flag) => Some(if *flag { 1.0 } else { 0.0 }),
        _ => None,
    }
}

/// Port of booleanValue.
fn boolean_value(value: Option<&Value>, fallback: bool) -> bool {
    match value {
        Some(Value::Bool(flag)) => *flag,
        Some(Value::String(text)) => match text.trim().to_lowercase().as_str() {
            "true" | "1" | "yes" => true,
            "false" | "0" | "no" => false,
            _ => fallback,
        },
        _ => fallback,
    }
}

/// Port of stringArrayValue.
fn string_array_value(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| string_value(Some(item)))
                .collect()
        })
        .unwrap_or_default()
}

/// JS truthiness for a JSON value.
fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().map(|n| n != 0.0).unwrap_or(false),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// `(stringValue(x) ?? fallback).trim() || fallback`
fn non_empty_or(value: Option<String>, fallback: &str) -> String {
    let text = value.unwrap_or_else(|| fallback.to_string());
    let trimmed = text.trim();
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
}

/// Port of artifactFileType (native-local-runtime.ts:3687).
pub(crate) fn artifact_file_type(filename: &str) -> String {
    let extension = filename
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    if !extension.is_empty() && extension != filename {
        extension
    } else {
        "unknown".to_string()
    }
}

fn opt_string_json(value: Option<String>) -> Value {
    value.map(Value::String).unwrap_or(Value::Null)
}

fn opt_string_any(value: Option<String>) -> Any {
    value
        .map(|text| Any::from(text.as_str()))
        .unwrap_or(Any::Null)
}

fn opt_number_any(value: Option<f64>) -> Any {
    value.map(Any::Number).unwrap_or(Any::Null)
}

/// Integral floats serialize as JSON integers (matching JSON.stringify).
fn json_number(number: f64) -> Value {
    if number.is_finite() && number.fract() == 0.0 && number.abs() < 9.007_199_254_740_992e15 {
        json!(number as i64)
    } else {
        json!(number)
    }
}

/// `text.slice(0, limit)` approximation (chars rather than UTF-16 units).
fn js_slice(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

/// Port of numericTimestamp (native-local-runtime.ts:74): Number(value) when
/// finite (the queue journals epoch-millis strings), else "now". The TS
/// Date.parse fallback is collapsed into the clock fallback — cells never
/// journal ISO strings.
pub(crate) fn numeric_timestamp(value: &str) -> f64 {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return 0.0; // Number('') === 0
    }
    if let Ok(numeric) = trimmed.parse::<f64>() {
        if numeric.is_finite() {
            return numeric;
        }
    }
    crate::clock::epoch_millis() as f64
}

/// `new Date(ms).toISOString()` for finite epoch-ms values.
fn epoch_ms_to_iso(ms: f64) -> String {
    let total_ms = ms.trunc() as i64;
    let days = total_ms.div_euclid(86_400_000);
    let day_ms = total_ms.rem_euclid(86_400_000);
    let (year, month, day) = civil_from_days(days);
    let millis = day_ms % 1_000;
    let total_seconds = day_ms / 1_000;
    let hours = total_seconds / 3_600;
    let minutes = (total_seconds % 3_600) / 60;
    let seconds = total_seconds % 60;
    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}.{millis:03}Z")
}

/// Days-from-epoch → (year, month, day); Howard Hinnant's civil_from_days.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as i64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

// ─────────────────────────────────────────────────────────────────────────────
// Y.Map read/write helpers
// ─────────────────────────────────────────────────────────────────────────────

/// TS: `let m = parent.get(k); if (!(m instanceof Y.Map)) { m = new Y.Map(); parent.set(k, m) }`
fn child_map(parent: &MapRef, txn: &mut TransactionMut<'_>, key: &str) -> MapRef {
    match parent.get(&*txn, key) {
        Some(Out::YMap(map)) => map,
        _ => parent.insert(txn, key, MapPrelim::default()),
    }
}

fn map_value<T: ReadTxn>(txn: &T, map: &MapRef, key: &str) -> Value {
    map.get(txn, key)
        .map(|out| out_to_json(txn, &out))
        .unwrap_or(Value::Null)
}

fn map_string<T: ReadTxn>(txn: &T, map: &MapRef, key: &str) -> Option<String> {
    string_value(Some(&map_value(txn, map, key)))
}

fn map_number<T: ReadTxn>(txn: &T, map: &MapRef, key: &str) -> Option<f64> {
    finite_number(Some(&map_value(txn, map, key)))
}

fn map_bool<T: ReadTxn>(txn: &T, map: &MapRef, key: &str, fallback: bool) -> bool {
    boolean_value(Some(&map_value(txn, map, key)), fallback)
}

fn out_to_json<T: ReadTxn>(txn: &T, out: &Out) -> Value {
    match out {
        Out::Any(any) => any_to_json(any),
        Out::YMap(map) => {
            let mut object = JsonMap::new();
            let entries: Vec<(String, Out)> = map
                .iter(txn)
                .map(|(key, value)| (key.to_string(), value))
                .collect();
            for (key, value) in entries {
                object.insert(key, out_to_json(txn, &value));
            }
            Value::Object(object)
        }
        _ => Value::Null,
    }
}

fn any_to_json(any: &Any) -> Value {
    match any {
        Any::Null | Any::Undefined => Value::Null,
        Any::Bool(flag) => json!(flag),
        Any::Number(number) => json_number(*number),
        Any::BigInt(number) => json!(number),
        Any::String(text) => json!(text.as_ref()),
        Any::Buffer(bytes) => json!(bytes.as_ref()),
        Any::Array(items) => Value::Array(items.iter().map(any_to_json).collect()),
        Any::Map(entries) => {
            let mut object = JsonMap::new();
            for (key, value) in entries.iter() {
                object.insert(key.clone(), any_to_json(value));
            }
            Value::Object(object)
        }
    }
}

fn json_to_any(value: &Value) -> Any {
    match value {
        Value::Null => Any::Null,
        Value::Bool(flag) => Any::Bool(*flag),
        Value::Number(number) => Any::Number(number.as_f64().unwrap_or(0.0)),
        Value::String(text) => Any::from(text.as_str()),
        Value::Array(items) => {
            let converted: Vec<Any> = items.iter().map(json_to_any).collect();
            Any::Array(Arc::from(converted))
        }
        Value::Object(entries) => {
            let converted: HashMap<String, Any> = entries
                .iter()
                .map(|(key, value)| (key.clone(), json_to_any(value)))
                .collect();
            Any::Map(Arc::new(converted))
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn with_doc<T>(doc: &Doc, mutate: impl FnOnce(&mut TransactionMut<'_>) -> T) -> T {
        let mut txn = doc.transact_mut();
        mutate(&mut txn)
    }

    #[test]
    fn cyclic_folder_graph_rejects_snapshot_and_delete_without_mutation() {
        let doc = Doc::new();
        with_doc(&doc, |txn| {
            write_workspace_folder(
                txn,
                &json!({
                    "folderId": "cycle-a",
                    "name": "Cycle A",
                    "parentId": "cycle-b",
                    "section": "documents",
                    "order": 1,
                    "updatedAt": 1,
                }),
            )?;
            write_workspace_folder(
                txn,
                &json!({
                    "folderId": "cycle-b",
                    "name": "Cycle B",
                    "parentId": "cycle-a",
                    "section": "artifacts",
                    "order": 2,
                    "updatedAt": 2,
                }),
            )?;
            Ok::<_, String>(())
        })
        .expect("seed raw cross-section cycle");

        let snapshot_error = materialize_workspace_snapshot_json("cycle-graph", &doc)
            .expect_err("closed parent cycle must not become an incomplete snapshot");
        assert!(snapshot_error.contains("cycle"), "{snapshot_error}");

        let delete_error = with_doc(&doc, |txn| {
            delete_workspace_folder_in_doc(txn, "cycle-a", true)
        })
        .expect_err("destructive traversal must reject the cycle");
        assert!(delete_error.contains("cycle"), "{delete_error}");

        let txn = doc.transact();
        let folders = txn.get_map("folders").expect("folders remain");
        assert!(folders.contains_key(&txn, "cycle-a"));
        assert!(folders.contains_key(&txn, "cycle-b"));
    }

    #[test]
    fn lifecycle_create_move_delete_materializes_snapshot() {
        let doc = Doc::new();

        // createFolder
        let folder = with_doc(&doc, |txn| {
            write_workspace_folder(
                txn,
                &json!({ "folderId": "folder-a", "name": "Research", "order": 1, "updatedAt": 1000 }),
            )
        })
        .expect("create folder");
        assert_eq!(folder["folderId"], "folder-a");
        assert_eq!(folder["section"], "documents");
        assert_eq!(folder["order"], 1);

        // createDocument inside the folder + one at root
        let created = with_doc(&doc, |txn| {
            write_workspace_document(
                txn,
                &json!({
                    "documentId": "doc-1",
                    "title": "  Notes  ",
                    "parentId": "folder-a",
                    "order": 2,
                    "updatedAt": 2000,
                }),
            )
        })
        .expect("create document");
        assert_eq!(
            created,
            json!({ "documentId": "doc-1", "id": "doc-1", "title": "Notes", "parentId": "folder-a", "order": 2 })
        );
        with_doc(&doc, |txn| {
            write_workspace_document(
                txn,
                &json!({ "documentId": "doc-2", "title": "Scratch", "parentId": null, "order": 5, "updatedAt": 2500 }),
            )
        })
        .expect("create second document");

        let snapshot = materialize_workspace_snapshot_json("graph-a", &doc)
            .expect("materialize workspace snapshot");
        assert_eq!(snapshot["schemaVersion"], 1);
        assert_eq!(snapshot["graphId"], "graph-a");
        assert_eq!(snapshot["counts"]["folders"], 1);
        assert_eq!(snapshot["counts"]["documents"], 2);
        let tree = snapshot["tree"]["documents"].as_array().expect("tree");
        assert_eq!(tree.len(), 2, "folder + root document at top level");
        assert_eq!(tree[0]["id"], "folder-a");
        assert_eq!(tree[0]["type"], "folder");
        let children = tree[0]["children"].as_array().expect("folder children");
        assert_eq!(children.len(), 1);
        assert_eq!(children[0]["id"], "doc-1");
        assert_eq!(children[0]["title"], "Notes");
        assert_eq!(children[0]["createdAt"], 2000);
        assert_eq!(children[0]["readOnly"], false);
        assert_eq!(tree[1]["id"], "doc-2");

        // moveDocuments: doc-2 into the folder
        let moved = with_doc(&doc, |txn| {
            move_workspace_documents(
                txn,
                &json!({
                    "documentIds": ["doc-2", "doc-missing"],
                    "parentId": "folder-a",
                    "order": 10,
                    "updatedAt": 3000,
                }),
            )
        })
        .expect("move documents");
        assert_eq!(moved["moved"], json!(["doc-2"]));
        assert_eq!(moved["missing"], json!(["doc-missing"]));
        assert_eq!(moved["parentId"], "folder-a");

        let snapshot = materialize_workspace_snapshot_json("graph-a", &doc)
            .expect("materialize workspace snapshot");
        let tree = snapshot["tree"]["documents"].as_array().expect("tree");
        assert_eq!(tree.len(), 1, "only the folder remains at the root");
        let children = tree[0]["children"].as_array().expect("folder children");
        assert_eq!(children.len(), 2);
        assert_eq!(children[0]["id"], "doc-1");
        assert_eq!(children[1]["id"], "doc-2");
        assert_eq!(children[1]["order"], 10);
        assert_eq!(children[1]["updatedAt"], 3000);

        // deleteFolder cascade
        let (existed, child_documents, child_artifacts, deleted_folders) = with_doc(&doc, |txn| {
            delete_workspace_folder_in_doc(txn, "folder-a", true)
        })
        .expect("delete folder");
        assert!(existed);
        assert_eq!(
            child_documents,
            vec!["doc-1".to_string(), "doc-2".to_string()]
        );
        assert!(child_artifacts.is_empty());
        assert!(deleted_folders.is_empty());

        let snapshot = materialize_workspace_snapshot_json("graph-a", &doc)
            .expect("materialize workspace snapshot");
        assert_eq!(snapshot["counts"]["folders"], 0);
        assert_eq!(snapshot["counts"]["documents"], 0);
        assert_eq!(snapshot["tree"]["documents"], json!([]));
        assert_eq!(
            snapshot["ui"],
            json!({ "expandedFolders": [], "dreamingEnabled": false })
        );
    }

    #[test]
    fn create_document_requires_injected_order_and_updated_at() {
        let doc = Doc::new();
        let error = with_doc(&doc, |txn| {
            write_workspace_document(txn, &json!({ "documentId": "doc-1", "updatedAt": 1 }))
        })
        .unwrap_err();
        assert_eq!(
            error,
            "workspace.createDocument: order is required — normalize_payload_ids must inject it from enqueueTimestamp (A2 item 10)"
        );
        let error = with_doc(&doc, |txn| {
            write_workspace_document(txn, &json!({ "documentId": "doc-1", "order": 1 }))
        })
        .unwrap_err();
        assert_eq!(
            error,
            "workspace.createDocument: updatedAt is required — normalize_payload_ids must inject it from enqueueTimestamp (A2 item 11)"
        );
        let error = with_doc(&doc, |txn| {
            write_workspace_document(txn, &json!({ "order": 1, "updatedAt": 1 }))
        })
        .unwrap_err();
        assert_eq!(error, "workspace.createDocument: documentId is required");
    }

    #[test]
    fn update_document_handles_partial_payloads() {
        let doc = Doc::new();
        with_doc(&doc, |txn| {
            write_workspace_document(
                txn,
                &json!({ "documentId": "doc-1", "title": "Original", "order": 1, "updatedAt": 1000 }),
            )
        })
        .expect("create");
        let error = with_doc(&doc, |txn| {
            update_workspace_document(txn, "doc-missing", &json!({ "updatedAt": 2000 }))
        })
        .unwrap_err();
        assert_eq!(error, "document not found: doc-missing");

        let updated = with_doc(&doc, |txn| {
            update_workspace_document(
                txn,
                "doc-1",
                &json!({ "title": "Renamed", "description": "About things", "updatedAt": 2000 }),
            )
        })
        .expect("update");
        assert_eq!(updated["title"], "Renamed");
        assert_eq!(updated["description"], "About things");
        assert_eq!(updated["describedAt"], 2000);
        assert_eq!(updated["updatedAt"], 2000);
        assert_eq!(updated["parentId"], Value::Null);
        assert_eq!(updated["order"], 1);
        assert_eq!(updated["readOnly"], false);
    }

    #[test]
    fn update_folder_rejects_self_descendant_and_unknown_parents() {
        let doc = Doc::new();
        with_doc(&doc, |txn| {
            write_workspace_folder(
                txn,
                &json!({ "folderId": "folder-a", "name": "A", "order": 1 }),
            )
        })
        .expect("folder a");
        with_doc(&doc, |txn| {
            write_workspace_folder(
                txn,
                &json!({ "folderId": "folder-b", "name": "B", "parentId": "folder-a", "order": 2 }),
            )
        })
        .expect("folder b");

        let error = with_doc(&doc, |txn| {
            update_workspace_folder(
                txn,
                "folder-a",
                &json!({ "newParentId": "folder-b", "updatedAt": 10 }),
            )
        })
        .unwrap_err();
        assert_eq!(
            error,
            "folder cannot be moved under itself or its descendant"
        );

        let error = with_doc(&doc, |txn| {
            update_workspace_folder(
                txn,
                "folder-b",
                &json!({ "newParentId": "folder-ghost", "updatedAt": 10 }),
            )
        })
        .unwrap_err();
        assert_eq!(error, "parent folder not found: folder-ghost");

        let error = with_doc(&doc, |txn| {
            update_workspace_folder(txn, "folder-ghost", &json!({ "updatedAt": 10 }))
        })
        .unwrap_err();
        assert_eq!(error, "folder not found: folder-ghost");

        let moved = with_doc(&doc, |txn| {
            update_workspace_folder(
                txn,
                "folder-b",
                &json!({ "newParentId": null, "newOrder": 7, "updatedAt": 20 }),
            )
        })
        .expect("move to root");
        assert_eq!(moved["parentId"], Value::Null);
        assert_eq!(moved["order"], 7);
        assert_eq!(moved["updatedAt"], 20);
    }

    #[test]
    fn delete_folder_requires_cascade_and_short_circuits_when_absent() {
        let doc = Doc::new();
        with_doc(&doc, |txn| {
            write_workspace_folder(
                txn,
                &json!({ "folderId": "folder-a", "name": "A", "order": 1 }),
            )
        })
        .expect("folder");
        with_doc(&doc, |txn| {
            write_workspace_document(
                txn,
                &json!({ "documentId": "doc-1", "title": "Doc", "parentId": "folder-a", "order": 2, "updatedAt": 1 }),
            )
        })
        .expect("doc");

        let error = with_doc(&doc, |txn| {
            delete_workspace_folder_in_doc(txn, "folder-a", false)
        })
        .unwrap_err();
        assert_eq!(
            error,
            "folder is not empty; pass cascade=true to delete children"
        );

        let result = with_doc(&doc, |txn| {
            delete_workspace_folder_in_doc(txn, "folder-ghost", false)
        })
        .expect("absent folder short-circuits");
        assert_eq!(result, (false, Vec::new(), Vec::new(), Vec::new()));
    }

    #[test]
    fn recovered_folder_snapshot_targets_are_ordered_and_path_safe() {
        let snapshot = json!({
            "graphId": "graph-a",
            "folders": [
                { "id": "folder-root", "parentId": null },
                { "id": "folder-b", "parentId": "folder-root" },
                { "id": "folder-a", "parentId": "folder-root" },
                { "id": "folder-deep", "parentId": "folder-a" }
            ],
            "documents": [
                { "id": "doc-root", "parentId": "folder-root" },
                { "id": "doc-deep", "parentId": "folder-deep" },
                { "id": "doc-a", "parentId": "folder-a" }
            ],
            "artifacts": [
                { "id": "artifact-b", "parentId": "folder-b" },
                { "id": "artifact-root", "parentId": "folder-root" }
            ]
        });
        let targets = folder_delete_targets_from_snapshot(&snapshot, "folder-root")
            .expect("derive nested targets")
            .expect("root exists");
        assert_eq!(targets.0, vec!["doc-deep", "doc-a", "doc-root"]);
        assert_eq!(targets.1, vec!["artifact-b", "artifact-root"]);
        assert_eq!(targets.2, vec!["folder-deep", "folder-a", "folder-b"]);

        let traversal = json!({
            "graphId": "graph-a",
            "folders": [{ "id": "folder-root", "parentId": null }],
            "documents": [{ "id": "../escape", "parentId": "folder-root" }],
            "artifacts": []
        });
        let targets = folder_delete_targets_from_snapshot(&traversal, "folder-root")
            .expect("a row whose id is not a local id is listed, not refused")
            .expect("root exists");
        // ...and never names a body to delete: it is a stray row.
        assert_eq!(
            split_stray_rows(targets.0),
            (Vec::<String>::new(), vec!["../escape".to_string()])
        );

        let cycle = json!({
            "graphId": "graph-a",
            "folders": [
                { "id": "folder-root", "parentId": "folder-child" },
                { "id": "folder-child", "parentId": "folder-root" }
            ],
            "documents": [],
            "artifacts": []
        });
        let error = folder_delete_targets_from_snapshot(&cycle, "folder-root")
            .expect_err("reachable folder cycle must be rejected");
        assert!(error.contains("cycle"), "{error}");
    }

    /// A client can write any key into the workspace map, so the snapshot a
    /// recovered hard cascade reads can hold rows whose id is not a local id,
    /// inside the folder or anywhere else. The plan used to refuse the whole
    /// snapshot, and after the cascade's hot commit that refusal was retried.
    /// Such rows are listed with the folder's documents; the caller removes
    /// them as rows and never deletes a body by them.
    #[test]
    fn recovered_folder_snapshot_lists_stray_rows_without_refusing() {
        let snapshot = json!({
            "graphId": "graph-a",
            "folders": [
                { "id": "folder-root", "parentId": null },
                { "id": "stray folder", "parentId": null }
            ],
            "documents": [
                { "id": "doc-root", "parentId": "folder-root" },
                { "id": "stray row", "parentId": "folder-root" },
                { "id": "stray outside", "parentId": null },
                { "id": "doc-in-stray", "parentId": "stray folder" }
            ],
            "artifacts": [{ "id": "stray artifact", "parentId": null }]
        });
        let targets = folder_delete_targets_from_snapshot(&snapshot, "folder-root")
            .expect("stray rows do not refuse the plan")
            .expect("root exists");
        assert_eq!(targets.0, vec!["doc-root", "stray row"]);
        assert!(targets.1.is_empty() && targets.2.is_empty());
    }

    #[test]
    fn move_documents_requires_injected_order_and_updated_at() {
        let doc = Doc::new();
        let error = with_doc(&doc, |txn| {
            move_workspace_documents(txn, &json!({ "documentIds": ["doc-1"], "updatedAt": 1 }))
        })
        .unwrap_err();
        assert_eq!(
            error,
            "workspace.moveDocuments: order is required — normalize_payload_ids must inject it from enqueueTimestamp (A2 item 10)"
        );
        let error = with_doc(&doc, |txn| {
            move_workspace_documents(txn, &json!({ "documentIds": ["doc-1"], "order": 1 }))
        })
        .unwrap_err();
        assert_eq!(
            error,
            "workspace.moveDocuments: updatedAt is required — normalize_payload_ids must inject it from enqueueTimestamp (A2 item 11)"
        );
    }

    #[test]
    fn vault_adoption_receipt_preserves_edits_and_deletion_with_real_mutator() {
        use crate::vault_adoption::{apply_once, Claim};
        let doc = Doc::new();
        let scan = "f7e73c3f-cc36-4e9e-9af6-e6a9df4fb654";
        let id = Claim::artifact_id("graph-a", "incarnation-a", scan, 0);
        let payload = json!({
            "artifactId": id, "label": "Original label", "originalFilename": "image.png",
            "mimeType": "image/png", "sizeBytes": 12, "sha256": "a".repeat(64),
            "mode": "attached", "storage": "owned", "status": "ready",
            "source": {"kind": "import", "importId": scan, "relativePath": "assets/image.png"},
            "order": 3, "updatedAt": 1_700_000_000_000i64
        });
        let claim = Claim::new("graph-a", "incarnation-a", scan, 0, &"b".repeat(64), &payload).unwrap();
        let original = object_payload(&payload);
        with_doc(&doc, |txn| {
            apply_once(txn, &claim, |txn| {
                put_workspace_artifact(txn, "graph-a", &id, &original).map(|_| ())
            })
        }).unwrap();
        let mut edited = original.clone();
        edited.insert("label".into(), json!("User's later label"));
        with_doc(&doc, |txn| put_workspace_artifact(txn, "graph-a", &id, &edited)).unwrap();
        let retry = || with_doc(&doc, |txn| {
            apply_once(txn, &claim, |txn| {
                put_workspace_artifact(txn, "graph-a", &id, &original).map(|_| ())
            })
        });
        assert!(retry().unwrap().replayed);
        let snapshot = materialize_workspace_snapshot_json("graph-a", &doc).unwrap();
        assert_eq!(snapshot["counts"]["artifacts"], 1);
        assert_eq!(snapshot["artifacts"][0]["name"], "User's later label");
        assert!(snapshot.get("_vaultArtifactReceipts").is_none());
        with_doc(&doc, |txn| delete_workspace_artifact_in_doc(txn, &id, false)).unwrap();
        assert!(retry().unwrap().replayed);
        let snapshot = materialize_workspace_snapshot_json("graph-a", &doc).unwrap();
        assert_eq!(snapshot["counts"]["artifacts"], 0);
    }

    #[test]
    fn put_artifact_returns_hosted_response_and_materializes() {
        let doc = Doc::new();
        let payload = object_payload(&json!({
            "label": "Paper.pdf",
            "originalFilename": "Paper.pdf",
            "mimeType": "application/pdf",
            "sizeBytes": 1234,
            "status": "ready",
            "order": 3,
            "updatedAt": 1_700_000_000_000i64,
        }));
        let response = with_doc(&doc, |txn| {
            put_workspace_artifact(txn, "graph-a", "artifact-1", &payload)
        })
        .expect("put artifact");
        assert_eq!(response["entityType"], "artifact");
        assert_eq!(response["id"], "artifact-1");
        assert_eq!(response["graphId"], "graph-a");
        assert_eq!(response["label"], "Paper.pdf");
        assert_eq!(response["fileType"], "pdf");
        assert_eq!(response["status"], "ready");
        assert_eq!(response["sizeBytes"], 1234);
        assert_eq!(response["mimeType"], "application/pdf");
        assert_eq!(response["createdAt"], "2023-11-14T22:13:20.000Z");
        assert_eq!(response["updatedAt"], "2023-11-14T22:13:20.000Z");
        assert!(response.get("sceneProjection").is_none());

        let snapshot = materialize_workspace_snapshot_json("graph-a", &doc)
            .expect("materialize workspace snapshot");
        assert_eq!(snapshot["counts"]["artifacts"], 1);
        assert_eq!(snapshot["artifacts"][0]["id"], "artifact-1");
        assert_eq!(snapshot["artifacts"][0]["fileType"], "pdf");
        assert_eq!(snapshot["artifacts"][0]["sourceFile"]["sf_fileType"], "pdf");
        assert_eq!(snapshot["tree"]["artifacts"][0]["id"], "artifact-1");

        with_doc(&doc, |txn| {
            delete_workspace_artifact_in_doc(txn, "artifact-1", false)
        })
        .expect("delete artifact");
        let error = with_doc(&doc, |txn| {
            delete_workspace_artifact_in_doc(txn, "artifact-1", false)
        })
        .unwrap_err();
        assert_eq!(error, "artifact not found: artifact-1");
    }

    #[test]
    fn an_attached_artifact_keeps_its_mode_hash_and_index_and_takes_its_wires_when_deleted() {
        let doc = Doc::new();
        let payload = object_payload(&json!({
            "label": "export.zip",
            "originalFilename": "export.zip",
            "mimeType": "application/zip",
            "sizeBytes": 400_000_000u64,
            "status": "ready",
            "order": 1,
            "updatedAt": 1_700_000_000_000i64,
            "mode": "attach",
            "sha256": "ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            "hashedAt": "1700000000000",
            "source": { "kind": "upload" },
            "index": { "version": 1, "title": "export", "members": 2, "toc": [{ "label": "a" }] },
        }));
        let response = with_doc(&doc, |txn| {
            put_workspace_artifact(txn, "graph-a", "artifact-z", &payload)
        })
        .expect("put attached artifact");
        assert_eq!(response["mode"], "attached");
        assert_eq!(response["sha256"], "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789");
        assert_eq!(response["source"]["kind"], "upload");
        assert_eq!(response["index"]["members"], 2);
        assert_eq!(response["index"]["toc"][0]["label"], "a");

        // A fourth mode is refused by name.
        let bad = object_payload(&json!({
            "label": "x", "order": 1, "updatedAt": 1_700_000_000_000i64, "mode": "linked",
        }));
        let error = with_doc(&doc, |txn| put_workspace_artifact(txn, "graph-a", "artifact-bad", &bad))
            .expect_err("unknown mode");
        assert!(error.contains("mode must be one of"), "{error}");
        // The back doors are shut: no asserted reference, no short hash, no
        // path on the record, no oversized index.
        for (payload, needle) in [
            (json!({ "mode": "referenced" }), "together"),
            (json!({ "storage": "external" }), "together"),
            (json!({ "sha256": "abc" }), "64 lowercase hex"),
            (json!({ "source": { "kind": "path", "path": "/etc/passwd" } }), "together"),
            (json!({ "source": { "kind": "url", "url": "https://x" } }), "source.kind"),
            (json!({ "index": { "pad": "x".repeat(ARTIFACT_INDEX_BYTES_CAP + 1) } }), "the cap is"),
        ] {
            let mut bad = object_payload(&json!({
                "label": "x", "order": 1, "updatedAt": 1_700_000_000_000i64,
            }));
            for (k, v) in payload.as_object().unwrap() {
                bad.insert(k.clone(), v.clone());
            }
            let error = with_doc(&doc, |txn| put_workspace_artifact(txn, "graph-a", "artifact-bad", &bad))
                .expect_err("refused");
            assert!(error.contains(needle), "{error}");
        }
        {
            let txn = doc.transact();
            let artifacts = txn.get_map("artifacts").expect("artifacts");
            assert!(
                artifacts.get(&txn, "artifact-bad").is_none(),
                "a refused put leaves no stub record"
            );
        }

        // A later put without the fields leaves them in place.
        let touch = object_payload(&json!({
            "label": "export.zip", "order": 1, "updatedAt": 1_700_000_001_000i64, "status": "ready",
        }));
        let response = with_doc(&doc, |txn| {
            put_workspace_artifact(txn, "graph-a", "artifact-z", &touch)
        })
        .expect("touch artifact");
        assert_eq!(response["mode"], "attached");
        assert_eq!(response["sha256"], "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789");

        let snapshot = materialize_workspace_snapshot_json("graph-a", &doc)
            .expect("materialize workspace snapshot");
        let artifact = &snapshot["artifacts"][0];
        assert_eq!(artifact["mode"], "attached");
        assert_eq!(artifact["sha256"], "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789");
        assert_eq!(artifact["hashedAt"], "1700000000000");
        assert_eq!(artifact["source"]["kind"], "upload");
        assert_eq!(artifact["index"]["title"], "export");

        // Two wires at the artifact (a target and a scene), one elsewhere.
        with_doc(&doc, |txn| {
            let wires = txn.get_or_insert_map("wires");
            for (wire_id, key, target) in [
                ("w-target", "targetArtifactId", "artifact-z"),
                ("w-scene", "sceneArtifactId", "artifact-z"),
                ("w-other", "targetArtifactId", "artifact-other"),
            ] {
                let wire = wires.insert(txn, wire_id, MapPrelim::default());
                wire.insert(txn, "sourceDocumentId", "doc-src");
                wire.insert(txn, "predicate", "annotates");
                wire.insert(txn, key, target);
            }
        });
        with_doc(&doc, |txn| delete_workspace_artifact_in_doc(txn, "artifact-z", false))
            .expect("delete artifact");
        let remaining: Vec<String> = {
            let txn = doc.transact();
            let wires = txn.get_map("wires").expect("wires");
            let mut ids: Vec<String> = wires.iter(&txn).map(|(id, _)| id.to_string()).collect();
            ids.sort();
            ids
        };
        assert_eq!(remaining, vec!["w-other".to_string()]);
    }

    #[test]
    fn a_borrowed_record_goes_with_its_document_and_an_owned_one_drops_to_attached() {
        let doc = Doc::new();
        let put = |doc: &Doc, id: &str, extra: Value| {
            let mut payload = object_payload(&json!({
                "label": format!("{id}.pdf"), "order": 1, "updatedAt": 1_700_000_000_000i64,
                "status": "ready",
            }));
            for (k, v) in extra.as_object().unwrap() {
                payload.insert(k.clone(), v.clone());
            }
            with_doc(doc, |txn| put_workspace_artifact(txn, "graph-a", id, &payload))
        };
        // A third storage word is refused, and leaves no stub.
        let error = put(&doc, "art-bad", json!({ "storage": "rented" })).expect_err("refused");
        assert!(error.contains("storage must be one of"), "{error}");
        // The borrow invariant: the name, the document, and no re-pointing.
        let error = put(&doc, "art-x", json!({ "storage": "borrowed", "ingestedDocId": "doc-a" })).expect_err("name");
        assert!(error.contains("must be named original-doc-a"), "{error}");
        let error = put(&doc, "original-doc-a", json!({ "storage": "borrowed", "ingestedDocId": "doc-a" })).expect_err("no document yet");
        assert!(error.contains("not in the workspace"), "{error}");
        let error = put(&doc, "original-doc-a", json!({ "storage": "borrowed" })).expect_err("no document named");
        assert!(error.contains("needs ingestedDocId"), "{error}");
        let error = put(&doc, "original-doc-a", json!({ "storage": "owned" })).expect_err("prefix reserved");
        assert!(error.contains("borrowed record's name"), "{error}");
        {
            let txn = doc.transact();
            assert!(txn.get_map("artifacts").expect("artifacts").get(&txn, "art-bad").is_none());
        }
        // doc-a kept its original: a borrowed record points at it.
        with_doc(&doc, |txn| {
            write_workspace_document(
                txn,
                &json!({
                    "documentId": "doc-a", "title": "Dropped", "order": 1,
                    "updatedAt": 1_700_000_000_000i64,
                    "sourceFile": { "storageKey": "local://documents/doc-a/original/dropped.pdf",
                                    "originalFilename": "dropped.pdf", "mimeType": "application/pdf",
                                    "sizeBytes": 10, "fileType": "pdf" },
                }),
            )
        })
        .expect("doc-a");
        let borrowed = put(
            &doc,
            "original-doc-a",
            json!({ "mode": "digested", "storage": "borrowed", "ingestedDocId": "doc-a",
                    "storageKey": "local://documents/doc-a/original/dropped.pdf" }),
        )
        .expect("borrowed record");
        assert_eq!(borrowed["storage"], "borrowed");
        assert_eq!(borrowed["mode"], "digested");
        // Once borrowed: a later put cannot clear the link, re-point it, or own it.
        let touched = put(&doc, "original-doc-a", json!({ "ingestedDocId": null, "status": "ready" })).expect("a null link is ignored");
        assert_eq!(touched["ingestedDocId"], "doc-a");
        assert_eq!(touched["storage"], "borrowed");
        let error = put(&doc, "original-doc-a", json!({ "ingestedDocId": "doc-z" })).expect_err("re-point");
        assert!(error.contains("cannot be re-pointed"), "{error}");
        let error = put(&doc, "original-doc-a", json!({ "storage": "owned" })).expect_err("un-borrow");
        assert!(error.contains("cannot become owned"), "{error}");
        // doc-b was digested from an OWNED record (ingest_artifact).
        with_doc(&doc, |txn| {
            write_workspace_document(
                txn,
                &json!({
                    "documentId": "doc-b", "title": "Digested", "order": 2,
                    "updatedAt": 1_700_000_000_000i64,
                    "sourceFile": { "storageKey": "local://artifacts/art-owned/original/paper.pdf",
                                    "originalFilename": "paper.pdf", "mimeType": "application/pdf",
                                    "sizeBytes": 10, "fileType": "pdf" },
                }),
            )
        })
        .expect("doc-b");
        put(
            &doc,
            "art-owned",
            json!({ "mode": "digested", "storage": "owned", "ingestedDocId": "doc-b",
                    "status": "ingested",
                    "storageKey": "local://artifacts/art-owned/original/paper.pdf" }),
        )
        .expect("owned record");
        with_doc(&doc, |txn| {
            let wires = txn.get_or_insert_map("wires");
            for (wire_id, target) in [("w-borrowed", "original-doc-a"), ("w-owned", "art-owned")] {
                let wire = wires.insert(txn, wire_id, MapPrelim::default());
                wire.insert(txn, "sourceDocumentId", "doc-src");
                wire.insert(txn, "predicate", "annotates");
                wire.insert(txn, "targetArtifactId", target);
            }
        });
        let snapshot = materialize_workspace_snapshot_json("graph-a", &doc).expect("snapshot");
        let storages: Vec<(String, String)> = snapshot["artifacts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| (a["id"].as_str().unwrap().to_string(), a["storage"].as_str().unwrap().to_string()))
            .collect();
        assert!(storages.contains(&("original-doc-a".to_string(), "borrowed".to_string())));
        assert!(storages.contains(&("art-owned".to_string(), "owned".to_string())));

        // Deleting doc-a removes its borrowed record and that record's wire.
        let removed = with_doc(&doc, |txn| {
            txn.get_or_insert_map("documents").remove(txn, "doc-a");
            cascade_document_delete_to_artifacts(txn, "doc-a")
        });
        assert_eq!(removed, vec!["original-doc-a".to_string()]);
        // Deleting doc-b leaves the owned record, now attached, with no digester.
        let removed = with_doc(&doc, |txn| {
            txn.get_or_insert_map("documents").remove(txn, "doc-b");
            cascade_document_delete_to_artifacts(txn, "doc-b")
        });
        assert!(removed.is_empty());
        let snapshot = materialize_workspace_snapshot_json("graph-a", &doc).expect("snapshot");
        let artifacts = snapshot["artifacts"].as_array().unwrap();
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0]["id"], "art-owned");
        assert_eq!(artifacts[0]["mode"], "attached");
        assert_eq!(artifacts[0]["status"], "ready");
        assert!(artifacts[0].get("ingestedDocId").is_none());
        let wires: Vec<String> = {
            let txn = doc.transact();
            let mut ids: Vec<String> = txn.get_map("wires").expect("wires").iter(&txn).map(|(id, _)| id.to_string()).collect();
            ids.sort();
            ids
        };
        assert_eq!(wires, vec!["w-owned".to_string()]);

        // A moved document takes its borrowed record along, and a folder
        // never counts a borrowed record as a child.
        with_doc(&doc, |txn| {
            write_workspace_document(
                txn,
                &json!({ "documentId": "doc-m", "title": "Moved", "order": 4, "parentId": "f-1",
                         "updatedAt": 1_700_000_000_000i64,
                         "sourceFile": { "storageKey": "local://documents/doc-m/original/m.pdf" } }),
            )
        })
        .expect("doc-m");
        put(&doc, "original-doc-m", json!({ "storage": "borrowed", "ingestedDocId": "doc-m", "parentId": "f-1" })).expect("borrowed m");
        with_doc(&doc, |txn| {
            move_workspace_documents(txn, &json!({ "documentIds": ["doc-m"], "parentId": "f-2", "order": 9, "updatedAt": 1_700_000_002_000i64 }))
        })
        .expect("move");
        let snapshot = materialize_workspace_snapshot_json("graph-a", &doc).expect("snapshot");
        let record = snapshot["artifacts"].as_array().unwrap().iter().find(|a| a["id"] == "original-doc-m").unwrap();
        assert_eq!(record["parentId"], "f-2");
        with_doc(&doc, |txn| {
            let folders = txn.get_or_insert_map("folders");
            let folder = folders.insert(txn, "f-1", MapPrelim::default());
            folder.insert(txn, "name", "Empty");
            put_workspace_artifact(txn, "graph-a", "original-doc-m", &object_payload(&json!({
                "order": 1, "updatedAt": 1_700_000_003_000i64, "parentId": "f-1" }))).expect("drift the record back");
            delete_workspace_folder_in_doc(txn, "f-1", false)
        })
        .expect("a folder holding only a drifted borrowed record is empty");
        assert!(with_doc(&doc, |txn| txn.get_or_insert_map("artifacts").contains_key(&*txn, "original-doc-m")));

        // The other direction: an owned digester deleted under its document
        // clears the document's sf_* pointers; a borrowed one does not touch
        // the document.
        with_doc(&doc, |txn| {
            write_workspace_document(
                txn,
                &json!({
                    "documentId": "doc-c", "title": "Digested again", "order": 3,
                    "updatedAt": 1_700_000_000_000i64,
                    "sourceFile": { "storageKey": "local://artifacts/art-owned/original/paper.pdf",
                                    "originalFilename": "paper.pdf", "mimeType": "application/pdf",
                                    "sizeBytes": 10, "fileType": "pdf" },
                }),
            )
        })
        .expect("doc-c");
        // The loopback ingest leaves ingestedDocId null; the pointer is the authority.
        put(&doc, "art-owned", json!({ "ingestedDocId": null, "mode": "digested" })).expect("re-digest without link");
        with_doc(&doc, |txn| delete_workspace_artifact_in_doc(txn, "art-owned", false)).expect("delete owned");
        let snapshot = materialize_workspace_snapshot_json("graph-a", &doc).expect("snapshot");
        let doc_c = snapshot["documents"].as_array().unwrap().iter().find(|d| d["id"] == "doc-c").unwrap();
        assert!(doc_c.get("sourceFile").is_none(), "{doc_c}");
    }

    #[test]
    fn a_wire_predicate_that_cannot_be_an_iri_is_refused_at_creation() {
        let doc = Doc::new();
        let temp_dir = std::env::temp_dir().join(format!("sophia-wire-{}", Uuid::new_v4()));
        let payload = object_payload(&json!({
            "wireId": "w-bad",
            "targetDocumentId": "doc-target",
            "predicate": "draws on",
            "updatedAt": 1_700_000_000_000i64,
        }));
        let error = with_doc(&doc, |txn| {
            create_workspace_wire_in_doc(txn, &temp_dir, "graph-a", "doc-src", &payload)
        })
        .expect_err("a predicate with a space is refused");
        assert!(error.contains("not a usable name"), "{error}");

        // What the store's parser refuses is refused here too (angel review F1),
        // and a blank predicate is not quietly the bare namespace (F2).
        for bad in ["100%", "see#2", "a[1]", "   "] {
            let payload = object_payload(&json!({
                "wireId": "w-bad", "targetDocumentId": "doc-target",
                "predicate": bad, "updatedAt": 1_700_000_000_000i64,
            }));
            let refused = with_doc(&doc, |txn| {
                create_workspace_wire_in_doc(txn, &temp_dir, "graph-a", "doc-src", &payload)
            });
            assert!(refused.is_err(), "{bad:?} should be refused");
        }

        // The stored predicate is the one that was checked: trimmed.
        let payload = object_payload(&json!({
            "wireId": "w-padded", "targetDocumentId": "doc-target",
            "predicate": " supports ", "updatedAt": 1_700_000_000_000i64,
        }));
        let data = with_doc(&doc, |txn| {
            create_workspace_wire_in_doc(txn, &temp_dir, "graph-a", "doc-src", &payload)
        })
        .expect("a padded known predicate is accepted");
        assert_eq!(data["predicate"], "supports", "{data}");
    }

    #[test]
    fn only_the_server_writes_dream_wires_and_they_carry_their_expiry() {
        let doc = Doc::new();
        let temp_dir = std::env::temp_dir().join(format!("sophia-wire-{}", Uuid::new_v4()));
        let written_at = 1_700_000_000_000i64;
        let payload = |id: &str, predicate: &str| object_payload(&json!({
            "wireId": id, "targetDocumentId": "doc-target", "targetBlockId": "block-1",
            "predicate": predicate, "updatedAt": written_at,
        }));
        // The CRDT operation (tools, the interface, sync) refuses dreamtOf,
        // in any spelling the family reads.
        for spelling in ["dreamtOf", "http://mnemosyne.ai/vocab#dreamtOf"] {
            let error = with_doc(&doc, |txn| {
                create_workspace_wire_in_doc(txn, &temp_dir, "graph-a", "doc-src", &payload("w-tool", spelling))
            })
            .expect_err("a tool can't write a dream wire");
            assert!(error.contains("only by the dream cycle"), "{error}");
        }
        assert!(validated_wire_predicate(Some("dreamtOf".into())).is_err());
        // Record wires are ordinary to write.
        with_doc(&doc, |txn| create_workspace_wire_in_doc(txn, &temp_dir, "graph-a", "doc-src", &payload("w-cites", "cites")))
            .expect("a record wire is written like any other");

        let data = with_doc(&doc, |txn| create_wire_in_doc(txn, &temp_dir, "graph-a", "doc-src", &payload("w-dream", "dreamtOf"), true, false))
            .expect("the server writes a dream wire");
        let expected = epoch_ms_to_iso((written_at + crate::wire_predicates::DREAM_WIRE_LIFETIME_MS) as f64);
        assert_eq!(data["expiresAt"], expected);

        let snapshot = materialize_workspace_snapshot_json("graph-a", &doc).expect("materialize");
        let wires = snapshot["wires"].as_array().expect("wires");
        let dream = wires.iter().find(|w| w["id"] == "w-dream").expect("dream wire materialized");
        // The snapshot keeps it under `extra`, and every reader sees it there:
        // written in 2023, it expired long ago, so it reads as gone.
        assert_eq!(dream["extra"]["expiresAt"], expected);
        assert_eq!(crate::wire_state::wire_expires_at_ms(dream), Some(written_at + crate::wire_predicates::DREAM_WIRE_LIFETIME_MS));
        assert!(!crate::wire_state::wire_is_active(dream));
        let cites = wires.iter().find(|w| w["id"] == "w-cites").expect("record wire materialized");
        assert!(crate::wire_state::wire_is_active(cites));
        assert!(!crate::wire_state::wire_counts_toward_salience(cites));
    }

    #[test]
    fn wire_create_refresh_delete_roundtrip() {
        let doc = Doc::new();
        let temp_dir = std::env::temp_dir().join(format!("sophia-wire-{}", Uuid::new_v4()));
        with_doc(&doc, |txn| {
            write_workspace_document(
                txn,
                &json!({ "documentId": "doc-src", "title": "Source", "order": 1, "updatedAt": 1 }),
            )
        })
        .expect("source doc");

        let payload = object_payload(&json!({
            "wireId": "w-1",
            "targetDocumentId": "doc-target",
            "predicate": "supports",
            "updatedAt": 1_700_000_000_000i64,
        }));
        let data = with_doc(&doc, |txn| {
            create_workspace_wire_in_doc(txn, &temp_dir, "graph-a", "doc-src", &payload)
        })
        .expect("create wire");
        let response = create_hosted_wire_response("graph-a", "w-1", &data);
        assert_eq!(response["id"], "w-1");
        assert_eq!(response["source_graph_id"], "graph-a");
        assert_eq!(response["source_document_id"], "doc-src");
        assert_eq!(response["target_document_id"], "doc-target");
        assert_eq!(response["target_graph_id"], "graph-a");
        assert_eq!(response["predicate"], "http://mnemosyne.ai/vocab#supports");
        assert_eq!(response["predicate_label"], "supports");
        assert_eq!(response["bidirectional"], false);
        assert_eq!(response["source_title"], "Source");
        assert_eq!(response["created_at"], "2023-11-14T22:13:20.000Z");

        let refreshed = with_doc(&doc, |txn| {
            refresh_workspace_wire_in_doc(txn, &temp_dir, "graph-a", "w-1", 1_700_000_060_000.0)
        })
        .expect("refresh wire");
        let response = create_hosted_wire_response("graph-a", "w-1", &refreshed);
        assert_eq!(response["snapshot_at"], "2023-11-14T22:14:20.000Z");
        assert_eq!(response["source_title"], "Source");

        let error = with_doc(&doc, |txn| {
            refresh_workspace_wire_in_doc(txn, &temp_dir, "graph-a", "w-ghost", 1.0)
        })
        .unwrap_err();
        assert_eq!(error, "wire not found: w-ghost");

        let snapshot = materialize_workspace_snapshot_json("graph-a", &doc)
            .expect("materialize workspace snapshot");
        assert_eq!(snapshot["counts"]["wires"], 1);
        assert_eq!(snapshot["counts"]["activeWires"], 1);
        assert_eq!(snapshot["wires"][0]["id"], "w-1");
        assert_eq!(snapshot["wires"][0]["predicate"], "supports");
    }

    #[test]
    fn artifact_wire_create_refresh_and_projection_roundtrip() {
        let doc = Doc::new();
        let temp_dir = std::env::temp_dir().join(format!("sophia-wire-{}", Uuid::new_v4()));
        with_doc(&doc, |txn| {
            put_workspace_artifact(
                txn,
                "graph-a",
                "artifact-pdf",
                &object_payload(&json!({
                    "name": "Paper.pdf",
                    "mimeType": "application/pdf",
                    "order": 1,
                    "updatedAt": 1_700_000_000_000i64,
                })),
            )
        })
        .expect("target artifact");
        let payload = object_payload(&json!({
            "wireId": "w-artifact",
            "target_artifact_id": "artifact-pdf",
            "predicate": "annotates",
            "updatedAt": 1_700_000_000_000i64,
        }));

        let data = with_doc(&doc, |txn| {
            create_workspace_wire_in_doc(txn, &temp_dir, "graph-a", "doc-companion", &payload)
        })
        .expect("create artifact-targeted wire");
        assert_eq!(data["targetArtifactId"], "artifact-pdf");
        assert_eq!(data["targetDocumentId"], Value::Null);
        assert_eq!(data["sceneArtifactId"], Value::Null);
        let response = create_hosted_wire_response("graph-a", "w-artifact", &data);
        assert_eq!(response["target_artifact_id"], "artifact-pdf");
        assert_eq!(response["target_document_id"], Value::Null);
        assert_eq!(response["target_title"], "Paper.pdf");
        assert_eq!(response["predicate"], "http://mnemosyne.ai/vocab#annotates");

        let refreshed = with_doc(&doc, |txn| {
            refresh_workspace_wire_in_doc(txn, &temp_dir, "graph-a", "w-artifact", 1_700_000_060_000.0)
        })
        .expect("refresh artifact-targeted wire");
        assert_eq!(refreshed["targetArtifactId"], "artifact-pdf");
        assert_eq!(refreshed["targetDocumentId"], Value::Null);

        let snapshot = materialize_workspace_snapshot_json("graph-a", &doc)
            .expect("materialize artifact-targeted wire");
        let wire = &snapshot["wires"][0];
        assert_eq!(wire["targetArtifactId"], "artifact-pdf");
        assert_eq!(wire["targetDocumentId"], Value::Null);
        assert_eq!(wire["targetTitle"], "Paper.pdf");
        assert!(wire["extra"].get("targetArtifactId").is_none(), "targetArtifactId is not an extra field: {wire}");
    }

    #[test]
    fn wire_creation_requires_exactly_one_target() {
        let doc = Doc::new();
        let temp_dir = std::env::temp_dir().join(format!("sophia-wire-{}", Uuid::new_v4()));
        for payload in [
            json!({ "wireId": "w-missing", "updatedAt": 1_700_000_000_000i64 }),
            json!({
                "wireId": "w-ambiguous",
                "targetDocumentId": "doc-target",
                "target_artifact_id": "artifact-pdf",
                "updatedAt": 1_700_000_000_000i64,
            }),
        ] {
            let error = with_doc(&doc, |txn| {
                create_workspace_wire_in_doc(txn, &temp_dir, "graph-a", "doc-source", &object_payload(&payload))
            })
            .expect_err("missing and ambiguous targets are refused");
            assert!(error.contains("exactly one"), "{error}");
        }
        let snapshot = materialize_workspace_snapshot_json("graph-a", &doc).expect("snapshot");
        assert_eq!(snapshot["counts"]["wires"], 0);
    }

    #[test]
    fn artifact_targets_refuse_block_endpoints() {
        let doc = Doc::new();
        let temp_dir = std::env::temp_dir().join(format!("sophia-wire-{}", Uuid::new_v4()));
        let payload = json!({
            "wireId": "w-artifact-block",
            "targetArtifactId": "artifact-pdf",
            "targetBlockId": "block-not-in-artifact",
            "updatedAt": 1_700_000_000_000i64,
        });
        let error = with_doc(&doc, |txn| {
            create_workspace_wire_in_doc(txn, &temp_dir, "graph-a", "doc-source", &object_payload(&payload))
        })
        .expect_err("artifact plus block is refused");
        assert!(error.contains("requires a document target"), "{error}");
        let snapshot = materialize_workspace_snapshot_json("graph-a", &doc).expect("snapshot");
        assert_eq!(snapshot["counts"]["wires"], 0);
    }

    #[test]
    fn entity_id_value_extracts_block_and_typed_ids() {
        assert_eq!(entity_id_value(&json!("doc#block-xyz")), json!("xyz"));
        assert_eq!(entity_id_value(&json!("doc#mark-m1")), json!("m1"));
        assert_eq!(entity_id_value(&json!("urn:x:folder:abc")), json!("abc"));
        assert_eq!(entity_id_value(&json!("urn:x:doc:doc-9")), json!("doc-9"));
        assert_eq!(
            entity_id_value(&json!("folder-plain")),
            json!("folder-plain")
        );
        assert_eq!(entity_id_value(&json!(null)), Value::Null);
        assert_eq!(entity_id_value(&json!("")), Value::Null);
    }

    #[test]
    fn epoch_ms_to_iso_matches_js_to_iso_string() {
        assert_eq!(epoch_ms_to_iso(0.0), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            epoch_ms_to_iso(1_700_000_000_000.0),
            "2023-11-14T22:13:20.000Z"
        );
        assert_eq!(
            epoch_ms_to_iso(1_700_000_000_123.0),
            "2023-11-14T22:13:20.123Z"
        );
    }

    #[test]
    fn artifact_file_type_matches_ts_behavior() {
        assert_eq!(artifact_file_type("Paper.PDF"), "pdf");
        assert_eq!(artifact_file_type("archive.tar.gz"), "gz");
        // TS quirk: `extension !== filename` is case-sensitive, so "README"
        // yields "readme" while an already-lowercase "readme" is "unknown".
        assert_eq!(artifact_file_type("README"), "readme");
        assert_eq!(artifact_file_type("readme"), "unknown");
    }

    /// Room-layer integration: mutate through a RoomRegistry-hosted room with
    /// a temp state path, then run the full persist path (workspace.json +
    /// RDF + graph touch) without an AppHandle.
    #[test]
    fn room_mutation_persists_snapshot_and_state() {
        let profile_dir = std::env::temp_dir().join(format!("sophia-ws-room-{}", Uuid::new_v4()));
        let graph_dir = profile_dir.join("graphs").join("graph-room");
        std::fs::create_dir_all(&graph_dir).expect("graph dir");
        crate::storage::write_json(
            &graph_dir.join("graph.json"),
            &json!({
                "graphId": "graph-room",
                "title": "Graph Room",
                "status": "active",
                "origin": "local",
                "providerId": "local",
                "localPath": graph_dir.display().to_string(),
                "createdAt": "1700000000000",
                "updatedAt": "1700000000000",
                "capabilities": [],
            }),
        )
        .expect("graph.json");

        crate::app_runtime::async_runtime::block_on(async {
            let registry = RoomRegistry::default();
            let state_path = crate::ydoc_paths::workspace_ydoc_state_path(&graph_dir);
            let room = registry
                .get_or_create("workspace:graph-room", state_path.clone())
                .await
                .expect("room");
            room.update_doc(|_doc, txn| {
                write_workspace_folder(
                    txn,
                    &json!({ "folderId": "folder-r", "name": "Room Folder", "order": 1 }),
                )
            })
            .await
            .expect("folder via room");
            room.update_doc(|_doc, txn| {
                write_workspace_document(
                    txn,
                    &json!({ "documentId": "doc-r", "title": "Room Doc", "parentId": "folder-r", "order": 2, "updatedAt": 5 }),
                )
            })
            .await
            .expect("document via room");
            let (epoch, snapshot) = materialize_workspace(&room, "graph-room")
                .await
                .expect("materialize workspace");
            persist_materialized_workspace("graph-room", &graph_dir, &snapshot)
                .expect("persist workspace");
            room.mark_projection_persisted(epoch);

            assert!(state_path.is_file(), "update-v1.bin persisted by the room");
            let snapshot: Value =
                crate::storage::read_json(&crate::ydoc_paths::workspace_snapshot_path(&graph_dir))
                    .expect("workspace.json");
            assert_eq!(snapshot["graphId"], "graph-room");
            assert_eq!(snapshot["counts"]["folders"], 1);
            assert_eq!(snapshot["counts"]["documents"], 1);
            assert_eq!(snapshot["tree"]["documents"][0]["id"], "folder-r");
            assert_eq!(
                snapshot["tree"]["documents"][0]["children"][0]["id"],
                "doc-r"
            );
        });
    }

    #[test]
    #[ignore = "diagnostic: requires GARDEN_WORKSPACE_FIXTURE"]
    fn inspect_external_workspace_fixture_with_yrs() {
        use yrs::updates::decoder::Decode;
        use yrs::Update;

        let path = std::env::var("GARDEN_WORKSPACE_FIXTURE")
            .expect("set GARDEN_WORKSPACE_FIXTURE to a workspace update-v1 binary");
        let bytes = std::fs::read(&path).expect("read workspace fixture");
        let update = Update::decode_v1(&bytes).expect("decode workspace fixture");
        let doc = Doc::new();
        doc.transact_mut()
            .apply_update(update)
            .expect("apply workspace fixture");
        let snapshot = materialize_workspace_snapshot_json("fixture", &doc)
            .expect("materialize workspace fixture");
        eprintln!(
            "fixture={} bytes={} counts={}",
            path,
            bytes.len(),
            snapshot["counts"]
        );
    }

    /// Regression for the SAME root defect e4f819e fixed in rooms.rs's room
    /// hydration and graph_duplicate_storage.rs's
    /// `rewrite_duplicate_document_ydoc`: 32fa25b made
    /// `document_sidecar_store::write_ydoc_update` persist a canonical
    /// encoded-empty Y.Doc (non-zero bytes) instead of a zero-byte file for a
    /// caller that writes no ydoc content — which is every document ever
    /// saved through the file-only `save_document` path (`document_ops.rs`'s
    /// `sync_room_document_title` never touches the Y.Doc for such a
    /// document). `sync_room_document_title`'s
    /// `!record.ydoc_update_base64.is_empty()` check (introduced aa1463f,
    /// 2026-07-11 — before 32fa25b) used to be a valid signal that "this
    /// record's Y.Doc already carries real content." Since `read_document_record`
    /// canonicalizes every sidecar read to a non-empty base64 string (real
    /// content OR canonical-empty), the check now reads true for EVERY
    /// document with a sidecar file at all, not just ones with real content.
    /// A live-but-untouched room (opened by, say, an editor blob route, but
    /// never edited — hydration correctly leaves its in-memory Doc blank per
    /// the rooms.rs fix) then takes the `persist_room_document_locked`
    /// branch on a bare workspace title rename, materializes the room's
    /// (blank) Doc, and clobbers the file-authored body/tiptapXml/tree with
    /// an empty projection.
    #[test]
    fn workspace_title_rename_of_a_live_untouched_room_preserves_file_authored_body() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile = std::env::temp_dir().join(format!(
            "garden-title-sync-preserve-body-{}",
            Uuid::new_v4()
        ));
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let result = std::panic::catch_unwind(|| {
            crate::app_runtime::async_runtime::block_on(async {
                let app = crate::tauri_runtime::build_mock_app_for_tests(true);
                let graph_id = "title-sync-preserve-body";
                let document_id = "file-only-doc";
                crate::graph_service::create_graph_service(
                    &app,
                    crate::graph_service::CreateGraphInput {
                        title: "Title Sync Preserve Body".to_string(),
                        graph_id: Some(graph_id.to_string()),
                        description: None,
                        operation_id: None,
                    },
                )
                .expect("create graph");
                crate::document_service::create_document(
                    app.clone(),
                    crate::document_service::CreateDocumentInput {
                        graph_id: graph_id.to_string(),
                        title: "Original Title".to_string(),
                        document_id: Some(document_id.to_string()),
                    },
                )
                .expect("create document");
                let real_body =
                    "Real file-authored content that must survive a title rename.";
                crate::document_persistence_service::save_document(
                    app.clone(),
                    serde_json::from_value(json!({
                        "graphId": graph_id,
                        "documentId": document_id,
                        "title": "Original Title",
                        "body": real_body,
                    }))
                    .expect("seed file-only save input"),
                )
                .expect("file-only save never touches the Y.Doc");

                let graph_dir = crate::graph_paths::existing_graph_dir(&app, graph_id)
                    .expect("graph directory");
                let registry = app.state::<RoomRegistry>();

                // Simulate an editor/blob-route touch: open the document room
                // live. Its sidecar carries canonical-empty bytes (no real
                // content was ever written through the Y.Doc), so the FIXED
                // rooms.rs hydration correctly leaves the in-memory Doc blank.
                registry
                    .get_or_create(
                        &format!("doc:{graph_id}:{document_id}"),
                        crate::ydoc_paths::document_ydoc_state_path(&graph_dir, document_id),
                    )
                    .await
                    .expect("live document room");

                // Rename the document purely through the workspace channel.
                let workspace_room = workspace_room(&app, graph_id, &graph_dir)
                    .await
                    .expect("workspace room");
                update_workspace_doc_checked(&workspace_room, |_doc, txn| {
                    write_workspace_document(
                        txn,
                        &json!({
                            "documentId": document_id,
                            "title": "Renamed Title",
                            "order": 1.0,
                            "updatedAt": 1.0,
                        }),
                    )
                })
                .await
                .expect("register renamed title in workspace");
                persist_workspace(&app, graph_id, &graph_dir, &workspace_room, "rename-op")
                    .await
                    .expect("persist workspace triggers the title sweep");

                let record = crate::document_service::read_document(
                    app.clone(),
                    graph_id.to_string(),
                    document_id.to_string(),
                )
                .expect("read document after rename");
                assert_eq!(record.title, "Renamed Title", "workspace title wins");
                assert_eq!(
                    record.body, real_body,
                    "a bare title rename must not clobber file-authored body content"
                );
            });
        });

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }
}

// ---------------------------------------------------------------------------
// document.batchPrepare / document.batchRegister (port of
// batchPrepareDocuments / batchRegisterDocuments,
// native-local-runtime.ts:1670-1791, plus the relative-path helpers at
// :3606-3662). Batches live in the workspace Y.Doc as nested Y.Maps under
// "uploadBatches": {clientBatchKey, folderMapJson, pendingDocumentsJson,
// createdAt, updatedAt[, registeredAt]}.
// ---------------------------------------------------------------------------

/// Port of normalizeRelativePath: backslashes → slashes, trimmed segments,
/// drops '.' segments, rejects any '..'.
pub fn normalize_relative_path(value: &str) -> Option<String> {
    let replaced = value.replace('\\', "/");
    let parts: Vec<&str> = replaced
        .split('/')
        .map(str::trim)
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    if parts.iter().any(|part| *part == "..") {
        return None;
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/"))
    }
}

/// Port of expandBatchFolderPaths: every ancestor of every path, depth-first
/// ordering (depth, then lexicographic).
pub fn expand_batch_folder_paths(paths: &[String]) -> Vec<String> {
    let mut expanded: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for raw_path in paths {
        let Some(normalized) = normalize_relative_path(raw_path) else {
            continue;
        };
        let parts: Vec<&str> = normalized.split('/').collect();
        for end in 1..=parts.len() {
            let prefix = parts[..end].join("/");
            if seen.insert(prefix.clone()) {
                expanded.push(prefix);
            }
        }
    }
    expanded.sort_by(|left, right| {
        let left_depth = left.split('/').count();
        let right_depth = right.split('/').count();
        left_depth.cmp(&right_depth).then_with(|| left.cmp(right))
    });
    expanded
}

/// Port of parentFolderPath.
pub fn parent_folder_path(relative_path: &str) -> Option<String> {
    let normalized = normalize_relative_path(relative_path)?;
    match normalized.rfind('/') {
        Some(index) if index > 0 => Some(normalized[..index].to_string()),
        _ => None,
    }
}

/// Port of pathBasename.
pub fn path_basename(relative_path: &str) -> String {
    match normalize_relative_path(relative_path) {
        None => "Untitled Folder".to_string(),
        Some(normalized) => normalized
            .rsplit('/')
            .next()
            .filter(|part| !part.is_empty())
            .map(str::to_string)
            .unwrap_or(normalized),
    }
}

/// Port of titleFromRelativePath: basename minus a trailing extension
/// (lastIndexOf('.') > 0).
pub fn title_from_relative_path(relative_path: &str) -> String {
    let filename = path_basename(relative_path);
    match filename.rfind('.') {
        Some(index) if index > 0 => filename[..index].to_string(),
        _ => filename,
    }
}

/// Port of parseFolderMap: normalized-path → folderId, accepting either an
/// object or its JSON string (the stored folderMapJson form).
pub fn parse_folder_map(value: &Value) -> JsonMap<String, Value> {
    match value {
        Value::Object(entries) => {
            let mut out = JsonMap::new();
            for (key, folder_id) in entries {
                let Some(normalized) = normalize_relative_path(key) else {
                    continue;
                };
                let Some(id) = string_value(Some(folder_id)) else {
                    continue;
                };
                out.insert(normalized, json!(id));
            }
            out
        }
        Value::String(text) if !text.trim().is_empty() => serde_json::from_str::<Value>(text)
            .map(|parsed| parse_folder_map(&parsed))
            .unwrap_or_default(),
        _ => JsonMap::new(),
    }
}

/// In-transaction body of document.batchPrepare. `folderIdMap` is
/// pre-injected by crdt_queue::normalize_payload_ids (stable IDs for every
/// path component, replay-safe).
pub fn batch_prepare_in_doc(
    txn: &mut TransactionMut<'_>,
    payload: &Value,
    enqueue_timestamp: &str,
) -> Result<Value, String> {
    let value = object_payload(payload);
    let client_batch_key = string_value(pick(&value, &["clientBatchKey", "client_batch_key"]))
        .ok_or_else(|| "clientBatchKey is required".to_string())?;
    let folder_paths = expand_batch_folder_paths(&string_array_value(value.get("folders")));

    let batches = txn.get_or_insert_map("uploadBatches");

    // clientBatchKey idempotency scan (sorted keys for deterministic
    // first-match; the TS forEach order is yjs-internal).
    let mut existing_ids: Vec<String> = batches.keys(&*txn).map(str::to_string).collect();
    existing_ids.sort();
    for existing_id in &existing_ids {
        let Some(Out::YMap(batch_map)) = batches.get(&*txn, existing_id) else {
            continue;
        };
        if map_string(&*txn, &batch_map, "clientBatchKey").as_deref()
            != Some(client_batch_key.as_str())
        {
            continue;
        }
        let folder_map = parse_folder_map(&map_value(&*txn, &batch_map, "folderMapJson"));
        return Ok(json!({ "batchId": existing_id, "folderMap": folder_map }));
    }

    let batch_id = string_value(pick(&value, &["batchId", "batch_id"]))
        .ok_or_else(|| "document.batchPrepare: batchId is required".to_string())?;
    let payload_folder_id_map = pick(&value, &["folderIdMap", "folder_id_map"])
        .map(object_payload)
        .unwrap_or_default();
    let order_base = numeric_timestamp(enqueue_timestamp);
    // A2 item 11: `now` derives from the journaled enqueueTimestamp so replay
    // produces the same timestamps rather than a fresh Date.now().
    let now = order_base;
    let mut folder_map = JsonMap::new();
    for (index, folder_path) in folder_paths.iter().enumerate() {
        let parent_path = parent_folder_path(folder_path);
        let folder_id = string_value(payload_folder_id_map.get(folder_path)).ok_or_else(|| {
            format!("document.batchPrepare: folderId missing for path \"{folder_path}\"")
        })?;
        folder_map.insert(folder_path.clone(), json!(folder_id));
        let parent_id = parent_path.and_then(|path| {
            folder_map
                .get(&path)
                .and_then(Value::as_str)
                .map(str::to_string)
        });
        write_workspace_folder(
            txn,
            &json!({
                "folderId": folder_id,
                "name": path_basename(folder_path),
                "parentId": parent_id,
                "section": "documents",
                "order": order_base + index as f64,
            }),
        )?;
    }

    let folder_map_json = serde_json::to_string(&Value::Object(folder_map.clone()))
        .unwrap_or_else(|_| "{}".to_string());
    let batch_map = batches.insert(txn, batch_id.as_str(), MapPrelim::default());
    batch_map.insert(txn, "clientBatchKey", client_batch_key.as_str());
    batch_map.insert(txn, "folderMapJson", folder_map_json.as_str());
    batch_map.insert(txn, "pendingDocumentsJson", "[]");
    batch_map.insert(txn, "createdAt", Any::Number(now));
    batch_map.insert(txn, "updatedAt", Any::Number(now));
    Ok(json!({ "batchId": batch_id, "folderMap": folder_map }))
}

/// In-transaction body of document.batchRegister. The TS handler resolves
/// each documentId against the runtime's record cache (hydrated from
/// nativeBridge.listDocuments); cells resolve against document.json on disk —
/// the same store that cache mirrors.
pub fn batch_register_in_doc(
    txn: &mut TransactionMut<'_>,
    graph_dir: &Path,
    payload: &Value,
    enqueue_timestamp: &str,
) -> Result<Value, String> {
    let value = object_payload(payload);
    let batch_id = string_value(pick(&value, &["batchId", "batch_id"]))
        .ok_or_else(|| "batchId is required".to_string())?;
    let documents: Vec<JsonMap<String, Value>> = value
        .get("documents")
        .and_then(Value::as_array)
        .map(|items| items.iter().map(object_payload).collect())
        .unwrap_or_default();

    let batches = txn.get_or_insert_map("uploadBatches");
    let batch_map = match batches.get(&*txn, &batch_id) {
        Some(Out::YMap(map)) => map,
        _ => return Err(format!("upload batch not found: {batch_id}")),
    };
    let folder_map = parse_folder_map(&map_value(&*txn, &batch_map, "folderMapJson"));
    let mut failed: Vec<String> = Vec::new();
    let mut registered = 0u64;
    let order_base = numeric_timestamp(enqueue_timestamp);
    // A2 item 11: `now` derives from the journaled enqueueTimestamp.
    let now = order_base;

    for (index, document) in documents.iter().enumerate() {
        let Some(document_id) = string_value(pick(document, &["documentId", "document_id"])) else {
            failed.push(format!("documents[{index}]"));
            continue;
        };
        let Some(record) = document_record_json(graph_dir, &document_id) else {
            failed.push(document_id);
            continue;
        };
        let Some(relative_path) = string_value(pick(document, &["relativePath", "relative_path"]))
            .and_then(|path| normalize_relative_path(&path))
        else {
            failed.push(document_id);
            continue;
        };
        let folder_path = parent_folder_path(&relative_path);
        let parent_id = folder_path.as_ref().and_then(|path| {
            folder_map
                .get(path)
                .and_then(Value::as_str)
                .map(str::to_string)
        });
        if folder_path.is_some() && parent_id.is_none() {
            failed.push(document_id);
            continue;
        }
        // nativeRecordTitle(record) || (document.title ?? titleFromRelativePath):
        // nativeRecordTitle never yields a falsy value ('Untitled' fallback),
        // so the record title always wins — ported verbatim.
        let record_title = string_value(record.get("title"))
            .map(|title| title.trim().to_string())
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| "Untitled".to_string());
        let title = if record_title.is_empty() {
            string_value(document.get("title"))
                .unwrap_or_else(|| title_from_relative_path(&relative_path))
        } else {
            record_title
        };
        let source_file = pick(document, &["sourceFile", "source_file"])
            .cloned()
            .unwrap_or(Value::Null);
        let read_only = boolean_value(pick(document, &["readOnly", "read_only"]), true);
        write_workspace_document(
            txn,
            &json!({
                "documentId": document_id,
                "title": title,
                "parentId": parent_id,
                "order": order_base + index as f64,
                "updatedAt": now,
                "readOnly": read_only,
                "sourceFile": source_file,
            }),
        )?;
        registered += 1;
    }

    batch_map.insert(txn, "pendingDocumentsJson", "[]");
    batch_map.insert(txn, "registeredAt", Any::Number(now));
    batch_map.insert(txn, "updatedAt", Any::Number(now));
    Ok(json!({ "registered": registered, "failed": failed }))
}

pub(crate) async fn batch_prepare(
    app: &AppHandle,
    operation: &CrdtOperation,
) -> ApplyOperationResult<Value> {
    let graph_id = operation.graph_id.clone();
    let graph_dir = crate::graph_paths::existing_graph_dir(app, &graph_id)?;
    let room = workspace_room(app, &graph_id, &graph_dir).await?;
    let payload = operation.payload.clone();
    let enqueue_timestamp = operation.enqueue_timestamp.clone();
    let result = update_workspace_doc_checked(&room, move |_doc, txn| {
        batch_prepare_in_doc(txn, &payload, &enqueue_timestamp)
    })
    .await?;
    persist_workspace(app, &graph_id, &graph_dir, &room, &operation.operation_id)
        .await
        .map_err(ApplyOperationError::retryable_after_hot_commit)?;
    Ok(result)
}

pub(crate) async fn batch_register(
    app: &AppHandle,
    operation: &CrdtOperation,
) -> ApplyOperationResult<Value> {
    let graph_id = operation.graph_id.clone();
    let graph_dir = crate::graph_paths::existing_graph_dir(app, &graph_id)?;
    let room = workspace_room(app, &graph_id, &graph_dir).await?;
    let payload = operation.payload.clone();
    let enqueue_timestamp = operation.enqueue_timestamp.clone();
    let result = {
        let graph_dir = graph_dir.clone();
        update_workspace_doc_checked(&room, move |_doc, txn| {
            batch_register_in_doc(txn, &graph_dir, &payload, &enqueue_timestamp)
        })
        .await?
    };
    persist_workspace(app, &graph_id, &graph_dir, &room, &operation.operation_id)
        .await
        .map_err(ApplyOperationError::retryable_after_hot_commit)?;
    Ok(result)
}

#[cfg(test)]
mod created_at_healing_tests {
    use super::healed_created_at;

    const NOW: f64 = 1_792_000_000_000.0; // October 2026
    const MARCH: f64 = 1_772_400_000_000.0; // March 2026

    #[test]
    fn a_sane_creation_time_is_kept() {
        assert_eq!(healed_created_at(Some(MARCH), Some(NOW), NOW), Some(MARCH));
        assert_eq!(healed_created_at(None, Some(MARCH), NOW), None);
    }

    #[test]
    fn an_impossible_creation_time_takes_a_plausible_order() {
        // A cloud-1 value 1000 times too large (the year ~31970).
        assert_eq!(healed_created_at(Some(946_713_600_000_000.0), Some(MARCH), NOW), Some(MARCH));
    }

    #[test]
    fn without_a_plausible_order_it_takes_now() {
        for order in [None, Some(5.0), Some(NOW + 1.0), Some(f64::NAN)] {
            assert_eq!(healed_created_at(Some(946_713_600_000_000.0), order, NOW), Some(NOW));
        }
    }
}
