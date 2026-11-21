use crate::{
    document_history_service::{
        copy_document_snapshot_as_manual, current_document_snapshot,
        delete_manual_document_snapshot, document_snapshot_hosted_json,
        hosted_document_snapshot_count_response, hosted_document_snapshot_html_response,
        hosted_document_snapshot_list_response, hosted_document_snapshot_text_response,
    },
    loopback_document_inputs::ManualSnapshotRequest,
    loopback_http::{loopback_error, require_loopback_scope},
    loopback_state::LoopbackState,
};
use axum::{
    extract::{Path as AxumPath, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use std::{collections::BTreeMap, sync::Arc};

pub(super) async fn loopback_hosted_list_document_snapshots(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, doc_id)): AxumPath<(String, String)>,
    Query(params): Query<BTreeMap<String, String>>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "documents.history.read") {
        return response;
    }
    let limit = params
        .get("limit")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(200);
    match hosted_document_snapshot_list_response(&state.app, &graph_id, &doc_id, limit) {
        Ok(value) => Json(value).into_response(),
        Err(error) if error.contains("not found") => loopback_error(StatusCode::NOT_FOUND, &error),
        Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
    }
}

pub(super) async fn loopback_hosted_create_manual_document_snapshot(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, doc_id)): AxumPath<(String, String)>,
    body: Option<Json<ManualSnapshotRequest>>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "documents.snapshots.write") {
        return response;
    }
    let label = body
        .and_then(|Json(input)| input.label)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    match current_document_snapshot(&state.app, &graph_id, &doc_id, true, label) {
        Ok(snapshot) => (
            StatusCode::CREATED,
            Json(document_snapshot_hosted_json(&snapshot)),
        )
            .into_response(),
        Err(error) if error.contains("not found") => loopback_error(StatusCode::NOT_FOUND, &error),
        Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
    }
}

pub(super) async fn loopback_hosted_document_snapshot_count(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, doc_id)): AxumPath<(String, String)>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "documents.history.read") {
        return response;
    }
    match hosted_document_snapshot_count_response(&state.app, &graph_id, &doc_id) {
        Ok(value) => Json(value).into_response(),
        Err(error) if error.contains("not found") => loopback_error(StatusCode::NOT_FOUND, &error),
        Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
    }
}

pub(super) async fn loopback_hosted_document_snapshot_html(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, doc_id, snapshot_id)): AxumPath<(String, String, String)>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "documents.history.read") {
        return response;
    }
    match hosted_document_snapshot_html_response(&state.app, &graph_id, &doc_id, &snapshot_id) {
        Ok(response) => response,
        Err(error) if error.contains("not found") => loopback_error(StatusCode::NOT_FOUND, &error),
        Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
    }
}

pub(super) async fn loopback_hosted_document_snapshot_text(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, doc_id, snapshot_id)): AxumPath<(String, String, String)>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "documents.history.read") {
        return response;
    }
    match hosted_document_snapshot_text_response(&state.app, &graph_id, &doc_id, &snapshot_id) {
        Ok(response) => response,
        Err(error) if error.contains("not found") => loopback_error(StatusCode::NOT_FOUND, &error),
        Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
    }
}

pub(super) async fn loopback_hosted_copy_document_snapshot(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, doc_id, snapshot_id)): AxumPath<(String, String, String)>,
    body: Option<Json<ManualSnapshotRequest>>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "documents.snapshots.write") {
        return response;
    }
    let label = body
        .and_then(|Json(input)| input.label)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    match copy_document_snapshot_as_manual(&state.app, &graph_id, &doc_id, &snapshot_id, label) {
        Ok(snapshot) => (
            StatusCode::CREATED,
            Json(document_snapshot_hosted_json(&snapshot)),
        )
            .into_response(),
        Err(error) if error.contains("not found") => loopback_error(StatusCode::NOT_FOUND, &error),
        Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
    }
}

pub(super) async fn loopback_hosted_delete_document_snapshot(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath((graph_id, doc_id, snapshot_id)): AxumPath<(String, String, String)>,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "documents.snapshots.delete") {
        return response;
    }
    match delete_manual_document_snapshot(&state.app, &graph_id, &doc_id, &snapshot_id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) if error.contains("not found") => loopback_error(StatusCode::NOT_FOUND, &error),
        Err(error) if error.contains("manually saved") => {
            loopback_error(StatusCode::FORBIDDEN, &error)
        }
        Err(error) => loopback_error(StatusCode::BAD_REQUEST, &error),
    }
}
