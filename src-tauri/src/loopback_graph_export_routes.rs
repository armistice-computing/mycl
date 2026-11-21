//! Graph archive export routes — the mirror of `/graphs/import`.
//!
//! POST /graphs/{graph_id}/export submits an async job that builds the full
//! tar.gz bundle (manifest + n-quads RDF + workspace/document Y.Docs +
//! artifact originals; see `graph_archive_export`). The job result carries
//! `download_url` + `archive_size_bytes`, which the settings page polls for.
//!
//! GET /graphs/{graph_id}/export/download serves the built archive. It is
//! authorized by the short-lived token minted at build time (query params
//! `file` + `token`) rather than the loopback bearer, because the browser
//! download is a plain <a> click that cannot send headers — the same
//! pattern as image access tokens.

use crate::{
    graph_archive_export::{
        build_graph_archive, export_access_token_matches, resolve_export_archive,
        write_export_access_token,
    },
    local_job_types::LocalJobStatus,
    local_jobs::local_job_submit_response,
    loopback_graph_inputs::GraphExportInput,
    loopback_http::{loopback_app_error, loopback_error, require_loopback_scopes},
    loopback_state::LoopbackState,
};
use axum::{
    extract::{Path as AxumPath, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use std::sync::Arc;

pub(super) async fn loopback_hosted_export_graph(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
    Json(input): Json<GraphExportInput>,
) -> Response {
    if let Err(response) = require_loopback_scopes(&headers, &state, &["graphs.export", "rdf.dump"])
    {
        return response;
    }

    let export_format = match input
        .format
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("archive")
    {
        "archive" | "tar.gz" => "archive",
        "markdown-folder" | "markdown" | "obsidian" => "markdown-folder",
        other => {
            return loopback_error(
                StatusCode::BAD_REQUEST,
                &format!("unknown export format '{other}'; use 'archive' or 'markdown-folder'"),
            )
        }
    };
    let record = match state.jobs.insert_queued(
        "export_graph",
        Some(graph_id.clone()),
        serde_json::json!({
            "include_artifacts": input.include_artifacts,
            "includeArtifacts": input.include_artifacts,
            "format": export_format,
        }),
    ) {
        Ok(record) => record,
        Err(error) => return loopback_app_error(error),
    };

    let app = state.app.clone();
    let jobs = state.jobs.clone();
    let job_id = record.job_id.clone();
    let include_artifacts = input.include_artifacts;
    std::thread::spawn(move || {
        if jobs.is_cancelled(&job_id).unwrap_or(false) {
            return;
        }
        match jobs.mark_running(&job_id) {
            Ok(Some(record)) if matches!(record.status, LocalJobStatus::Running) => {}
            Ok(Some(_)) | Ok(None) => return,
            Err(error) => {
                let _ = jobs.finish_existing(&job_id, Err(error.into()), "application/json");
                return;
            }
        }
        let built = if export_format == "markdown-folder" {
            crate::markdown_folder_export::build_markdown_folder_export(&app, &graph_id).and_then(
                |built| {
                    let mut value = serde_json::to_value(&built)
                        .map_err(|error| format!("serialize export result: {error}"))?;
                    if let Some(object) = value.as_object_mut() {
                        object.insert("format".into(), "markdown-folder".into());
                    }
                    Ok((built.archive_file, built.archive_size_bytes, value))
                },
            )
        } else {
            build_graph_archive(&app, &graph_id, include_artifacts).and_then(|built| {
                let mut value = serde_json::to_value(&built)
                    .map_err(|error| format!("serialize export result: {error}"))?;
                if let Some(object) = value.as_object_mut() {
                    object.insert("format".into(), "archive".into());
                }
                Ok((built.archive_file, built.archive_size_bytes, value))
            })
        };
        let result = built.and_then(|(archive_file, archive_size_bytes, mut value)| {
            let token = write_export_access_token(&app, &archive_file)?;
            let download_url = format!(
                "/graphs/{}/export/download?file={}&token={}",
                urlencoding_component(&graph_id),
                urlencoding_component(&archive_file),
                urlencoding_component(&token),
            );
            if let Some(object) = value.as_object_mut() {
                object.insert("download_url".into(), download_url.clone().into());
                object.insert("downloadUrl".into(), download_url.into());
                object.insert("archive_size_bytes".into(), archive_size_bytes.into());
            }
            Ok(value)
        });
        if jobs.is_cancelled(&job_id).unwrap_or(false) {
            return;
        }
        if let Err(error) = jobs.finish_existing(&job_id, result, "application/json") {
            log::error!("Failed to finish graph export job {job_id}: {error}");
        }
    });

    (
        StatusCode::ACCEPTED,
        Json(local_job_submit_response(&record)),
    )
        .into_response()
}

/// Minimal percent-encoding for the URL components we emit (job-generated
/// filenames and UUID tokens are already URL-safe; graph ids are validated
/// local ids). Encodes anything outside unreserved characters defensively.
fn urlencoding_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char)
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

#[derive(Debug, Deserialize)]
pub(super) struct ExportDownloadQuery {
    #[serde(default)]
    file: String,
    #[serde(default)]
    token: String,
}

pub(super) async fn loopback_hosted_export_download(
    State(state): State<Arc<LoopbackState>>,
    AxumPath(graph_id): AxumPath<String>,
    Query(query): Query<ExportDownloadQuery>,
) -> Response {
    // Defense in depth alongside the token: the requested archive must
    // belong to the graph named in the path (archive filenames are
    // `{safe_filename(graph_id)}-{timestamp}.tar.gz`), so a token for one
    // graph's export can never fetch another's through a different URL.
    // The timestamp follows the graph id, so graph "a" cannot name graph
    // "a-b"'s file by prefix alone.
    let expected_prefix = format!("{}-", crate::ids::safe_filename(&graph_id));
    let after_prefix = query
        .file
        .strip_prefix(&expected_prefix)
        .and_then(|rest| rest.chars().next());
    if !after_prefix.is_some_and(|c| c.is_ascii_digit()) {
        return loopback_error(
            StatusCode::FORBIDDEN,
            "Export archive does not belong to this graph.",
        );
    }
    if !export_access_token_matches(&state.app, &query.file, &query.token) {
        return loopback_error(
            StatusCode::FORBIDDEN,
            "Export download token is missing, invalid, or expired.",
        );
    }
    let (path, content_type) = match resolve_export_archive(&state.app, &query.file) {
        Some(path) => (path, "application/gzip"),
        None => {
            match crate::markdown_folder_export::resolve_markdown_export(&state.app, &query.file) {
                Some(path) => (path, "application/zip"),
                None => return loopback_error(StatusCode::NOT_FOUND, "Export archive not found."),
            }
        }
    };
    match std::fs::read(&path) {
        Ok(bytes) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, content_type.to_string()),
                (
                    header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"{}\"", query.file),
                ),
            ],
            bytes,
        )
            .into_response(),
        Err(error) => loopback_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("read export archive: {error}"),
        ),
    }
}
