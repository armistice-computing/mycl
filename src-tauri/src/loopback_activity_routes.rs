//! Owner-only routes for the human's document activity: record that the owner
//! opened a document, and list the recently opened ones. Agents hold scoped
//! connector tokens and are refused, so an agent reading a document never
//! counts as the human opening it.

use crate::{
    human_activity_store::{opened_documents, record_opened},
    loopback_http::{loopback_result, require_loopback_owner},
    loopback_state::LoopbackState,
};
use axum::{
    extract::{Path as AxumPath, Query, State},
    http::HeaderMap,
    response::Response,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc};

const DEFAULT_RECENT_LIMIT: usize = 20;
const MAX_RECENT_LIMIT: usize = 200;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OpenedRequest {
    #[serde(alias = "document_id")]
    document_id: String,
}

#[derive(Debug, Deserialize)]
struct RecentQuery {
    #[serde(default)]
    limit: Option<usize>,
}

pub(super) fn loopback_activity_router() -> Router<Arc<LoopbackState>> {
    Router::new()
        .route("/activity/{graph_id}/opened", post(loopback_record_opened))
        .route("/activity/{graph_id}/recent", get(loopback_recent_opened))
}

async fn loopback_record_opened(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
    Json(body): Json<OpenedRequest>,
) -> Response {
    if let Err(response) = require_loopback_owner(&headers, &state) {
        return response;
    }
    loopback_result((|| {
        let open = PendingOpen::begin(&state.app, &graph_id)?;
        let now = u64::try_from(crate::clock::epoch_millis()).unwrap_or(u64::MAX);
        let recorded = open.record(body.document_id.trim(), now)?;
        Ok(json!({ "ok": true, "recorded": recorded }))
    })())
}

/// An open validated against ONE incarnation of its graph. The graph's
/// identity (`incarnation_id`, minted at creation, with `created_at` for
/// legacy records) is captured at validation and compared again under the
/// activity lock just before the write. A hard delete removes the graph and
/// then forgets its activity under that same lock, so a stalled open can
/// neither recreate the forgotten record nor, if the id was recreated in the
/// meantime, land in the new graph's record: existence is not identity.
pub(crate) struct PendingOpen {
    profile_dir: std::path::PathBuf,
    graph_dir: std::path::PathBuf,
    graph_id: String,
    incarnation: (Option<String>, String),
}

impl PendingOpen {
    pub(crate) fn begin(
        app: &crate::app_runtime::AppHandle,
        graph_id: &str,
    ) -> Result<Self, String> {
        let graph_dir = crate::graph_paths::existing_graph_dir(app, graph_id)?;
        let incarnation =
            graph_incarnation(&graph_dir).ok_or_else(|| format!("graph not found: {graph_id}"))?;
        Ok(Self {
            profile_dir: crate::profile_paths::profile_dir(app)?,
            graph_dir,
            graph_id: graph_id.to_string(),
            incarnation,
        })
    }

    pub(crate) fn record(&self, document_id: &str, now: u64) -> Result<bool, String> {
        record_opened(&self.profile_dir, &self.graph_id, document_id, now, || {
            graph_incarnation(&self.graph_dir).as_ref() == Some(&self.incarnation)
        })
    }
}

fn graph_incarnation(graph_dir: &std::path::Path) -> Option<(Option<String>, String)> {
    let record = crate::storage::read_json::<crate::graph_record_store::GraphRecord>(
        &graph_dir.join("graph.json"),
    )
    .ok()?;
    Some((record.incarnation_id, record.created_at))
}

async fn loopback_recent_opened(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    AxumPath(graph_id): AxumPath<String>,
    Query(query): Query<RecentQuery>,
) -> Response {
    if let Err(response) = require_loopback_owner(&headers, &state) {
        return response;
    }
    let limit = query
        .limit
        .unwrap_or(DEFAULT_RECENT_LIMIT)
        .clamp(1, MAX_RECENT_LIMIT);
    loopback_result((|| {
        let graph_dir = crate::graph_paths::existing_graph_dir(&state.app, &graph_id)?;
        let profile_dir = crate::profile_paths::profile_dir(&state.app)?;
        let titles = workspace_titles(&graph_dir);
        let mut documents = Vec::new();
        for (document_id, record) in opened_documents(&profile_dir, &graph_id)? {
            if documents.len() >= limit {
                break;
            }
            // A deleted document leaves the list; the record stays until the
            // cap drops it, so a restored document keeps its history.
            if crate::document_tombstone_store::document_is_tombstoned(&graph_dir, &document_id)
                .unwrap_or(false)
            {
                continue;
            }
            documents.push(json!({
                "documentId": document_id,
                "title": titles.get(&document_id),
                "lastOpenedAt": record.last_opened_at,
                "openCount": record.open_count,
            }));
        }
        Ok(json!({ "graphId": graph_id, "documents": documents }))
    })())
}

/// Titles from the workspace snapshot. The snapshot can lag a document created
/// moments ago, so a missing title is null rather than a reason to drop it.
fn workspace_titles(graph_dir: &std::path::Path) -> HashMap<String, Value> {
    let Ok(Some(snapshot)) = crate::document_access::workspace_snapshot_for_read(graph_dir) else {
        return HashMap::new();
    };
    snapshot["documents"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let id = entry.get("id")?.as_str()?.to_string();
            Some((id, entry.get("title").cloned().unwrap_or(Value::Null)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::PendingOpen;
    use crate::graph_service::CreateGraphInput;
    use crate::graph_service::{create_graph_service, soft_delete_graph_service};

    fn create(app: &crate::app_runtime::AppHandle, graph_id: &str, title: &str) {
        create_graph_service(
            app,
            CreateGraphInput {
                title: title.to_string(),
                graph_id: Some(graph_id.to_string()),
                description: None,
                operation_id: None,
            },
        )
        .expect("create graph");
    }

    #[test]
    fn an_open_stalled_across_delete_and_recreate_records_nothing() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile =
            std::env::temp_dir().join(format!("garden-activity-aba-{}", uuid::Uuid::new_v4()));
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);
        let result = std::panic::catch_unwind(|| {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "activity-aba";
            create(&app, graph_id, "Old incarnation");
            let old = PendingOpen::begin(&app, graph_id).expect("validate against the old graph");
            assert!(old
                .record("doc-before", 1_000_000)
                .expect("an ordinary open records"));

            // The open stalls here while the graph is hard-deleted and an
            // unrelated graph is created under the same id.
            soft_delete_graph_service(&app, graph_id.to_string(), true).expect("hard delete");
            create(&app, graph_id, "New incarnation");

            assert!(
                old.record("doc-late", 2_000_000).is_err(),
                "a stale open must not land"
            );
            let fresh = PendingOpen::begin(&app, graph_id).expect("validate against the new graph");
            let opened =
                crate::human_activity_store::opened_documents(&fresh.profile_dir, graph_id)
                    .expect("read activity");
            assert!(
                opened.is_empty(),
                "the new graph inherits nothing: {opened:?}"
            );
            assert!(fresh
                .record("doc-new", 3_000_000)
                .expect("the new graph records"));
        });
        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }
}
