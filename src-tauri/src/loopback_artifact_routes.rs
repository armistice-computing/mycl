use crate::{
    app_error::AppError,
    artifact_attach::{attach_pending_upload, AttachInput, EntryMode},
    artifact_revisions::{
        create_artifact_revision, list_artifact_revisions, read_artifact_revision,
        restore_artifact_revision,
    },
    crdt_queue::{enqueue_crdt_operation_outcome, EnqueueCrdtOperationInput},
    ids::safe_filename,
    local_crdt_jobs::{insert_and_spawn_crdt_job, LocalCrdtJobInput},
    local_jobs::{local_job_submit_response, LocalJobRecord},
    loopback_artifact_ingest_routes::{
        loopback_hosted_convert_artifact, loopback_hosted_import_artifact,
    },
    loopback_batch_routes::{loopback_hosted_batch_prepare, loopback_hosted_batch_register},
    loopback_http::{loopback_error, loopback_original_file_error, require_loopback_scope},
    loopback_image_routes::{loopback_hosted_serve_image, loopback_hosted_upload_image},
    loopback_ingestion_routes::loopback_ingestion_config_router,
    loopback_pdf_ingest_routes::loopback_hosted_ingest_pdf_accurate,
    loopback_state::LoopbackState,
    multipart_pending_upload::stream_field_to_pending_upload,
    original_file_service::{original_file_download_response, read_artifact_original_file},
    paths::{cleanup_pending_upload_file, existing_graph_dir},
    runtime_config::LOCAL_UPLOAD_MAX_BYTES,
    storage::display_path,
};
use axum::{
    extract::{Multipart, Path as AxumPath, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use std::sync::Arc;
use uuid::Uuid;

pub(super) fn loopback_artifact_router() -> Router<Arc<LoopbackState>> {
    Router::new()
        .merge(loopback_ingestion_config_router())
        .route(
            "/artifacts/{graph_id}/batch/prepare",
            post(loopback_hosted_batch_prepare),
        )
        .route(
            "/artifacts/{graph_id}/batch/register",
            post(loopback_hosted_batch_register),
        )
        .route(
            "/artifacts/{graph_id}/upload",
            post(loopback_hosted_upload_artifact),
        )
        .route(
            "/artifacts/{graph_id}/reference",
            post(loopback_hosted_reference_artifact),
        )
        .route("/artifacts/{graph_id}", get(loopback_hosted_list_artifacts))
        .route(
            "/artifacts/{graph_id}/{artifact_id}/status",
            get(loopback_hosted_artifact_status),
        )
        .route(
            "/artifacts/{graph_id}/ingest/pdf-accurate",
            post(loopback_hosted_ingest_pdf_accurate),
        )
        .route(
            "/artifacts/{graph_id}/images/upload",
            post(loopback_hosted_upload_image),
        )
        .route(
            "/artifacts/{graph_id}/images/{image_id}",
            get(loopback_hosted_serve_image),
        )
        .route(
            "/artifacts/{graph_id}/{artifact_id}/download",
            get(loopback_hosted_download_artifact),
        )
        .route(
            "/artifacts/{graph_id}/{artifact_id}/revisions",
            get(loopback_list_artifact_revisions).post(loopback_create_artifact_revision),
        )
        .route(
            "/artifacts/{graph_id}/{artifact_id}/revisions/{revision_id}/download",
            get(loopback_download_artifact_revision),
        )
        .route(
            "/artifacts/{graph_id}/{artifact_id}/revisions/{revision_id}/restore",
            post(loopback_restore_artifact_revision),
        )
        .route(
            "/artifacts/{graph_id}/{artifact_id}/import",
            post(loopback_hosted_import_artifact),
        )
        .route(
            "/artifacts/{graph_id}/{artifact_id}/convert",
            post(loopback_hosted_convert_artifact),
        )
}

fn prefers_sync_work(headers: &HeaderMap) -> bool {
    headers
        .get("prefer")
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .split(',')
                .any(|part| part.trim().eq_ignore_ascii_case("respond-sync"))
        })
        .unwrap_or(false)
}

fn artifact_upload_job_response(
    record: &LocalJobRecord,
    document_id: &str,
) -> Result<serde_json::Value, String> {
    let mut value = serde_json::to_value(local_job_submit_response(record))
        .map_err(|error| format!("serialize local artifact upload job response: {error}"))?;
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "jobId".to_string(),
            serde_json::Value::String(record.job_id.clone()),
        );
        object.insert(
            "documentId".to_string(),
            serde_json::Value::String(document_id.to_string()),
        );
        object.insert(
            "document_id".to_string(),
            serde_json::Value::String(document_id.to_string()),
        );
    }
    Ok(value)
}

pub(super) async fn loopback_hosted_upload_artifact(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
    mut multipart: Multipart,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "artifacts.write") {
        return response;
    }

    let mut filename: Option<String> = None;
    let mut mime_type: Option<String> = None;
    let graph_dir = match existing_graph_dir(&state.app, &graph_id) {
        Ok(graph_dir) => graph_dir,
        Err(error) => return loopback_error(StatusCode::NOT_FOUND, &error),
    };
    let mut pending_original_path: Option<std::path::PathBuf> = None;
    let mut size_bytes: usize = 0;
    let mut sha256: Option<String> = None;
    let mut parent_id: Option<String> = None;
    let mut batch_id: Option<String> = None;
    let mut label: Option<String> = None;
    let mut mode: Option<EntryMode> = None;

    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(error) => {
                return loopback_error(
                    StatusCode::BAD_REQUEST,
                    &format!("invalid multipart upload: {error}"),
                )
            }
        };

        let name = field.name().map(str::to_string).unwrap_or_default();
        if name == "file" {
            let field_filename = field
                .file_name()
                .map(str::to_string)
                .unwrap_or_else(|| "unnamed".to_string());
            let field_mime_type = field.content_type().map(ToString::to_string);
            let upload = match stream_field_to_pending_upload(
                &graph_dir,
                field,
                LOCAL_UPLOAD_MAX_BYTES,
                "failed to read uploaded file",
            )
            .await
            {
                Ok(upload) => upload,
                Err(error) => return loopback_error(error.status, &error.message),
            };
            if let Some(previous_path) = pending_original_path.replace(upload.path) {
                cleanup_pending_upload_file(&previous_path);
            }
            filename = Some(field_filename);
            mime_type = field_mime_type;
            size_bytes = upload.bytes_written;
            sha256 = Some(upload.sha256);
            continue;
        }

        let text = match field.text().await {
            Ok(value) => value.trim().to_string(),
            Err(error) => {
                return loopback_error(
                    StatusCode::BAD_REQUEST,
                    &format!("failed to read multipart field {name}: {error}"),
                )
            }
        };
        if text.is_empty() {
            continue;
        }
        match name.as_str() {
            "parent_id" | "parentId" => parent_id = Some(text),
            "batch_id" | "batchId" => batch_id = Some(text),
            "label" => label = Some(text),
            "mode" => match EntryMode::parse(&text) {
                Ok(parsed) => mode = Some(parsed),
                Err(error) => {
                    if let Some(path) = pending_original_path.as_ref() {
                        cleanup_pending_upload_file(path);
                    }
                    return loopback_error(StatusCode::BAD_REQUEST, &error);
                }
            },
            _ => {}
        }
    }

    let Some(pending_original_path) = pending_original_path else {
        return loopback_error(StatusCode::BAD_REQUEST, "missing multipart file field");
    };
    if size_bytes == 0 {
        cleanup_pending_upload_file(&pending_original_path);
        return loopback_error(StatusCode::BAD_REQUEST, "Empty file uploaded");
    }
    let filename = filename.unwrap_or_else(|| "unnamed".to_string());
    let mime_type = mime_type.unwrap_or_else(|| "application/octet-stream".to_string());

    // Attach-only: bytes kept, hashed and indexed, no blocks. The default
    // when the door names no mode is to digest what the upload job can parse
    // and attach the rest, instead of failing on a type with no parser.
    let mode = mode.unwrap_or_else(|| EntryMode::default_for(&filename, &mime_type));
    if mode == EntryMode::Attach {
        return match attach_pending_upload(
            state.app.clone(),
            AttachInput {
                graph_id,
                filename,
                mime_type,
                pending_path: pending_original_path,
                size_bytes,
                sha256,
                parent_id,
                label,
            },
        )
        .await
        {
            Ok(value) => (StatusCode::CREATED, Json(value)).into_response(),
            Err(error) => loopback_error(StatusCode::UNPROCESSABLE_ENTITY, &error),
        };
    }

    let mut payload = serde_json::json!({
        "filename": filename,
        "mimeType": mime_type,
        "sizeBytes": size_bytes,
        "pendingOriginalPath": display_path(&pending_original_path),
        "parentId": parent_id,
        "batchId": batch_id,
    });

    if prefers_sync_work(&headers) {
        let outcome = enqueue_crdt_operation_outcome(
            state.app.clone(),
            EnqueueCrdtOperationInput {
                kind: "document.uploadIngest".to_string(),
                graph_id,
                document_id: None,
                payload,
            },
        )
        .await;
        return match outcome {
            Ok(outcome) => (StatusCode::CREATED, Json(outcome.value)).into_response(),
            Err(error) => {
                cleanup_pending_upload_file(&pending_original_path);
                loopback_error(StatusCode::UNPROCESSABLE_ENTITY, &error)
            }
        };
    }

    let document_id = format!("doc-{}", Uuid::new_v4().simple());
    let payload_filename = payload
        .get("filename")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let payload_mime_type = payload
        .get("mimeType")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let payload_parent_id = payload
        .get("parentId")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let payload_batch_id = payload
        .get("batchId")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let source_filename = payload_filename.as_str().unwrap_or("unnamed");
    let detail_graph_id = graph_id.clone();
    if let Some(object) = payload.as_object_mut() {
        object.insert(
            "documentId".to_string(),
            serde_json::Value::String(document_id.clone()),
        );
        object.insert(
            "sourceFile".to_string(),
            serde_json::json!({
                "storageKey": format!(
                    "local://documents/{document_id}/original/{}",
                    safe_filename(source_filename)
                ),
                "originalFilename": payload_filename.clone(),
                "mimeType": payload_mime_type.clone(),
                "sizeBytes": size_bytes,
            }),
        );
    }
    let record = match insert_and_spawn_crdt_job(
        state,
        LocalCrdtJobInput {
            job_type: "artifact_upload".to_string(),
            graph_id,
            operation_kind: "document.uploadIngest".to_string(),
            document_id: Some(document_id.clone()),
            detail: serde_json::json!({
                "graphId": detail_graph_id,
                "documentId": document_id.clone(),
                "filename": payload_filename,
                "mimeType": payload_mime_type,
                "sizeBytes": size_bytes,
                "parentId": payload_parent_id,
                "batchId": payload_batch_id,
                "asyncWork": true,
            }),
            payload,
            pending_cleanup_path: Some(pending_original_path),
            running_message: "Ingesting uploaded artifact".to_string(),
            success_message: "Artifact upload import complete".to_string(),
            result_mapper: None,
        },
    ) {
        Ok(record) => record,
        Err(error) => return loopback_error(StatusCode::INTERNAL_SERVER_ERROR, &error),
    };
    match artifact_upload_job_response(&record, &document_id) {
        Ok(value) => (StatusCode::ACCEPTED, Json(value)).into_response(),
        Err(error) => loopback_error(StatusCode::INTERNAL_SERVER_ERROR, &error),
    }
}

/// The Files list: every record with its mode, storage, hash, index, and
/// how many wires and which documents point at it.
pub(super) async fn loopback_hosted_list_artifacts(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "artifacts.read") {
        return response;
    }
    if let Err(error) =
        crate::artifact_storage::backfill_borrowed_records(state.app.clone(), &graph_id).await
    {
        log::warn!("artifacts: backfill of borrowed records for {graph_id} failed: {error}");
    }
    match crate::artifact_storage::artifact_list_with_pointers(&state.app, &graph_id) {
        Ok(artifacts) => Json(serde_json::json!({ "graphId": graph_id, "artifacts": artifacts })).into_response(),
        Err(error) if error.contains("not found") => loopback_error(StatusCode::NOT_FOUND, &error),
        Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
    }
}

/// One record's standing now: ok, stale, missing or (for a reference) denied.
pub(super) async fn loopback_hosted_artifact_status(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, artifact_id)): AxumPath<(String, String)>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "artifacts.read") {
        return response;
    }
    let app = state.app.clone();
    match tokio::task::spawn_blocking(move || {
        crate::artifact_storage::artifact_status(&app, &graph_id, &artifact_id)
    })
    .await
    {
        Ok(Ok(status)) => Json(status).into_response(),
        Ok(Err(error)) if error.contains("not found") => loopback_error(StatusCode::NOT_FOUND, &error),
        Ok(Err(error)) => loopback_error(StatusCode::BAD_REQUEST, &error),
        Err(error) => loopback_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("status worker failed: {error}")),
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ReferenceArtifactInput {
    path: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    parent_id: Option<String>,
}

/// Make a reference to a file under one of the owner's roots: nothing is
/// copied; the record carries the hash, the index and the address.
pub(super) async fn loopback_hosted_reference_artifact(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
    Json(input): Json<ReferenceArtifactInput>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "artifacts.write") {
        return response;
    }
    match crate::artifact_reference::reference_path(
        state.app.clone(),
        crate::artifact_reference::ReferenceInput {
            graph_id,
            path: input.path,
            label: input.label,
            parent_id: input.parent_id,
        },
    )
    .await
    {
        Ok(value) => (StatusCode::CREATED, Json(value)).into_response(),
        Err(error) if error.starts_with("denied:") => loopback_error(StatusCode::FORBIDDEN, &error),
        Err(error) if error.starts_with("missing:") => loopback_error(StatusCode::NOT_FOUND, &error),
        Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
    }
}

pub(super) async fn loopback_hosted_download_artifact(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, artifact_id)): AxumPath<(String, String)>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "artifacts.read") {
        return response;
    }
    // A reference is read from outside the house: re-authorized against the
    // roots and re-opened every time, and missing or denied are answered as
    // such, never as an empty body.
    if let Ok(Some(record)) = crate::artifact_storage::artifact_record(&state.app, &graph_id, &artifact_id) {
        if crate::artifact_reference::record_is_referenced(&record) {
            use crate::artifact_reference::{read_referenced, ReadState, REFERENCE_DOWNLOAD_CAP_BYTES};
            let read = read_referenced(
                &state.app,
                &graph_id,
                &record,
                REFERENCE_DOWNLOAD_CAP_BYTES as usize + 1,
            );
            if matches!(read.state, ReadState::Ok | ReadState::Stale)
                && read.size_bytes > REFERENCE_DOWNLOAD_CAP_BYTES
            {
                return loopback_error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    &format!(
                        "reference is {} bytes; the house serves at most {} bytes of a reference (the file is on this computer at {})",
                        read.size_bytes, REFERENCE_DOWNLOAD_CAP_BYTES, read.path
                    ),
                );
            }
            return match read.state {
                ReadState::Missing => loopback_error(StatusCode::NOT_FOUND, read.note.as_deref().unwrap_or("missing")),
                ReadState::Denied => loopback_error(StatusCode::FORBIDDEN, read.note.as_deref().unwrap_or("denied")),
                ReadState::Ok | ReadState::Stale => {
                    let manifest = crate::original_file_types::OriginalFileManifest {
                        filename: read.filename.clone(),
                        mime_type: read.mime_type.clone(),
                        size_bytes: read.bytes.len(),
                        local_path: read.path.clone(),
                        created_at: String::new(),
                        updated_at: String::new(),
                        sha256: read.sha256_now.clone(),
                    };
                    match original_file_download_response(&manifest, read.bytes, false) {
                        Ok(mut response) => {
                            if read.state == ReadState::Stale {
                                response.headers_mut().insert(
                                    "x-artifact-state",
                                    axum::http::HeaderValue::from_static("stale"),
                                );
                            }
                            response
                        }
                        Err(error) => loopback_original_file_error(&error),
                    }
                }
            };
        }
    }
    match read_artifact_original_file(&state.app, &graph_id, &artifact_id).and_then(
        |(manifest, bytes)| {
            original_file_download_response(&manifest, bytes, false).map_err(AppError::storage)
        },
    ) {
        Ok(response) => response,
        Err(error) => loopback_original_file_error(error.message_ref()),
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CreateArtifactRevisionInput {
    data_base64: String,
    mime_type: String,
    #[serde(default)]
    filename: Option<String>,
    #[serde(default)]
    label: Option<String>,
}

pub(super) async fn loopback_create_artifact_revision(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, artifact_id)): AxumPath<(String, String)>,
    Json(input): Json<CreateArtifactRevisionInput>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "artifacts.write") {
        return response;
    }
    // Preserve the artifact's existing filename across edits when the client
    // doesn't supply one.
    let filename = input.filename.unwrap_or_else(|| {
        read_artifact_original_file(&state.app, &graph_id, &artifact_id)
            .map(|(manifest, _)| manifest.filename)
            .unwrap_or_else(|_| format!("{artifact_id}.png"))
    });
    match create_artifact_revision(
        &state.app,
        &graph_id,
        &artifact_id,
        &filename,
        &input.mime_type,
        &input.data_base64,
        input.label,
    ) {
        Ok(entry) => {
            // The bytes moved; the record's hash and index follow (owed from
            // the step 1 angel review).
            if let Err(error) = crate::artifact_storage::refresh_owned_record_after_write(
                state.app.clone(),
                &graph_id,
                &artifact_id,
            )
            .await
            {
                return loopback_error(StatusCode::BAD_REQUEST, &error);
            }
            (StatusCode::CREATED, Json(entry)).into_response()
        }
        Err(error) => loopback_original_file_error(error.message_ref()),
    }
}

pub(super) async fn loopback_list_artifact_revisions(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, artifact_id)): AxumPath<(String, String)>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "artifacts.read") {
        return response;
    }
    match list_artifact_revisions(&state.app, &graph_id, &artifact_id) {
        Ok(revisions) => Json(serde_json::json!({ "revisions": revisions })).into_response(),
        Err(error) => loopback_original_file_error(error.message_ref()),
    }
}

pub(super) async fn loopback_download_artifact_revision(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, artifact_id, revision_id)): AxumPath<(String, String, String)>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "artifacts.read") {
        return response;
    }
    match read_artifact_revision(&state.app, &graph_id, &artifact_id, &revision_id).and_then(
        |(manifest, bytes)| {
            original_file_download_response(&manifest, bytes, false).map_err(AppError::storage)
        },
    ) {
        Ok(response) => response,
        Err(error) => loopback_original_file_error(error.message_ref()),
    }
}

pub(super) async fn loopback_restore_artifact_revision(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, artifact_id, revision_id)): AxumPath<(String, String, String)>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "artifacts.write") {
        return response;
    }
    match restore_artifact_revision(&state.app, &graph_id, &artifact_id, &revision_id) {
        Ok(checkpoint) => (StatusCode::OK, Json(checkpoint)).into_response(),
        Err(error) => loopback_original_file_error(error.message_ref()),
    }
}
