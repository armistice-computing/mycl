use crate::app_runtime::AppHandle;
use crate::{
    crdt_queue::{enqueue_crdt_operation, EnqueueCrdtOperationInput},
    mcp_utils::{
        mcp_block_delete_payload, mcp_block_edit_text_payload, mcp_block_insert_payload,
        mcp_block_target, mcp_block_update_payload,
    },
};

pub(super) async fn mcp_local_insert_blocks(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let (graph_id, document_id) = mcp_block_target(arguments)?;
    // Step 4: the blocks come from an artifact region when the call names
    // one; the region is read through the storage gate and every block is
    // wired back to its own lines. The document is checked first, so a
    // pull into a document that is not there neither reads the file nor
    // makes a record for a path (angel review).
    let names_artifact = ["artifactId", "artifact_id", "artifactPath", "artifact_path"]
        .iter()
        .any(|key| arguments.get(*key).and_then(serde_json::Value::as_str).is_some_and(|value| !value.trim().is_empty()));
    if names_artifact {
        let graph_dir = crate::paths::existing_graph_dir(&app, &graph_id)?;
        if crate::crdt_engine::workspace_ops::document_record_json(&graph_dir, &document_id).is_none() {
            return Err(format!("document not found: {document_id}"));
        }
        let artifact_id = crate::artifact_pull::artifact_for_pull(&app, &graph_id, arguments)
            .await?
            .ok_or_else(|| "artifactId or artifactPath is empty".to_string())?;
        return insert_blocks_from_artifact(app, graph_id, document_id, artifact_id, arguments).await;
    }
    let payload = serde_json::to_value(mcp_block_insert_payload(arguments))
        .expect("McpBlockInsertPayload always serializes");
    enqueue_crdt_operation(
        app,
        EnqueueCrdtOperationInput {
            kind: "block.insert".to_string(),
            graph_id,
            document_id: Some(document_id),
            payload,
        },
    )
    .await
}

/// Pull a region into blocks (design seed, step 4): read and split on a
/// blocking thread, insert through `block.insert`, then one `cites` wire
/// per block to its region and one `authoredBy` on the document, all
/// through `workspace.createWire` so step 3's invariants hold (server
/// fileHash, capture, side store, cleanup).
async fn insert_blocks_from_artifact(
    app: AppHandle,
    graph_id: String,
    document_id: String,
    artifact_id: String,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    use crate::artifact_pull::{cites_payloads, document_has_authored_by, pull_blocks};
    for forbidden in ["content", "blocks", "tiptapJson", "tiptap_json"] {
        if arguments.get(forbidden).is_some_and(|value| !value.is_null()) {
            return Err(format!("{forbidden} and an artifact source cannot both be given; the artifact is the content"));
        }
    }
    let region = match arguments.get("region").filter(|value| !value.is_null()) {
        Some(value) => Some(crate::artifact_regions::Region::parse(value).map_err(|error| format!("region: {error}"))?),
        None => None,
    };
    let as_value = crate::mcp_utils::mcp_arg_string(arguments, &["as"]);
    let operation_id = crate::mcp_utils::mcp_arg_string(arguments, &["operationId", "operation_id"]);
    let pulled = {
        let app = app.clone();
        let graph_id = graph_id.clone();
        let artifact_id = artifact_id.clone();
        let operation_id = operation_id.clone();
        tokio::task::spawn_blocking(move || {
            pull_blocks(&app, &graph_id, &artifact_id, region, as_value, operation_id.as_deref())
        })
        .await
        .map_err(|error| format!("pull worker failed: {error}"))??
    };
    let mut insert_arguments = arguments.clone();
    if let Some(object) = insert_arguments.as_object_mut() {
        for key in ["artifactId", "artifact_id", "artifactPath", "artifact_path", "region", "as", "content", "format"] {
            object.remove(key);
        }
        object.insert("blocks".to_string(), serde_json::Value::Array(pulled.blocks.clone()));
    }
    let payload = serde_json::to_value(mcp_block_insert_payload(&insert_arguments))
        .expect("McpBlockInsertPayload always serializes");
    let mut inserted = enqueue_crdt_operation(
        app.clone(),
        EnqueueCrdtOperationInput {
            kind: "block.insert".to_string(),
            graph_id: graph_id.clone(),
            document_id: Some(document_id.clone()),
            payload,
        },
    )
    .await?;
    let block_ids: Vec<String> = inserted
        .get("block_ids")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|id| id.as_str().map(str::to_string))
        .collect();
    // The blocks are in; from here nothing fails the call (a failure now
    // would invite a retry that duplicates committed blocks, the 2026-10-15
    // pattern). Wires that fail are reported beside their block, and a
    // block that already has an active cites wire (a replayed operation)
    // is not wired twice.
    let mut wire_errors = Vec::new();
    let snapshot = crate::paths::existing_graph_dir(&app, &graph_id)
        .and_then(|graph_dir| crate::document_service::read_workspace_record(&graph_dir, &graph_id))
        .map(|workspace| workspace.snapshot)
        .unwrap_or_else(|error| {
            wire_errors.push(format!("could not read the workspace before wiring: {error}"));
            None
        });
    let already = crate::artifact_pull::blocks_already_cited(snapshot.as_ref(), &document_id, &artifact_id);
    let mut cites = Vec::new();
    for (block_id, payload) in block_ids.iter().zip(cites_payloads(&document_id, &block_ids, &pulled)) {
        if already.contains(block_id) {
            cites.push(serde_json::json!({ "blockId": block_id, "existing": true }));
            continue;
        }
        match enqueue_crdt_operation(
            app.clone(),
            EnqueueCrdtOperationInput {
                kind: "workspace.createWire".to_string(),
                graph_id: graph_id.clone(),
                document_id: Some(document_id.clone()),
                payload,
            },
        )
        .await
        {
            Ok(wire) => cites.push(serde_json::json!({ "blockId": block_id, "wireId": wire.get("id").cloned().unwrap_or(serde_json::Value::Null) })),
            Err(error) => {
                wire_errors.push(format!("cites wire for block {block_id}: {error}"));
                cites.push(serde_json::json!({ "blockId": block_id, "error": error }));
            }
        }
    }
    let mut authored_by = serde_json::Value::Null;
    if !document_has_authored_by(snapshot.as_ref(), &document_id, &artifact_id) {
        match enqueue_crdt_operation(
            app.clone(),
            EnqueueCrdtOperationInput {
                kind: "workspace.createWire".to_string(),
                graph_id: graph_id.clone(),
                document_id: Some(document_id.clone()),
                payload: serde_json::json!({
                    "sourceDocumentId": document_id,
                    "targetArtifactId": artifact_id,
                    "predicate": "authoredBy",
                }),
            },
        )
        .await
        {
            Ok(wire) => authored_by = wire.get("id").cloned().unwrap_or(serde_json::Value::Null),
            Err(error) => wire_errors.push(error),
        }
    }
    let warning = crate::crdt_projection_flush::flush_graph_projection(app, &graph_id)
        .await
        .err()
        .map(|error| format!("blocks and wires created; the search projection will catch up later: {error}"));
    if let Some(object) = inserted.as_object_mut() {
        object.insert("artifactId".into(), serde_json::json!(pulled.artifact_id));
        object.insert("filename".into(), serde_json::json!(pulled.filename));
        object.insert("region".into(), pulled.region.as_ref().map(|region| region.to_json()).unwrap_or(serde_json::Value::Null));
        object.insert("as".into(), serde_json::json!(match pulled.as_kind {
            crate::artifact_pull::PullAs::Paragraphs => "paragraphs",
            crate::artifact_pull::PullAs::Code => "code",
            crate::artifact_pull::PullAs::Quote => "quote",
        }));
        object.insert("cites".into(), serde_json::Value::Array(cites));
        object.insert("authoredBy".into(), authored_by);
        let mut warnings: Vec<String> = wire_errors;
        if let Some(warning) = warning {
            warnings.push(warning);
        }
        if !warnings.is_empty() {
            object.insert("warnings".into(), serde_json::json!(warnings));
        }
    }
    Ok(inserted)
}

pub(super) async fn mcp_local_update_blocks(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let (graph_id, document_id) = mcp_block_target(arguments)?;
    let payload = serde_json::to_value(mcp_block_update_payload(arguments))
        .expect("McpBlockUpdatePayload always serializes");
    enqueue_crdt_operation(
        app,
        EnqueueCrdtOperationInput {
            kind: "block.update".to_string(),
            graph_id,
            document_id: Some(document_id),
            payload,
        },
    )
    .await
}

pub(super) async fn mcp_local_edit_block_text(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let (graph_id, document_id) = mcp_block_target(arguments)?;
    let payload = serde_json::to_value(mcp_block_edit_text_payload(arguments))
        .expect("McpBlockEditTextPayload always serializes");
    enqueue_crdt_operation(
        app,
        EnqueueCrdtOperationInput {
            kind: "block.editText".to_string(),
            graph_id,
            document_id: Some(document_id),
            payload,
        },
    )
    .await
}

pub(super) async fn mcp_local_delete_blocks(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let (graph_id, document_id) = mcp_block_target(arguments)?;
    let payload = serde_json::to_value(mcp_block_delete_payload(arguments))
        .expect("McpBlockDeletePayload always serializes");
    enqueue_crdt_operation(
        app,
        EnqueueCrdtOperationInput {
            kind: "block.delete".to_string(),
            graph_id,
            document_id: Some(document_id),
            payload,
        },
    )
    .await
}
