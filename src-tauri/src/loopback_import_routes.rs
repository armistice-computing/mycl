use crate::{
    clock::timestamp,
    local_crdt_jobs::{insert_and_spawn_crdt_job, LocalCrdtJobInput},
    loopback_http::{loopback_error, require_loopback_scope},
    loopback_import_job_response::import_job_accepted_response,
    loopback_import_uploads::{read_archive_import_upload, read_graph_import_upload},
    loopback_state::LoopbackState,
    paths::existing_graph_dir,
    profile_paths::profile_dir,
    storage::display_path,
};
use axum::{
    extract::{Multipart, Path as AxumPath, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use std::sync::Arc;

fn import_source_default_folder_name(source_type: &str) -> String {
    let label = match source_type {
        "obsidian" => "Obsidian",
        "notion" => "Notion",
        "roam" => "Roam",
        _ => "Archive",
    };
    format!("Imported {label} {}", timestamp())
}

pub(super) async fn loopback_hosted_import_graph(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    multipart: Multipart,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "graphs.import") {
        return response;
    }

    let profile_root = match profile_dir(&state.app) {
        Ok(profile_dir) => profile_dir,
        Err(error) => return loopback_error(StatusCode::INTERNAL_SERVER_ERROR, &error),
    };

    let upload = match read_graph_import_upload(&profile_root, multipart).await {
        Ok(upload) => upload,
        Err(response) => return response,
    };

    let job_graph_id = upload.new_graph_id.clone();
    let payload = serde_json::json!({
        "filename": upload.filename,
        "newGraphId": upload.new_graph_id,
        "newTitle": upload.new_title,
        "pendingArchivePath": display_path(&upload.pending_archive_path),
        "sizeBytes": upload.size_bytes,
    });
    let record = match insert_and_spawn_crdt_job(
        state,
        LocalCrdtJobInput {
            job_type: "import_graph".to_string(),
            graph_id: job_graph_id,
            operation_kind: "graph.importArchive".to_string(),
            document_id: None,
            detail: serde_json::json!({
                "filename": payload.get("filename").cloned().unwrap_or(serde_json::Value::Null),
                "newGraphId": payload.get("newGraphId").cloned().unwrap_or(serde_json::Value::Null),
                "newTitle": payload.get("newTitle").cloned().unwrap_or(serde_json::Value::Null),
                "sizeBytes": payload.get("sizeBytes").cloned().unwrap_or(serde_json::Value::Null),
                "asyncWork": true,
            }),
            payload,
            pending_cleanup_path: Some(upload.pending_archive_path),
            running_message: "Importing graph archive".to_string(),
            success_message: "Graph archive import complete".to_string(),
            result_mapper: None,
        },
    ) {
        Ok(record) => record,
        Err(error) => return loopback_error(StatusCode::INTERNAL_SERVER_ERROR, &error),
    };
    import_job_accepted_response(&record)
}

async fn loopback_hosted_import_archive(
    state: Arc<LoopbackState>,
    headers: HeaderMap,
    graph_id: String,
    source_type: &'static str,
    multipart: Multipart,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "graphs.import") {
        return response;
    }
    let graph_dir = match existing_graph_dir(&state.app, &graph_id) {
        Ok(graph_dir) => graph_dir,
        Err(error) => return loopback_error(StatusCode::NOT_FOUND, &error),
    };

    let upload = match read_archive_import_upload(source_type, &graph_dir, multipart).await {
        Ok(upload) => upload,
        Err(response) => return response,
    };
    let effective_folder = upload
        .folder_name
        .unwrap_or_else(|| import_source_default_folder_name(source_type));
    let payload = serde_json::json!({
        "sourceType": source_type,
        "filename": upload.filename,
        "folderName": effective_folder,
        "pendingArchivePath": display_path(&upload.pending_archive_path),
        "sizeBytes": upload.size_bytes,
    });
    let record = match insert_and_spawn_crdt_job(
        state,
        LocalCrdtJobInput {
            job_type: format!("import_{source_type}"),
            graph_id,
            operation_kind: "import.vault".to_string(),
            document_id: None,
            detail: serde_json::json!({
                "sourceType": source_type,
                "filename": payload.get("filename").cloned().unwrap_or(serde_json::Value::Null),
                "folderName": payload.get("folderName").cloned().unwrap_or(serde_json::Value::Null),
                "sizeBytes": payload.get("sizeBytes").cloned().unwrap_or(serde_json::Value::Null),
                "asyncWork": true,
            }),
            payload,
            pending_cleanup_path: Some(upload.pending_archive_path),
            running_message: format!("Importing {source_type} archive"),
            success_message: format!("Imported {source_type} archive"),
            result_mapper: None,
        },
    ) {
        Ok(record) => record,
        Err(error) => return loopback_error(StatusCode::INTERNAL_SERVER_ERROR, &error),
    };
    import_job_accepted_response(&record)
}

pub(super) async fn loopback_hosted_import_obsidian(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
    multipart: Multipart,
) -> Response {
    loopback_hosted_import_archive(state, headers, graph_id, "obsidian", multipart).await
}

pub(super) async fn loopback_hosted_import_notion(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
    multipart: Multipart,
) -> Response {
    loopback_hosted_import_archive(state, headers, graph_id, "notion", multipart).await
}

pub(super) async fn loopback_hosted_import_roam(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
    multipart: Multipart,
) -> Response {
    loopback_hosted_import_archive(state, headers, graph_id, "roam", multipart).await
}
