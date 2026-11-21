use crate::app_runtime::AppHandle;
use crate::{
    document_service::read_workspace_record,
    document_tombstone_store::document_is_tombstoned,
    json_utils::{json_bool, json_string},
    mcp_utils::{mcp_required_document_id, mcp_required_graph_id},
    paths::existing_graph_dir,
    workspace_entity_projection::{workspace_documents, workspace_entity_id},
};

pub(super) fn mcp_local_check_document(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_required_graph_id(arguments)?;
    let document_id = mcp_required_document_id(arguments)?;
    let graph_dir = existing_graph_dir(&app, &graph_id)?;

    if document_is_tombstoned(&graph_dir, &document_id)? {
        return Ok(serde_json::json!({
            "graph_id": graph_id,
            "document_id": document_id,
            "status": "tombstoned",
        }));
    }

    let workspace = read_workspace_record(&graph_dir, &graph_id)?;
    let documents = workspace_documents(workspace.snapshot.as_ref());
    if let Some(document) = documents
        .iter()
        .find(|candidate| workspace_entity_id(candidate).as_deref() == Some(document_id.as_str()))
    {
        let title = json_string(document.get("title")).unwrap_or_else(|| document_id.clone());
        let read_only = json_bool(
            document
                .get("readOnly")
                .or_else(|| document.get("read_only")),
        )
        .unwrap_or(false);
        return Ok(serde_json::json!({
            "graph_id": graph_id,
            "document_id": document_id,
            "status": "exists",
            "title": title,
            "read_only": read_only,
        }));
    }

    Ok(serde_json::json!({
        "graph_id": graph_id,
        "document_id": document_id,
        "status": "available",
    }))
}

#[cfg(all(test, feature = "headless", not(feature = "desktop")))]
mod tests {
    use super::*;
    use crate::{
        document_tombstone_store::write_document_tombstone_for_operation,
        graph_service::{create_graph_service, CreateGraphInput},
        paths::existing_graph_dir,
    };
    use uuid::Uuid;

    #[test]
    fn check_document_distinguishes_available_and_tombstoned_ids() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile =
            std::env::temp_dir().join(format!("garden-check-document-{}", Uuid::new_v4()));
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let result = std::panic::catch_unwind(|| {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "check-document-graph";
            create_graph_service(
                &app,
                CreateGraphInput {
                    title: "Check Document".to_string(),
                    graph_id: Some(graph_id.to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");

            let available = mcp_local_check_document(
                app.clone(),
                &serde_json::json!({
                    "graph_id": graph_id,
                    "document_id": "available-document",
                }),
            )
            .expect("check available document");
            assert_eq!(available["status"], "available");

            let graph_dir = existing_graph_dir(&app, graph_id).expect("graph directory");
            write_document_tombstone_for_operation(
                &graph_dir,
                "deleted-document",
                Some("delete-operation"),
            )
            .expect("write document tombstone");
            let tombstoned = mcp_local_check_document(
                app,
                &serde_json::json!({
                    "graph_id": graph_id,
                    "document_id": "deleted-document",
                }),
            )
            .expect("check tombstoned document");
            assert_eq!(tombstoned["status"], "tombstoned");
        });

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }
}
