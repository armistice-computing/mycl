use crate::app_runtime::AppHandle;
use crate::{
    crdt_projection_flush::flush_graph_projection,
    crdt_queue::{enqueue_crdt_operation, EnqueueCrdtOperationInput},
    graph_projection_service::hosted_graph_entry_from_record,
    graph_service::{update_graph_metadata_service_async, UpdateGraphMetadataInput},
    json_utils::json_string,
    mcp_utils::{mcp_arg_string, mcp_graph_id_or_default, mcp_required_graph_id},
    mcp_workspace_payloads::{edit_comment_payload, wire_create_payload, wire_inputs},
};

pub(super) async fn mcp_local_rename(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_required_graph_id(arguments)?;
    let entity_type = mcp_arg_string(arguments, &["entity_type", "entityType", "type"])
        .ok_or_else(|| "entity_type is required".to_string())?
        .to_ascii_lowercase();
    let new_name = mcp_arg_string(arguments, &["new_name", "newName", "name", "title"])
        .ok_or_else(|| "new_name is required".to_string())?;

    match entity_type.as_str() {
        "graph" => {
            let graph = update_graph_metadata_service_async(
                &app,
                graph_id.clone(),
                UpdateGraphMetadataInput {
                    title: Some(new_name.clone()),
                    description: graph_description_argument(arguments),
                },
            )
            .await
            .map_err(crate::app_error::AppError::message)?;
            Ok(serde_json::json!({
                "success": true,
                "entity_type": "graph",
                "graph_id": graph_id,
                "new_name": new_name,
                "graph": hosted_graph_entry_from_record(graph, true),
            }))
        }
        "document" => {
            let entity_id = mcp_arg_string(
                arguments,
                &["entity_id", "entityId", "document_id", "documentId"],
            )
            .ok_or_else(|| "entity_id is required for document rename".to_string())?;
            let value = enqueue_crdt_operation(
                app.clone(),
                EnqueueCrdtOperationInput {
                    kind: "workspace.updateDocument".to_string(),
                    graph_id: graph_id.clone(),
                    document_id: Some(entity_id.clone()),
                    payload: serde_json::json!({
                        "documentId": entity_id,
                        "title": new_name,
                    }),
                },
            )
            .await?;
            flush_graph_projection(app, &graph_id).await?;
            Ok(serde_json::json!({
                "success": true,
                "entity_type": "document",
                "entity_id": json_string(value.get("documentId").or_else(|| value.get("id"))),
                "graph_id": graph_id,
                "new_name": json_string(value.get("title")),
                "value": value,
            }))
        }
        "folder" => {
            let entity_id = mcp_arg_string(
                arguments,
                &["entity_id", "entityId", "folder_id", "folderId"],
            )
            .ok_or_else(|| "entity_id is required for folder rename".to_string())?;
            let value = enqueue_crdt_operation(
                app.clone(),
                EnqueueCrdtOperationInput {
                    kind: "workspace.updateFolder".to_string(),
                    graph_id: graph_id.clone(),
                    document_id: Some(entity_id.clone()),
                    payload: serde_json::json!({
                        "folderId": entity_id,
                        "name": new_name,
                    }),
                },
            )
            .await?;
            flush_graph_projection(app, &graph_id).await?;
            Ok(serde_json::json!({
                "success": true,
                "entity_type": "folder",
                "entity_id": json_string(value.get("folderId").or_else(|| value.get("id"))),
                "graph_id": graph_id,
                "new_name": json_string(value.get("name")),
                "value": value,
            }))
        }
        _ => Err("entity_type must be graph, document, or folder".to_string()),
    }
}

pub(super) async fn mcp_local_edit_comment(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_graph_id_or_default(&app, arguments)?;
    let document_id = mcp_arg_string(arguments, &["document_id", "documentId"])
        .ok_or_else(|| "document_id is required".to_string())?;
    let comment_id = mcp_arg_string(arguments, &["comment_id", "commentId"])
        .ok_or_else(|| "comment_id is required".to_string())?;
    let action = mcp_arg_string(arguments, &["action"])
        .ok_or_else(|| "action is required".to_string())?
        .to_ascii_lowercase();
    if action != "set" && action != "resolve" && action != "delete" {
        return Err("action must be set, resolve, or delete".to_string());
    }
    if action == "set" && mcp_arg_string(arguments, &["text"]).is_none() {
        return Err("text is required for action=set".to_string());
    }

    let mut value = enqueue_crdt_operation(
        app,
        EnqueueCrdtOperationInput {
            kind: "document.editComment".to_string(),
            graph_id: graph_id.clone(),
            document_id: Some(document_id.clone()),
            payload: edit_comment_payload(arguments, &document_id, &comment_id, &action),
        },
    )
    .await?;
    if let Some(object) = value.as_object_mut() {
        object.insert("success".to_string(), serde_json::Value::Bool(true));
        object.insert(
            "graph_id".to_string(),
            serde_json::Value::String(graph_id.clone()),
        );
        object.insert("graphId".to_string(), serde_json::Value::String(graph_id));
        object.insert(
            "document_id".to_string(),
            serde_json::Value::String(document_id.clone()),
        );
        object.insert(
            "documentId".to_string(),
            serde_json::Value::String(document_id),
        );
        object.insert(
            "comment_id".to_string(),
            serde_json::Value::String(comment_id.clone()),
        );
        object.insert(
            "commentId".to_string(),
            serde_json::Value::String(comment_id),
        );
    }
    Ok(value)
}

async fn resolve_artifact_path_target(
    app: &AppHandle,
    graph_id: &str,
    wire_input: &mut serde_json::Value,
    arguments: &serde_json::Value,
) -> Result<(), String> {
    let path = crate::mcp_utils::mcp_arg_string_with_fallback(
        wire_input,
        arguments,
        &["artifact_path", "artifactPath"],
    )
    .map(|path| path.trim().to_string())
    .filter(|path| !path.is_empty());
    let Some(path) = path else {
        return Ok(());
    };
    let has_id = ["target_artifact_id", "targetArtifactId", "target_document_id", "targetDocumentId"]
        .iter()
        .any(|key| wire_input.get(*key).and_then(serde_json::Value::as_str).is_some());
    if has_id {
        return Err("give artifactPath or a target id, not both".to_string());
    }
    let artifact_id = crate::artifact_reference::find_or_reference_by_path(app.clone(), graph_id, &path).await?;
    if let Some(object) = wire_input.as_object_mut() {
        object.remove("artifact_path");
        object.remove("artifactPath");
        object.insert("targetArtifactId".to_string(), serde_json::Value::String(artifact_id));
    }
    Ok(())
}

pub(super) async fn mcp_local_create_wires(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_graph_id_or_default(&app, arguments)?;
    let mut wire_inputs = wire_inputs(arguments);
    if wire_inputs.is_empty() {
        return Err("wires must contain at least one wire object".to_string());
    }
    // The link form: `{ artifactPath, region }` names a file under the
    // owner's roots. One file has one referenced record however many wires
    // point at it, so the path finds the existing record or makes one, and
    // the wire then targets it by id like any other (design seed, step 3).
    for (index, wire_input) in wire_inputs.iter_mut().enumerate() {
        resolve_artifact_path_target(&app, &graph_id, wire_input, arguments)
            .await
            .map_err(|error| format!("wire {index}: {error}; no wires were created"))?;
    }

    // Build and check every wire before creating any, so a bad predicate in a
    // batch creates nothing. This is validation, not a transaction: a later
    // failure (authorization, journal, timeout) can still stop the batch
    // part-way, and then the error names the wires already created.
    let mut payloads = Vec::with_capacity(wire_inputs.len());
    for (index, wire_input) in wire_inputs.iter().enumerate() {
        let payload = wire_create_payload(wire_input, arguments, &graph_id).map_err(|error| {
            format!("wire {index}: {error}; no wires were created")
        })?;
        payloads.push(payload);
    }

    let total = payloads.len();
    let mut created = Vec::new();
    for (source_document_id, payload) in payloads {
        let value = enqueue_crdt_operation(
            app.clone(),
            EnqueueCrdtOperationInput {
                kind: "workspace.createWire".to_string(),
                graph_id: graph_id.clone(),
                document_id: Some(source_document_id),
                payload,
            },
        )
        .await
        .map_err(|error| {
            if created.is_empty() {
                error
            } else {
                let ids: Vec<&str> = created
                    .iter()
                    .filter_map(|wire: &serde_json::Value| wire.get("id").and_then(serde_json::Value::as_str))
                    .collect();
                format!(
                    "{error}; {} of {total} wires were created before this one and are kept: {ids:?}; \
                     do not resend them",
                    created.len()
                )
            }
        })?;
        created.push(value);
    }
    let created_count = created.len();
    // The wires are committed; a projection flush that fails now is reported
    // as a warning, not as a failed call. Reporting committed data as a
    // failure invites a retry that duplicates it, which is how the outage of
    // 2026-10-15 fed itself (angel review F2). The next flush catches up.
    let warning = flush_graph_projection(app, &graph_id)
        .await
        .err()
        .map(|error| format!("wires created; the search projection will catch up later: {error}"));

    let mut response = serde_json::json!({
        "success": true,
        "graph_id": graph_id.clone(),
        "graphId": graph_id,
        "wires": created,
        "count": created_count,
        "created_count": created_count,
        "createdCount": created_count,
    });
    if let Some(warning) = warning {
        response["warnings"] = serde_json::json!([warning]);
    }
    Ok(response)
}

#[cfg(all(test, feature = "headless"))]
mod tests {
    use super::*;

    #[test]
    fn async_generic_graph_rename_does_not_block_current_thread_while_lease_is_held() {
        let outcome = crate::mcp_graph_service::async_graph_alias_test_support::assert_alias_yields_while_graph_lease_is_held(
            "garden-mcp-generic-rename-alias-lease",
            "mcp-generic-rename-alias-held-lease",
            |app, graph_id| async move {
                let arguments = serde_json::json!({
                    "graph_id": graph_id,
                    "entity_type": "graph",
                    "new_name": "After generic rename",
                });
                mcp_local_rename(app, &arguments).await
            },
        );
        assert_eq!(outcome["success"], true);
        assert_eq!(outcome["new_name"], "After generic rename");
        assert_eq!(outcome["graph"]["title"], "After generic rename");
    }
}

/// The optional `description` a graph rename may carry: absent leaves it
/// alone, `null` clears it, a string sets it. (Moved from the retired
/// `update_graph` tool on 2026-10-28; `rename` is the one naming tool.)
fn graph_description_argument(arguments: &serde_json::Value) -> Option<String> {
    if arguments
        .get("description")
        .is_some_and(serde_json::Value::is_null)
    {
        Some(String::new())
    } else {
        mcp_arg_string(arguments, &["description"])
    }
}

#[cfg(test)]
mod graph_description_tests {
    use super::graph_description_argument;

    #[test]
    fn graph_description_argument_preserves_null_semantics() {
        assert_eq!(graph_description_argument(&serde_json::json!({})), None);
        assert_eq!(
            graph_description_argument(&serde_json::json!({ "description": null })),
            Some(String::new())
        );
        assert_eq!(
            graph_description_argument(&serde_json::json!({ "description": "  A graph  " })).as_deref(),
            Some("A graph")
        );
    }
}
