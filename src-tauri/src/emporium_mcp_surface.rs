//! Thin MCP handlers over Emporium's engine surface: `emporium_read` wraps
//! the generic object read ([`crate::emporium::objects::read_object`]),
//! `sparql_query_named` wraps the query-face engine
//! ([`crate::emporium::query_engine::run_named_query`]), and `emporium_write`
//! / `emporium_retract` wrap the memory write path ([`crate::emporium::write`]).
//! The list, heads, sweep and object-query faces were cut on 2026-10-27
//! (backend pruning for the Mycl release); the three shipping callers use
//! read, write and retract.
//!
//! Lives at the crate root (not inside `emporium/`) because every function it
//! calls is already `pub(crate)`.

use serde_json::Value;

use crate::app_error::{AppError, AppResult};
use crate::app_error_codes;
use crate::app_runtime::AppHandle;
use crate::emporium::objects::{read_object, ObjectError};
use crate::emporium::query_emit::QueryEmitError;
use crate::emporium::query_engine::run_named_query;
use crate::emporium::write::{emporium_retract_with_identity, emporium_write};
use crate::mcp_arg_utils::{mcp_arg_bool, mcp_arg_string, mcp_required_graph_id};

/// Map the object surface's HTTP-shaped [`ObjectError`] onto [`AppError`] —
/// the same status-family mapping `emporium/vocab_routes.rs`'s
/// `object_error_response` uses for the HTTP face, so the MCP face agrees.
fn object_error_to_app_error(error: ObjectError) -> AppError {
    let message = error.message().to_string();
    match error {
        ObjectError::BadRequest(_) => AppError::validation(message),
        // Vocab/class misconfiguration — NEVER coded, so a client cannot
        // collapse a setup error into a quiet empty card.
        ObjectError::NotFound(_) => AppError::not_found(message),
        // Genuine absence of an object in a well-formed class — the ONLY
        // NotFound-family variant that carries `object_not_found`.
        ObjectError::Absent(_) => {
            AppError::not_found(message).with_code(app_error_codes::OBJECT_NOT_FOUND)
        }
        ObjectError::Conflict(_) => AppError::conflict(message),
        ObjectError::Internal(_) => AppError::internal(message),
    }
}

fn required_field(arguments: &Value, key: &str) -> AppResult<String> {
    mcp_arg_string(arguments, &[key])
        .ok_or_else(|| AppError::validation(format!("{key} is required")))
}

/// `emporium_read` — READ one object's full predicate span. Required
/// `graph_id`/`graphId`, `vocab`, `class`, `address` (a full subject IRI, or a
/// bare `localId` minted through the class's `subject_rule`).
pub(crate) fn mcp_local_emporium_read(app: AppHandle, arguments: &Value) -> AppResult<Value> {
    let graph_id = mcp_required_graph_id(arguments).map_err(AppError::validation)?;
    let vocab = required_field(arguments, "vocab")?;
    let class = required_field(arguments, "class")?;
    let address = required_field(arguments, "address")?;
    read_object(&app, &graph_id, &vocab, &class, &address).map_err(object_error_to_app_error)
}

/// Map the query-face engine's [`QueryEmitError`] onto [`AppError`] — the same
/// status-family shape as [`object_error_to_app_error`] above (this MCP tool
/// sits directly beside the HTTP query-catalog face, which maps the same
/// error type via `loopback_error`).
fn query_emit_error_to_app_error(error: QueryEmitError) -> AppError {
    let message = error.message().to_string();
    match error {
        QueryEmitError::BadRequest(_) => AppError::validation(message),
        QueryEmitError::NotFound(_) => AppError::not_found(message),
        QueryEmitError::Internal(_) => AppError::internal(message),
    }
}

/// `sparql_query_named` — T2 item 11's query-face substrate reachable from the
/// agent's hands: run one of a class's canonical named queries (`byId`,
/// `currentHeads`, `lineageOf`, `countBy`), DERIVED from the class's declared
/// shape + [`crate::emporium::contract::MaterializationSignature`] (never a
/// hand-maintained per-class query). Required `graph_id`/`graphId`, `vocab`,
/// `class`, `query_name`/`queryName`; `params` carries the query-specific
/// arguments (`subject` for byId/lineageOf, `asOf` for currentHeads, `attr`
/// for countBy); optional `observer` widens a membrane-ed class's scope to
/// commons ∪ that observer's membrane (RATIFIED: no observer ⇒ commons only).
/// Returns `{graphId, vocab, class, queryName, variables, rows, warnings?}` —
/// the `rdf_query_service` rows+warnings shape, generalized.
pub(crate) fn mcp_local_sparql_query_named(app: AppHandle, arguments: &Value) -> AppResult<Value> {
    let graph_id = mcp_required_graph_id(arguments).map_err(AppError::validation)?;
    let vocab = required_field(arguments, "vocab")?;
    let class = required_field(arguments, "class")?;
    let query_name = mcp_arg_string(arguments, &["query_name", "queryName"]).ok_or_else(|| {
        AppError::validation(
            "query_name is required (one of: byId, currentHeads, lineageOf, countBy)",
        )
    })?;
    let observer = mcp_arg_string(arguments, &["observer"]);
    let params = arguments
        .get("params")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let outcome = run_named_query(
        &app,
        &graph_id,
        &vocab,
        &class,
        &query_name,
        observer.as_deref(),
        &params,
    )
    .map_err(query_emit_error_to_app_error)?;
    Ok(serde_json::json!(outcome))
}

/// `emporium_write` — T-W item 1, the MCP write surface over the existing
/// spine (the five laws — see [`crate::emporium::write`]'s module doc for the
/// mechanical detail). Required `graph_id`/`graphId`, `vocab`, `records`
/// (non-empty array); optional `dry_run`/`dryRun` (validation preview, writes
/// nothing), `publish` (explicit commons for a membrane-ed vocab), `observer`
/// (the writer's own membrane; per-record `observerAgentId` overrides it).
/// Returns `{graphId, vocab, dryRun, ok, results:[{subject, outcome:
/// applied|converged|halted, flags?}], journalRef?, warnings, violations?}`.
pub(crate) async fn mcp_local_emporium_write(
    app: AppHandle,
    arguments: &Value,
) -> AppResult<Value> {
    let graph_id = mcp_required_graph_id(arguments).map_err(AppError::validation)?;
    let vocab = required_field(arguments, "vocab")?;
    let records: Vec<Value> = arguments
        .get("records")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| {
            AppError::validation("records is required (a non-empty array)".to_string())
        })?;
    let dry_run = mcp_arg_bool(arguments, &["dry_run", "dryRun"], false);
    let publish = mcp_arg_bool(arguments, &["publish"], false);
    let observer = mcp_arg_string(arguments, &["observer"]);
    let source_gate = crate::source_sync::acquire_source_gate(&graph_id).await;
    if !dry_run && crate::source_sync::source_authority_active(&app, &graph_id)? {
        drop(source_gate);
        return crate::source_sync::mcp_source_emporium_write(app, arguments).await;
    }
    let result = emporium_write(
        &app,
        &graph_id,
        &vocab,
        &records,
        dry_run,
        publish,
        observer.as_deref(),
    )
    .await
    .map_err(object_error_to_app_error);
    drop(source_gate);
    result
}

/// `emporium_retract` — T-W item 2, Law 4: mints a retraction EVENT on
/// `subject` (who/when/rationale) — heads exclude it, faces hide it, the log
/// keeps it. NO hard-delete (that stays a separate, deferred admin-tier act).
/// Required `graph_id`/`graphId`, `subject` (a full IRI), `rationale`;
/// optional `kind` (`"retract"` default, or `"archive"`), `observer` (the
/// witness recording the retraction — NOT used to locate the subject, which
/// is found by a store-wide scan since this tool takes no vocab/class).
pub(crate) async fn mcp_local_emporium_retract(
    app: AppHandle,
    arguments: &Value,
) -> AppResult<Value> {
    let graph_id = mcp_required_graph_id(arguments).map_err(AppError::validation)?;
    let subject = required_field(arguments, "subject")?;
    let rationale = required_field(arguments, "rationale")?;
    let kind = mcp_arg_string(arguments, &["kind"]).unwrap_or_else(|| "retract".to_string());
    let observer = mcp_arg_string(arguments, &["observer"]);
    let event_id = mcp_arg_string(
        arguments,
        &[
            "retraction_event_id",
            "retractionEventId",
            "operation_id",
            "operationId",
        ],
    );
    let at_ms = arguments
        .get("at_ms")
        .or_else(|| arguments.get("atMs"))
        .and_then(Value::as_i64);
    let source_gate = crate::source_sync::acquire_source_gate(&graph_id).await;
    if crate::source_sync::source_authority_active(&app, &graph_id)? {
        drop(source_gate);
        return crate::source_sync::mcp_source_emporium_retract(app, arguments).await;
    }
    let result = emporium_retract_with_identity(
        &app,
        &graph_id,
        &subject,
        &rationale,
        &kind,
        observer.as_deref(),
        event_id.as_deref(),
        at_ms,
    )
    .await
    .map_err(object_error_to_app_error);
    drop(source_gate);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D-C23/E24's regression guard, R18 (master §8.3): a single `.with_code()`
    /// must not code the misconfiguration and genuine-absence arms alike.
    /// Master §4.2.1 names this `object_not_found_is_coded_only_for_absence`.
    /// Proved directly against the mapping function — it is pure, and the
    /// claim under test is about ITS logic, not the surrounding MCP dispatch.
    #[test]
    fn object_not_found_is_coded_only_for_absence() {
        let misconfigured = object_error_to_app_error(ObjectError::NotFound(
            "class 'Nope' is not declared by vocab 'v'".to_string(),
        ));
        assert_eq!(
            misconfigured.code(),
            None,
            "a vocab/class misconfiguration must never be coded — a client must not \
             collapse a real setup error into a quiet empty card"
        );

        let absent = object_error_to_app_error(ObjectError::Absent(
            "no object <urn:x> in <urn:sink>".to_string(),
        ));
        assert_eq!(absent.code(), Some(app_error_codes::OBJECT_NOT_FOUND));

        // Both arms share the same 404 status — the split is about
        // machine-readability, not about what the wire says happened.
        assert_eq!(
            misconfigured.kind(),
            crate::app_error::AppErrorKind::NotFound
        );
        assert_eq!(absent.kind(), crate::app_error::AppErrorKind::NotFound);
    }
}
