//! Chronicle principal middleware: resolves WHO is making a loopback request
//! from the credential it presents, and scopes that actor over the handler
//! as a task-local (`document_chronicle::with_actor`). Every CRDT operation
//! the handler enqueues is then stamped at the enqueue choke point.
//!
//! The mapping is deliberately small:
//! - the cell's own per-run master token → the local human owner
//!   (`human:owner`, via `master-token`);
//! - a named client token → an agent named by the token's label
//!   (`agent:<label>`, via `client-token:<id>`); the MCP dispatcher may refine
//!   it further from an explicit `agent` argument;
//! - anything else → the human owner. Unauthenticated requests never reach a
//!   write, so this default only names the principal of a request that is
//!   about to be rejected anyway.

use crate::{
    document_chronicle::{with_actor, ChronicleActor},
    loopback_client_token_service::resolve_loopback_client_token_authorization,
    loopback_http::{bearer_token, loopback_master_token_presented},
    loopback_state::LoopbackState,
};
use axum::{
    body::Body,
    extract::{Request, State},
    http::HeaderMap,
    middleware::Next,
    response::Response,
};
use std::sync::Arc;

pub(crate) async fn loopback_actor_middleware(
    State(state): State<Arc<LoopbackState>>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let actor = resolve_request_actor(request.headers(), &state);
    with_actor(actor, next.run(request)).await
}

/// The trust decision, factored out so it is testable without axum's `Next`.
pub(crate) fn resolve_request_actor(headers: &HeaderMap, state: &LoopbackState) -> ChronicleActor {
    if loopback_master_token_presented(headers, state) {
        return ChronicleActor::human_owner_via("master-token");
    }
    if let Some(token) = bearer_token(headers) {
        match resolve_loopback_client_token_authorization(&state.app, token) {
            Ok(Some(principal)) => {
                let label = principal.label.trim();
                let name = if label.is_empty() {
                    principal.token_id.as_str()
                } else {
                    label
                };
                return ChronicleActor::agent(
                    name,
                    Some(format!("client-token:{}", principal.token_id)),
                );
            }
            Ok(None) => {}
            Err(error) => {
                log::debug!("chronicle principal: client token lookup failed: {error}");
            }
        }
    }
    ChronicleActor::human_owner()
}
