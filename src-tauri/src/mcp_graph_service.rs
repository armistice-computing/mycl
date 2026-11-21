use crate::app_runtime::AppHandle;
use crate::{
    document_service::read_graph_documents_cold,
    graph_duplicate_service::duplicate_graph,
    graph_maintenance_service::{reindex_graph_job_response, ReindexGraphInput},
    graph_projection_service::{hosted_graph_entry_from_record, hosted_workspace_summary},
    graph_service::{
        create_graph_service_async, create_graph_service_async_with_incarnation, read_graph_record,
        soft_delete_graph_service_async, CreateGraphInput,
    },
    local_jobs::LocalJobRegistry,
    mcp_utils::{mcp_arg_bool, mcp_arg_string, mcp_required_graph_id},
    paths::existing_graph_dir,
    semantic_service::semantic_model_status,
};
use std::sync::Arc;

pub(super) async fn mcp_local_create_graph(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let title =
        mcp_arg_string(arguments, &["title"]).ok_or_else(|| "title is required".to_string())?;
    let graph_id = mcp_arg_string(arguments, &["graph_id", "graphId"]);
    let description = create_graph_description_argument(arguments);
    let operation_id = mcp_arg_string(arguments, &["operation_id", "operationId"]);
    let graph_incarnation = mcp_arg_string(arguments, &["graph_incarnation", "graphIncarnation"]);
    let input = CreateGraphInput {
        title,
        graph_id,
        description,
        operation_id,
    };
    let result = match graph_incarnation {
        Some(graph_incarnation) => {
            create_graph_service_async_with_incarnation(&app, input, graph_incarnation).await
        }
        None => create_graph_service_async(&app, input).await,
    };
    result
        .map_err(crate::app_error::AppError::message)
        .map(|graph| hosted_graph_entry_from_record(graph, true))
}

pub(super) async fn mcp_local_duplicate_graph(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let source_graph_id = mcp_arg_string(arguments, &["source_graph_id", "sourceGraphId"])
        .ok_or_else(|| "source_graph_id is required".to_string())?;
    let new_graph_id = mcp_arg_string(arguments, &["new_graph_id", "newGraphId"])
        .ok_or_else(|| "new_graph_id is required".to_string())?;
    let new_title = mcp_arg_string(arguments, &["new_title", "newTitle"]);

    duplicate_graph(app, source_graph_id, new_graph_id, new_title).await
}

pub(super) async fn mcp_local_manage_graph(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_required_graph_id(arguments)?;
    let action = mcp_arg_string(arguments, &["action"])
        .ok_or_else(|| "action is required".to_string())?
        .trim()
        .to_ascii_lowercase();
    match action.as_str() {
        "read" => {
            let (_, graph) = read_graph_record(&app, &graph_id)?;
            Ok(hosted_graph_entry_from_record(graph, true))
        }
        "stats" => {
            let mut summary = hosted_workspace_summary(app, &graph_id)?;
            let triple_count = summary
                .get("edge_count")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            if let Some(object) = summary.as_object_mut() {
                object.insert("status".to_string(), serde_json::json!("ok"));
                object.insert("action".to_string(), serde_json::json!("stats"));
                object.insert("triple_count".to_string(), triple_count.clone());
                object.insert("tripleCount".to_string(), triple_count);
            }
            Ok(summary)
        }
        "delete" => {
            // Hard by default, matching the generic `delete(type=graph)` tool
            // and `DELETE /graphs/{id}`: on a local node a tombstone is just
            // hidden disk. Pass `hard: false` to keep the directory.
            let hard = mcp_arg_bool(arguments, &["hard"], true);
            soft_delete_graph_service_async(&app, graph_id.clone(), hard)
                .await
                .map_err(crate::app_error::AppError::message)
                .map(|value| {
                    serde_json::json!({
                        "status": "deleted",
                        "success": true,
                        "graph_id": graph_id.clone(),
                        "graphId": graph_id,
                        "hard": hard,
                        "value": value,
                    })
                })
        }
        _ => Err("action must be read, stats, or delete".to_string()),
    }
}

pub(super) fn mcp_local_reindex_graph(
    app: AppHandle,
    jobs: Arc<LocalJobRegistry>,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_required_graph_id(arguments)?;
    let model_status = semantic_model_status(&app)?;
    if model_status.setup_required {
        let graph_dir = existing_graph_dir(&app, &graph_id)?;
        let document_count = read_graph_documents_cold(&graph_dir)?.len();
        return Ok(serde_json::json!({
            "graph_id": graph_id,
            "total_docs": document_count,
            "queued": 0,
            "skipped": true,
            "setup_required": true,
            "reason": "local embedding model is not prepared",
        }));
    }
    reindex_graph_job_response(app, jobs, ReindexGraphInput { graph_id })
}

fn create_graph_description_argument(arguments: &serde_json::Value) -> Option<String> {
    if arguments
        .get("description")
        .is_some_and(serde_json::Value::is_null)
    {
        None
    } else {
        mcp_arg_string(arguments, &["description"])
    }
}

#[cfg(all(test, feature = "headless"))]
pub(crate) mod async_graph_alias_test_support {
    use super::*;
    use std::{
        future::Future,
        panic::AssertUnwindSafe,
        sync::mpsc,
        thread,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };
    #[cfg(feature = "desktop")]
    use tauri::Manager;

    pub(crate) fn assert_alias_yields_while_graph_lease_is_held<F, Fut>(
        profile_prefix: &str,
        graph_id: &str,
        invoke: F,
    ) -> serde_json::Value
    where
        F: FnOnce(AppHandle, String) -> Fut + Send + 'static,
        Fut: Future<Output = Result<serde_json::Value, String>> + Send + 'static,
    {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let profile = std::env::temp_dir().join(format!("{profile_prefix}-{nanos}"));
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            crate::graph_service::create_graph_service(
                &app,
                CreateGraphInput {
                    title: "Before lifecycle alias".to_string(),
                    graph_id: Some(graph_id.to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");
            let held = app
                .state::<crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator>()
                .acquire_hot_write_blocking(graph_id)
                .expect("hold graph lease");

            let (started_tx, started_rx) = mpsc::channel();
            let (progress_tx, progress_rx) = mpsc::channel();
            let (result_tx, result_rx) = mpsc::channel();
            let app_for_alias = app.clone();
            let graph_id_for_alias = graph_id.to_string();
            let worker = thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("current-thread runtime");
                runtime.block_on(async move {
                    let (task_started_tx, task_started_rx) = tokio::sync::oneshot::channel();
                    let alias = tokio::spawn(async move {
                        let _ = started_tx.send(());
                        let _ = task_started_tx.send(());
                        invoke(app_for_alias, graph_id_for_alias).await
                    });

                    // The child sends immediately before entering the alias. If
                    // that alias calls acquire_blocking, it pins this only Tokio
                    // worker before the root future can observe the handshake.
                    task_started_rx.await.expect("alias task starts");
                    let _ = progress_tx.send(());
                    let outcome = alias
                        .await
                        .map_err(|error| format!("MCP alias task failed: {error}"))
                        .and_then(|result| result);
                    let _ = result_tx.send(outcome);
                });
            });

            let started = started_rx.recv_timeout(Duration::from_secs(5)).is_ok();
            let worker_progressed = progress_rx.recv_timeout(Duration::from_millis(500)).is_ok();
            drop(held);
            let outcome = result_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("MCP alias result after lease release");
            worker.join().expect("current-thread worker");

            assert!(
                started,
                "MCP alias task did not reach its bounded startup handshake"
            );
            assert!(
                worker_progressed,
                "MCP graph lifecycle alias blocked the current-thread Tokio worker on the graph lease"
            );
            outcome.expect("MCP graph lifecycle alias succeeds")
        }));

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        match result {
            Ok(value) => value,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graph_description_argument_preserves_create_null_semantics() {
        let omitted = serde_json::json!({});
        let empty = serde_json::json!({ "description": null });
        let value = serde_json::json!({ "description": "  Graph description  " });

        assert_eq!(create_graph_description_argument(&omitted), None);
        assert_eq!(create_graph_description_argument(&empty), None);
        assert_eq!(
            create_graph_description_argument(&value).as_deref(),
            Some("Graph description")
        );
    }

    #[cfg(feature = "headless")]
    #[test]
    fn async_mcp_graph_update_does_not_block_a_single_worker_while_lease_is_held() {
        let outcome = async_graph_alias_test_support::assert_alias_yields_while_graph_lease_is_held(
            "garden-mcp-update-alias-lease",
            "mcp-update-alias-held-lease",
            |app, graph_id| async move {
                let arguments = serde_json::json!({
                    "graph_id": graph_id,
                    "entity_type": "graph",
                    "new_name": "After update",
                });
                crate::mcp_workspace_mutation_service::mcp_local_rename(app, &arguments).await
            },
        );
        assert_eq!(outcome["new_name"], "After update");
    }

    #[cfg(feature = "headless")]
    #[test]
    fn async_mcp_manage_graph_delete_does_not_block_current_thread_while_lease_is_held() {
        let outcome = async_graph_alias_test_support::assert_alias_yields_while_graph_lease_is_held(
            "garden-mcp-manage-delete-alias-lease",
            "mcp-manage-delete-alias-held-lease",
            |app, graph_id| async move {
                // `hard` omitted on purpose: the default is a hard delete.
                let arguments = serde_json::json!({
                    "graph_id": graph_id,
                    "action": "delete",
                });
                mcp_local_manage_graph(app, &arguments).await
            },
        );
        assert_eq!(outcome["status"], "deleted");
        assert_eq!(outcome["hard"], true);
        assert_eq!(outcome["value"]["purged"], true);
    }

    #[test]
    fn manage_graph_delete_defaults_to_hard_and_honors_explicit_soft() {
        let omitted = serde_json::json!({ "graph_id": "g", "action": "delete" });
        let soft = serde_json::json!({ "graph_id": "g", "action": "delete", "hard": false });
        assert!(mcp_arg_bool(&omitted, &["hard"], true));
        assert!(!mcp_arg_bool(&soft, &["hard"], true));
    }
}
