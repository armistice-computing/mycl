//! Fidelity report route — the migration gate.
//!
//! POST /graphs/{graph_id}/fidelity-report submits an async job that diffs a
//! local graph against the archive it was imported from (and, optionally, a
//! cloud snapshot of the graph the archive was exported from). See
//! `graph_fidelity_report` and `docs/fidelity-report.md`.
//!
//! The archive and the cloud snapshot are named by absolute local paths, not
//! uploaded: this is an owner-process loopback on the same machine, and the
//! archive is typically hundreds of megabytes.

use crate::{
    graph_fidelity_report::{build_fidelity_report, load_cloud_snapshot},
    local_job_types::LocalJobStatus,
    local_jobs::local_job_submit_response,
    loopback_graph_inputs::GraphFidelityReportInput,
    loopback_http::{loopback_app_error, loopback_error, require_loopback_owner},
    loopback_state::LoopbackState,
};
use axum::{
    extract::{Path as AxumPath, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn absolute_existing_file(value: &str, what: &str) -> Result<PathBuf, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(format!("{what} is required"));
    }
    let path = Path::new(trimmed);
    if !path.is_absolute() {
        return Err(format!("{what} must be an absolute path"));
    }
    if !path.is_file() {
        return Err(format!("{what} does not exist: {trimmed}"));
    }
    Ok(path.to_path_buf())
}

pub(super) async fn loopback_hosted_fidelity_report(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
    Json(input): Json<GraphFidelityReportInput>,
) -> Response {
    if let Err(response) = require_loopback_owner(&headers, &state) {
        return response;
    }
    let archive_path = match absolute_existing_file(&input.archive_path, "archive_path") {
        Ok(path) => path,
        Err(error) => return loopback_error(StatusCode::BAD_REQUEST, &error),
    };
    let cloud_snapshot_path = match input
        .cloud_snapshot_path
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(|value| absolute_existing_file(value, "cloud_snapshot_path"))
        .transpose()
    {
        Ok(path) => path,
        Err(error) => return loopback_error(StatusCode::BAD_REQUEST, &error),
    };

    let record = match state.jobs.insert_queued(
        "fidelity_report",
        Some(graph_id.clone()),
        serde_json::json!({
            "archive_path": archive_path.display().to_string(),
            "cloud_snapshot_path": cloud_snapshot_path.as_ref().map(|path| path.display().to_string()),
        }),
    ) {
        Ok(record) => record,
        Err(error) => return loopback_app_error(error),
    };

    let app = state.app.clone();
    let jobs = state.jobs.clone();
    let job_id = record.job_id.clone();
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
        let result = (|| {
            let archive_bytes = std::fs::read(&archive_path)
                .map_err(|error| format!("read archive {}: {error}", archive_path.display()))?;
            let cloud = cloud_snapshot_path
                .as_deref()
                .map(load_cloud_snapshot)
                .transpose()?;
            build_fidelity_report(&app, &graph_id, &archive_bytes, cloud)
        })();
        if jobs.is_cancelled(&job_id).unwrap_or(false) {
            return;
        }
        if let Err(error) = jobs.finish_existing(&job_id, result, "application/json") {
            log::error!("Failed to finish fidelity report job {job_id}: {error}");
        }
    });

    (
        StatusCode::ACCEPTED,
        Json(local_job_submit_response(&record)),
    )
        .into_response()
}
