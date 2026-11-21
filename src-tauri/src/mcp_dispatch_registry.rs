// Runtime dispatch registry for MCP `tools/call`. The MCP envelope's
// `tools/list` registry lives in `mcp_tool_registry.rs` (catalog + scope
// metadata); this module is the parallel surface that maps a tool name
// straight to its handler so the `tools/call` dispatcher does not carry
// an implementation table.
//
// Every entry is `(name, handler)`. Handlers are sync or async functions
// that return `AppResult<Value>`. Trampolines (`tramp_*`) bridge the
// existing handler shapes — `(AppHandle, &Value)`, `(&LocalJobRegistry,
// &Value)`, `(AppHandle, Arc<LocalJobRegistry>, &Value)`, etc. — into the
// uniform `Fn(&McpCallCtx, &Value) -> AppResult<Value>` shape without
// touching the handler bodies. Legacy `Result<Value, String>` handlers
// are funneled through `AppError::internal` at the trampoline so the
// dispatcher boundary needs only a single `String::from` shim.
//
// Adding a tool: write the handler, add a trampoline below, append an
// `entry!` row to `REGISTRY`, and add the catalog/scope rows in their
// respective sources of truth.

use crate::app_runtime::AppHandle;
use crate::{
    app_error::{AppError, AppResult},
    artifact_ingest_service::{mcp_local_ingest_artifact, mcp_local_upload_artifact},
    artifact_mcp_service::{
        mcp_local_list_artifact_kinds, mcp_local_list_artifact_revisions, mcp_local_read_artifact,
        mcp_local_reference_artifact, mcp_local_restore_artifact_revision,
    },
    document_digest_service::mcp_local_document_digest,
    document_history_mcp::{mcp_local_get_document_history, mcp_local_read_document_at_snapshot},
    document_mcp_blocks::{mcp_local_get_block, mcp_local_query_blocks, mcp_local_read_blocks},
    document_mcp_service::{mcp_local_list_documents, mcp_local_read_document},
    document_mutation_service::mcp_local_write_document,
    document_status_service::mcp_local_check_document,
    emporium_mcp_surface::{
        mcp_local_emporium_read, mcp_local_emporium_retract, mcp_local_emporium_write,
        mcp_local_sparql_query_named,
    },
    geist_diary_service::{mcp_local_diary, mcp_local_read_diary},
    geist_memory_recall_service::mcp_local_recall_memories_for,
    geist_memory_service::{
        mcp_local_archive_memories, mcp_local_remember, mcp_local_remember_batch,
    },
    geist_song_service::{mcp_local_music, mcp_local_sing},
    graph_catalog_projection::mcp_local_list_graphs,
    local_jobs::LocalJobRegistry,
    mcp_block_mutation_service::{
        mcp_local_delete_blocks, mcp_local_edit_block_text, mcp_local_insert_blocks,
        mcp_local_update_blocks,
    },
    mcp_delete_service::mcp_local_delete,
    mcp_graph_service::{
        mcp_local_create_graph, mcp_local_duplicate_graph, mcp_local_manage_graph,
        mcp_local_reindex_graph,
    },
    mcp_job_service::{mcp_local_get_job_result, mcp_local_get_job_status},
    mcp_search_service::{mcp_local_search_blocks, mcp_local_search_documents},
    mcp_session_service::{mcp_local_get_user_location, mcp_local_set_home_graph},
    mcp_wire_read_service::{mcp_local_get_wires, mcp_local_list_wire_predicates},
    mcp_workspace_crdt_service::{
        mcp_local_crdt_operation, mcp_local_create_document, mcp_local_create_folder,
        mcp_local_delete_document, mcp_local_flush_crdt, mcp_local_make_document_editable,
        mcp_local_move_documents, mcp_local_move_folder,
    },
    mcp_workspace_mutation_service::{
        mcp_local_create_wires, mcp_local_edit_comment, mcp_local_rename,
    },
    rdf_service::{
        mcp_local_rdf_dump, mcp_local_rdf_load, mcp_local_sparql_query, mcp_local_sparql_update,
    },
    salience_important_blocks::mcp_local_get_important_blocks,
    salience_service::{mcp_local_get_block_values, mcp_local_get_values, mcp_local_revaluate},
    source_sync::{
        mcp_authoritative_value, mcp_local_source_pull, mcp_local_source_push,
        mcp_local_source_rebuild,
    },
    time_travel_mcp::{
        mcp_local_cancel_restore_operation, mcp_local_create_restore_point,
        mcp_local_delete_restore_point, mcp_local_diff_restore_points,
        mcp_local_get_restore_operation, mcp_local_get_restore_point,
        mcp_local_list_restore_points, mcp_local_restore_to_restore_point,
    },
    wire_traversal_service::mcp_local_traverse_wires,
    workspace_projection_service::mcp_local_get_workspace,
};
use serde_json::Value;
use std::{future::Future, pin::Pin, sync::Arc};

pub(crate) struct McpCallCtx<'a> {
    pub(crate) app: AppHandle,
    pub(crate) jobs: &'a Arc<LocalJobRegistry>,
    pub(crate) credential_actor: Option<crate::document_chronicle::ChronicleActor>,
    pub(crate) token_id: Option<String>,
}

pub(crate) type McpHandlerFut<'a> = Pin<Box<dyn Future<Output = AppResult<Value>> + Send + 'a>>;
pub(crate) type McpHandler = for<'a> fn(&'a McpCallCtx<'a>, &'a Value) -> McpHandlerFut<'a>;

pub(crate) struct McpToolEntry {
    pub(crate) name: &'static str,
    pub(crate) handler: McpHandler,
}

pub(crate) fn lookup(name: &str) -> Option<&'static McpToolEntry> {
    REGISTRY.iter().find(|entry| entry.name == name)
}

// Trampolines bridge each handler's call shape onto the uniform
// `Fn(&McpCallCtx, &Value) -> AppResult<Value>` shape. Bodies stay
// untouched. Legacy `Result<Value, String>` handlers are mapped through
// `AppError::internal` here so the dispatcher boundary only needs a
// single `String::from` shim.

fn tramp_list_graphs<'a>(ctx: &'a McpCallCtx<'a>, _args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_list_graphs(app).map_err(AppError::internal) })
}

fn tramp_get_user_location<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_get_user_location(app, args).map_err(AppError::internal) })
}

fn tramp_set_home_graph<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    let token_id = ctx.token_id.clone();
    Box::pin(async move {
        mcp_local_set_home_graph(app, token_id.as_deref(), args).map_err(AppError::internal)
    })
}

fn tramp_remember<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_remember(app, args, ctx.credential_actor.clone())
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_check_document<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_check_document(app, args).map_err(AppError::internal) })
}

fn tramp_remember_batch<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_remember_batch(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_recall<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    let actor = ctx.credential_actor.clone();
    Box::pin(async move { mcp_local_recall_memories_for(app, args, actor).map_err(AppError::internal) })
}

fn tramp_diary<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    let args = args.clone();
    let actor = ctx.credential_actor.clone();
    Box::pin(async move { crate::app_runtime::async_runtime::spawn_blocking(move || mcp_local_diary(app, &args, actor))
        .await.map_err(|e| AppError::internal(e.to_string()))?.map_err(AppError::internal) })
}

fn tramp_attune<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    let actor = ctx.credential_actor.clone();
    Box::pin(async move { crate::mcp_attune::mcp_local_attune(app, args, actor).map_err(AppError::internal) })
}

fn tramp_read_diary<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    let actor = ctx.credential_actor.clone();
    Box::pin(async move { mcp_local_read_diary(app, args, actor).map_err(AppError::internal) })
}

fn tramp_emporium_read<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_emporium_read(app, args) })
}

fn tramp_sparql_query_named<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_sparql_query_named(app, args) })
}

fn tramp_emporium_write<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_emporium_write(app, args).await })
}

fn tramp_emporium_retract<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_emporium_retract(app, args).await })
}

fn tramp_archive_memories<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    let args = args.clone();
    Box::pin(async move { crate::app_runtime::async_runtime::spawn_blocking(move || mcp_local_archive_memories(app, &args))
        .await.map_err(|e|AppError::internal(e.to_string()))?.map_err(AppError::internal) })
}

fn tramp_music<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_music(app, args).map_err(AppError::internal) })
}

fn tramp_sing<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    let args = args.clone();
    Box::pin(async move { crate::app_runtime::async_runtime::spawn_blocking(move || mcp_local_sing(app, &args))
        .await.map_err(|e|AppError::internal(e.to_string()))?.map_err(AppError::internal) })
}

fn tramp_get_document_history<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_get_document_history(app, args).map_err(AppError::internal) })
}

fn tramp_read_document_at_snapshot<'a>(
    ctx: &'a McpCallCtx<'a>,
    args: &'a Value,
) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(
        async move { mcp_local_read_document_at_snapshot(app, args).map_err(AppError::internal) },
    )
}

fn tramp_create_graph<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_create_graph(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_duplicate_graph<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_duplicate_graph(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_manage_graph<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_manage_graph(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_get_job_status<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let jobs = ctx.jobs.clone();
    Box::pin(
        async move { mcp_local_get_job_status(jobs.as_ref(), args).map_err(AppError::internal) },
    )
}

fn tramp_get_job_result<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let jobs = ctx.jobs.clone();
    Box::pin(
        async move { mcp_local_get_job_result(jobs.as_ref(), args).map_err(AppError::internal) },
    )
}

fn tramp_reindex_graph<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    let jobs = ctx.jobs.clone();
    Box::pin(async move { mcp_local_reindex_graph(app, jobs, args).map_err(AppError::internal) })
}

fn tramp_list_documents<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_list_documents(app, args).map_err(AppError::internal) })
}

fn tramp_get_workspace<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_get_workspace(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_read_document<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_read_document(app, args).map_err(AppError::internal) })
}

fn tramp_write_document<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_write_document(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_read_blocks<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_read_blocks(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_document_digest<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_document_digest(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_value<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_authoritative_value(app, args).await })
}

fn tramp_get_block_values<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_get_block_values(app, args).map_err(AppError::internal) })
}

fn tramp_get_important_blocks<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_get_important_blocks(app, args).map_err(AppError::internal) })
}

fn tramp_get_block<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_get_block(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_query_blocks<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_query_blocks(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_search_documents<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    let args = args.clone();
    Box::pin(async move {
        crate::search_executor::run(move || mcp_local_search_documents(app, &args))
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_search_blocks<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    let args = args.clone();
    Box::pin(async move {
        crate::search_executor::run(move || mcp_local_search_blocks(app, &args))
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_list_wire_predicates<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_list_wire_predicates(app, args).map_err(AppError::internal) })
}

fn tramp_get_wires<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_get_wires(app, args).map_err(AppError::internal) })
}

fn tramp_create_wires<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_create_wires(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_traverse_wires<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_traverse_wires(app, args).map_err(AppError::internal) })
}

fn tramp_move_folder<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_move_folder(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_rename<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_rename(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_delete<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_delete(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_make_document_editable<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_make_document_editable(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_edit_comment<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_edit_comment(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_upload_artifact<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    let jobs = ctx.jobs.clone();
    Box::pin(async move {
        mcp_local_upload_artifact(app, jobs, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_ingest_artifact<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    let jobs = ctx.jobs.clone();
    Box::pin(async move {
        mcp_local_ingest_artifact(app, jobs, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_reference_artifact<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_reference_artifact(app, args).await })
}

fn tramp_read_artifact<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_read_artifact(app, args) })
}

fn tramp_list_artifact_kinds<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_list_artifact_kinds(app, args) })
}

fn tramp_list_artifact_revisions<'a>(
    ctx: &'a McpCallCtx<'a>,
    args: &'a Value,
) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_list_artifact_revisions(app, args) })
}

fn tramp_restore_artifact_revision<'a>(
    ctx: &'a McpCallCtx<'a>,
    args: &'a Value,
) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_restore_artifact_revision(app, args).await })
}

fn tramp_create_document<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_create_document(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_delete_document<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_delete_document(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_create_folder<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_create_folder(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_move_documents<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_move_documents(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_crdt_operation<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_crdt_operation(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_insert_blocks<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_insert_blocks(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_update_blocks<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_update_blocks(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_edit_block_text<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_edit_block_text(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_delete_blocks<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_delete_blocks(app, args)
            .await
            .map_err(AppError::internal)
    })
}

fn tramp_sparql_query<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_sparql_query(app, args).await })
}

fn tramp_sparql_update<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_sparql_update(app, args).await })
}

fn tramp_rdf_load<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_rdf_load(app, args) })
}

fn tramp_rdf_dump<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_rdf_dump(app, args) })
}

fn tramp_list_restore_points<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_list_restore_points(app, args) })
}

fn tramp_get_restore_point<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_get_restore_point(app, args) })
}

fn tramp_create_restore_point<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_create_restore_point(app, args) })
}

fn tramp_diff_restore_points<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_diff_restore_points(app, args) })
}

fn tramp_delete_restore_point<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_delete_restore_point(app, args) })
}

fn tramp_restore_to_restore_point<'a>(
    ctx: &'a McpCallCtx<'a>,
    args: &'a Value,
) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    let jobs = ctx.jobs.clone();
    Box::pin(async move { mcp_local_restore_to_restore_point(app, jobs, args) })
}

fn tramp_get_restore_operation<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let jobs = ctx.jobs.clone();
    Box::pin(async move { mcp_local_get_restore_operation(jobs.as_ref(), args) })
}

fn tramp_cancel_restore_operation<'a>(
    ctx: &'a McpCallCtx<'a>,
    args: &'a Value,
) -> McpHandlerFut<'a> {
    let jobs = ctx.jobs.clone();
    Box::pin(async move { mcp_local_cancel_restore_operation(jobs.as_ref(), args) })
}

fn tramp_source_pull<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_source_pull(app, args).await })
}

fn tramp_source_push<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_source_push(app, args).await })
}

fn tramp_source_rebuild<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_source_rebuild(app, args).await })
}

fn tramp_get_values<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_get_values(app, args).map_err(AppError::internal) })
}


fn tramp_revaluate<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move { mcp_local_revaluate(app, args).map_err(AppError::internal) })
}


fn tramp_flush_crdt<'a>(ctx: &'a McpCallCtx<'a>, args: &'a Value) -> McpHandlerFut<'a> {
    let app = ctx.app.clone();
    Box::pin(async move {
        mcp_local_flush_crdt(app, args)
            .await
            .map_err(AppError::internal)
    })
}


const REGISTRY: &[McpToolEntry] = &[
    McpToolEntry {
        name: "list_graphs",
        handler: tramp_list_graphs,
    },
    McpToolEntry {
        name: "get_user_location",
        handler: tramp_get_user_location,
    },
    McpToolEntry {
        name: "set_home_graph",
        handler: tramp_set_home_graph,
    },
    McpToolEntry {
        name: "remember",
        handler: tramp_remember,
    },
    McpToolEntry {
        name: "check_document",
        handler: tramp_check_document,
    },
    McpToolEntry {
        name: "remember_batch",
        handler: tramp_remember_batch,
    },
    McpToolEntry {
        name: "recall",
        handler: tramp_recall,
    },
    McpToolEntry {
        name: "diary",
        handler: tramp_diary,
    },
    McpToolEntry {
        name: "read_diary",
        handler: tramp_read_diary,
    },
    McpToolEntry {
        name: "attune",
        handler: tramp_attune,
    },
    McpToolEntry {
        name: "archive_memories",
        handler: tramp_archive_memories,
    },
    McpToolEntry {
        name: "emporium_read",
        handler: tramp_emporium_read,
    },
    McpToolEntry {
        name: "emporium_write",
        handler: tramp_emporium_write,
    },
    McpToolEntry {
        name: "emporium_retract",
        handler: tramp_emporium_retract,
    },
    McpToolEntry {
        name: "music",
        handler: tramp_music,
    },
    McpToolEntry {
        name: "sing",
        handler: tramp_sing,
    },
    McpToolEntry {
        name: "get_document_history",
        handler: tramp_get_document_history,
    },
    McpToolEntry {
        name: "read_document_at_snapshot",
        handler: tramp_read_document_at_snapshot,
    },
    McpToolEntry {
        name: "create_graph",
        handler: tramp_create_graph,
    },
    McpToolEntry {
        name: "duplicate_graph",
        handler: tramp_duplicate_graph,
    },
    McpToolEntry {
        name: "manage_graph",
        handler: tramp_manage_graph,
    },
    McpToolEntry {
        name: "get_job_status",
        handler: tramp_get_job_status,
    },
    McpToolEntry {
        name: "get_job_result",
        handler: tramp_get_job_result,
    },
    McpToolEntry {
        name: "reindex_graph",
        handler: tramp_reindex_graph,
    },
    McpToolEntry {
        name: "list_documents",
        handler: tramp_list_documents,
    },
    McpToolEntry {
        name: "get_workspace",
        handler: tramp_get_workspace,
    },
    McpToolEntry {
        name: "read_document",
        handler: tramp_read_document,
    },
    McpToolEntry {
        name: "write_document",
        handler: tramp_write_document,
    },
    McpToolEntry {
        name: "read_blocks",
        handler: tramp_read_blocks,
    },
    McpToolEntry {
        name: "document_digest",
        handler: tramp_document_digest,
    },
    McpToolEntry {
        name: "value",
        handler: tramp_value,
    },
    McpToolEntry {
        name: "get_block_values",
        handler: tramp_get_block_values,
    },
    McpToolEntry {
        name: "get_values",
        handler: tramp_get_values,
    },
    McpToolEntry {
        name: "revaluate",
        handler: tramp_revaluate,
    },
    McpToolEntry {
        name: "get_important_blocks",
        handler: tramp_get_important_blocks,
    },
    McpToolEntry {
        name: "get_block",
        handler: tramp_get_block,
    },
    McpToolEntry {
        name: "query_blocks",
        handler: tramp_query_blocks,
    },
    McpToolEntry {
        name: "search_documents",
        handler: tramp_search_documents,
    },
    McpToolEntry {
        name: "search_blocks",
        handler: tramp_search_blocks,
    },
    McpToolEntry {
        name: "list_wire_predicates",
        handler: tramp_list_wire_predicates,
    },
    McpToolEntry {
        name: "get_wires",
        handler: tramp_get_wires,
    },
    McpToolEntry {
        name: "create_wires",
        handler: tramp_create_wires,
    },
    McpToolEntry {
        name: "traverse_wires",
        handler: tramp_traverse_wires,
    },
    McpToolEntry {
        name: "move_folder",
        handler: tramp_move_folder,
    },
    McpToolEntry {
        name: "rename",
        handler: tramp_rename,
    },
    McpToolEntry {
        name: "delete",
        handler: tramp_delete,
    },
    McpToolEntry {
        name: "make_document_editable",
        handler: tramp_make_document_editable,
    },
    McpToolEntry {
        name: "edit_comment",
        handler: tramp_edit_comment,
    },
    McpToolEntry {
        name: "upload_artifact",
        handler: tramp_upload_artifact,
    },
    McpToolEntry {
        name: "reference_artifact",
        handler: tramp_reference_artifact,
    },
    McpToolEntry {
        name: "ingest_artifact",
        handler: tramp_ingest_artifact,
    },
    McpToolEntry {
        name: "read_artifact",
        handler: tramp_read_artifact,
    },
    McpToolEntry {
        name: "list_artifact_kinds",
        handler: tramp_list_artifact_kinds,
    },
    McpToolEntry {
        name: "list_artifact_revisions",
        handler: tramp_list_artifact_revisions,
    },
    McpToolEntry {
        name: "restore_artifact_revision",
        handler: tramp_restore_artifact_revision,
    },
    McpToolEntry {
        name: "create_document",
        handler: tramp_create_document,
    },
    McpToolEntry {
        name: "delete_document",
        handler: tramp_delete_document,
    },
    McpToolEntry {
        name: "create_folder",
        handler: tramp_create_folder,
    },
    McpToolEntry {
        name: "move_documents",
        handler: tramp_move_documents,
    },
    McpToolEntry {
        name: "flush_crdt",
        handler: tramp_flush_crdt,
    },
    McpToolEntry {
        name: "crdt_operation",
        handler: tramp_crdt_operation,
    },
    McpToolEntry {
        name: "insert_blocks",
        handler: tramp_insert_blocks,
    },
    McpToolEntry {
        name: "update_blocks",
        handler: tramp_update_blocks,
    },
    McpToolEntry {
        name: "edit_block_text",
        handler: tramp_edit_block_text,
    },
    McpToolEntry {
        name: "delete_blocks",
        handler: tramp_delete_blocks,
    },
    McpToolEntry {
        name: "sparql_query",
        handler: tramp_sparql_query,
    },
    McpToolEntry {
        name: "sparql_query_named",
        handler: tramp_sparql_query_named,
    },
    McpToolEntry {
        name: "sparql_update",
        handler: tramp_sparql_update,
    },
    McpToolEntry {
        name: "rdf_load",
        handler: tramp_rdf_load,
    },
    McpToolEntry {
        name: "rdf_dump",
        handler: tramp_rdf_dump,
    },
    McpToolEntry {
        name: "list_restore_points",
        handler: tramp_list_restore_points,
    },
    McpToolEntry {
        name: "get_restore_point",
        handler: tramp_get_restore_point,
    },
    McpToolEntry {
        name: "create_restore_point",
        handler: tramp_create_restore_point,
    },
    McpToolEntry {
        name: "diff_restore_points",
        handler: tramp_diff_restore_points,
    },
    McpToolEntry {
        name: "delete_restore_point",
        handler: tramp_delete_restore_point,
    },
    McpToolEntry {
        name: "restore_to_restore_point",
        handler: tramp_restore_to_restore_point,
    },
    McpToolEntry {
        name: "get_restore_operation",
        handler: tramp_get_restore_operation,
    },
    McpToolEntry {
        name: "cancel_restore_operation",
        handler: tramp_cancel_restore_operation,
    },
    McpToolEntry {
        name: "source_pull",
        handler: tramp_source_pull,
    },
    McpToolEntry {
        name: "source_push",
        handler: tramp_source_push,
    },
    McpToolEntry {
        name: "source_rebuild",
        handler: tramp_source_rebuild,
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    // Snapshot guard: every catalog tool name must have a dispatch entry,
    // and the registry must not carry tools that are not in the catalog.
    // This is the structural insurance against drift between
    // `mcp_tool_catalog.json` and the runtime dispatch table.
    #[test]
    fn registry_covers_every_catalog_tool_exactly() {
        #[derive(serde::Deserialize)]
        struct Catalog {
            tools: Vec<CatalogTool>,
        }
        #[derive(serde::Deserialize)]
        struct CatalogTool {
            name: String,
        }
        let catalog: Catalog = serde_json::from_str(include_str!("mcp_tool_catalog.json"))
            .expect("embedded MCP tool catalog must be valid JSON");

        let registry_names: BTreeSet<&'static str> =
            REGISTRY.iter().map(|entry| entry.name).collect();
        let catalog_names: BTreeSet<String> =
            catalog.tools.iter().map(|tool| tool.name.clone()).collect();

        assert_eq!(
            REGISTRY.len(),
            registry_names.len(),
            "registry contains duplicate tool names"
        );
        assert_eq!(
            REGISTRY.len(),
            catalog.tools.len(),
            "registry size ({}) does not match catalog size ({})",
            REGISTRY.len(),
            catalog.tools.len()
        );
        for name in &catalog_names {
            assert!(
                registry_names.contains(name.as_str()),
                "catalog tool {name} has no dispatch entry"
            );
        }
        for name in &registry_names {
            assert!(
                catalog_names.contains(*name),
                "dispatch entry {name} is not in the catalog"
            );
        }
    }

    #[test]
    fn registry_lookup_returns_known_entries() {
        assert!(lookup("list_graphs").is_some());
        assert!(lookup("write_document").is_some());
        assert!(lookup("rdf_load").is_some());
        assert!(lookup("restore_to_restore_point").is_some());
        assert!(lookup("not_a_real_tool").is_none());
    }

    // The emporium tools that ship: read, write, retract and the named query.
    // The list/heads/sweep/query/vocab/violations faces were cut 2026-10-27.
    #[test]
    fn emporium_mcp_family_is_reachable_through_dispatch() {
        assert!(lookup("emporium_read").is_some());
        assert!(lookup("sparql_query_named").is_some());
        assert!(lookup("emporium_write").is_some());
        assert!(lookup("emporium_retract").is_some());
        for cut in ["emporium_vocab", "emporium_list", "emporium_heads", "emporium_sweep", "emporium_query", "emporium_violations"] {
            assert!(lookup(cut).is_none(), "{cut} was cut");
        }
    }
}

// The emporium MCP family needs a REAL per-graph store (`emporium_read` and
// `sparql_query_named` both resolve one), so the "it actually SERVES, not just
// resolves" proof runs under the headless harness. NO MOCKS: real cell, real store, real spine —
// dispatched through the EXACT `McpCallCtx` + `(entry.handler)(...)` path
// `mcp_tool_dispatch.rs` uses for a live `tools/call`.
#[cfg(all(test, feature = "headless"))]
mod headless_dispatch_tests {
    use super::*;
    use crate::emporium::planner::memory_record_subject;
    use crate::emporium::schemas::{MemoryRecordIn, SourceRefIn};
    use crate::graph_service::{create_graph_service, CreateGraphInput};
    use crate::local_jobs::LocalJobRegistry;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn env_serial() -> &'static Mutex<()> {
        crate::tauri_runtime::profile_env_serial()
    }

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("garden-emporium-mcp-dispatch-{name}-{nanos}"))
    }

    fn memory_record(
        client_ref: &str,
        content: &str,
        supersedes: Option<String>,
    ) -> MemoryRecordIn {
        MemoryRecordIn {
            client_ref: Some(client_ref.to_string()),
            scope: "agent".to_string(),
            kind: "ClaimMemory".to_string(),
            content_orientation: "knowledge".to_string(),
            visibility: "private".to_string(),
            status: "active".to_string(),
            content: content.to_string(),
            source_refs: vec![SourceRefIn {
                source_kind: "DocumentBlock".to_string(),
                source_label: None,
                block_id: Some("abc".to_string()),
                document_id: Some("doc-shell".to_string()),
                external_id: None,
                external_uri: None,
                observed_at: None,
                trust_tier: None,
            }],
            evidence: vec![],
            observed_at: Some(1_718_700_000_000),
            valid_from: Some(1_718_700_000_000),
            is_current: Some(true),
            confidence: None,
            valence: None,
            agent_id: Some("gamma".to_string()),
            observer_agent_id: None,
            tags: vec![],
            supersedes_ref: supersedes,
            contradicts_ref: None,
        }
    }

    fn call(ctx: &McpCallCtx<'_>, name: &str, args: Value) -> AppResult<Value> {
        let entry = lookup(name).unwrap_or_else(|| panic!("dispatch entry for {name}"));
        crate::app_runtime::async_runtime::block_on((entry.handler)(ctx, &args))
    }

    #[test]
    fn emporium_mcp_family_is_dispatched_and_serves_real_reads() {
        let _serial = env_serial().lock().unwrap_or_else(|p| p.into_inner());
        let profile = temp_dir("profile");
        let jobs_dir = temp_dir("jobs");
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);
        let result = std::panic::catch_unwind(|| run_emporium_family_serve_trace(&jobs_dir));
        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        let _ = std::fs::remove_dir_all(&jobs_dir);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    fn run_emporium_family_serve_trace(jobs_dir: &Path) {
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);
        let graph_id = "emporium-mcp-serve-lab";
        create_graph_service(
            &app,
            CreateGraphInput {
                graph_id: Some(graph_id.to_string()),
                title: "Emporium MCP Serve Lab".to_string(),
                description: None,
                operation_id: None,
            },
        )
        .expect("create graph");

        let jobs =
            Arc::new(LocalJobRegistry::new(jobs_dir.to_path_buf()).expect("create job registry"));
        let ctx = McpCallCtx {
            app: app.clone(),
            jobs: &jobs,
            credential_actor: None,
            token_id: None,
        };

        // Seed one object through the REAL object surface directly; only the
        // dispatch of the read is under test here.
        crate::app_runtime::async_runtime::block_on(crate::emporium::objects::create_objects(
            &app,
            graph_id,
            "emporium-bookmark",
            serde_json::json!([{
                "kind": "Bookmark",
                "localId": "dispatch-book",
                "url": "https://example.test/book",
                "title": "Dispatch Book",
            }]),
        ))
        .expect("seed bookmark");

        // emporium_read: dispatched, round-trips the seeded object.
        let read = call(
            &ctx,
            "emporium_read",
            serde_json::json!({
                "graph_id": graph_id, "vocab": "emporium-bookmark", "class": "Bookmark",
                "address": "dispatch-book",
            }),
        )
        .expect("emporium_read serves");
        assert!(
            read["subject"]
                .as_str()
                .is_some_and(|s| s.ends_with("dispatch-book")),
            "{read}"
        );

        // File H, then race two supersessions of H — a genuine contested
        // lineage (both isCurrent=true) for the named query to surface.
        let h = memory_record("r-h", "nora prefers fish CLI", None);
        let h_subject = memory_record_subject(graph_id, &h);
        crate::app_runtime::async_runtime::block_on(
            crate::geist_memory_service::ingest_memory_record(&app, graph_id, h),
        )
        .expect("file H");
        let a = memory_record("r-a", "nora prefers zsh", Some(h_subject.clone()));
        let b = memory_record("r-b", "nora prefers nushell", Some(h_subject));
        let (ra, rb) = crate::app_runtime::async_runtime::block_on(async {
            tokio::join!(
                crate::geist_memory_service::ingest_memory_record(&app, graph_id, a),
                crate::geist_memory_service::ingest_memory_record(&app, graph_id, b),
            )
        });
        assert_eq!(ra.expect("A ingest ran")["ok"], serde_json::json!(true));
        assert_eq!(rb.expect("B ingest ran")["ok"], serde_json::json!(true));

        // sparql_query_named: dispatched, currentHeads surfaces the contested
        // lineage.
        let named_heads = call(
            &ctx,
            "sparql_query_named",
            serde_json::json!({
                "graph_id": graph_id, "vocab": "sophia-memory-core", "class": "MemoryRecord",
                "query_name": "currentHeads",
            }),
        )
        .expect("sparql_query_named currentHeads serves");
        let named_rows = named_heads["rows"].as_array().expect("rows array");
        assert!(
            named_rows
                .iter()
                .any(|r| r["contested"] == serde_json::json!(true)),
            "{named_heads}"
        );

        // sparql_query_named byId: dispatched, reads the seeded bookmark's span
        // (a non-membrane-ed, SimpleProjection-routed class).
        let named_by_id = call(
            &ctx,
            "sparql_query_named",
            serde_json::json!({
                "graph_id": graph_id, "vocab": "emporium-bookmark", "class": "Bookmark",
                "query_name": "byId", "params": {"subject": read["subject"]},
            }),
        )
        .expect("sparql_query_named byId serves");
        assert!(
            !named_by_id["rows"]
                .as_array()
                .expect("rows array")
                .is_empty(),
            "{named_by_id}"
        );
    }
}
