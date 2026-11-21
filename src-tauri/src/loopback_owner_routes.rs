//! Owner-only loopback routes for issuing connector credentials.
//!
//! Gardend owns the profile and its master session credential. Harnesses only
//! receive scoped connector tokens from this surface; they cannot bootstrap a
//! second owner or mint wider credentials for themselves.

use crate::{
    loopback_client_token_service::{
        create_client_token_as, list_client_tokens, reissue_client_token_as, revoke_client_token_as,
    },
    loopback_client_token_types::{
        CreateLoopbackClientTokenInput, LoopbackClientTokenSummary, ReissueLoopbackClientTokenInput,
        RevokeLoopbackClientTokenInput,
    },
    loopback_http::{loopback_result, require_loopback_owner},
    loopback_state::LoopbackState,
};
use axum::{
    extract::{Path, State},
    http::HeaderMap,
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use serde::Serialize;
use std::sync::Arc;

const OWNER_ROUTE_ACTOR: &str = "loopback-owner";

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CreateConnectorResponse {
    mcp_url: String,
    token: String,
    connector: LoopbackClientTokenSummary,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReissueConnectorResponse {
    mcp_url: String,
    token: String,
    connector: LoopbackClientTokenSummary,
    previous_token_id: String,
}

pub(super) fn loopback_owner_router() -> Router<Arc<LoopbackState>> {
    Router::new()
        .route(
            "/owner/connectors",
            get(loopback_list_connectors).post(loopback_create_connector),
        )
        .route(
            "/owner/connectors/{token_id}",
            delete(loopback_revoke_connector),
        )
        .route(
            "/owner/connectors/{token_id}/reissue",
            post(loopback_reissue_connector),
        )
        .route(
            "/owner/graphs/{graph_id}/artifact-roots",
            get(loopback_get_artifact_roots).put(loopback_put_artifact_roots),
        )
}

/// The graph's allowed roots for artifacts (design seed 2026-10-23, step
/// 2): owner control plane only, never behind `artifacts.write`.
async fn loopback_get_artifact_roots(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    Path(graph_id): Path<String>,
) -> Response {
    if let Err(response) = require_loopback_owner(&headers, &state) {
        return response;
    }
    loopback_result(crate::artifact_roots::roots_report(&state.app, &graph_id))
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PutArtifactRootsInput {
    #[serde(default)]
    roots: Vec<crate::artifact_roots::ArtifactRootInput>,
}

async fn loopback_put_artifact_roots(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    Path(graph_id): Path<String>,
    Json(input): Json<PutArtifactRootsInput>,
) -> Response {
    if let Err(response) = require_loopback_owner(&headers, &state) {
        return response;
    }
    loopback_result(
        crate::artifact_roots::replace_roots(&state.app, &graph_id, input.roots)
            .and_then(|_| crate::artifact_roots::roots_report(&state.app, &graph_id)),
    )
}

async fn loopback_list_connectors(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_loopback_owner(&headers, &state) {
        return response;
    }
    loopback_result(list_client_tokens(&state.app))
}

async fn loopback_create_connector(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    Json(input): Json<CreateLoopbackClientTokenInput>,
) -> Response {
    if let Err(response) = require_loopback_owner(&headers, &state) {
        return response;
    }
    match create_client_token_as(&state.app, input, OWNER_ROUTE_ACTOR) {
        Ok(created) => Json(CreateConnectorResponse {
            mcp_url: state.manifest.mcp_url.clone(),
            token: created.token,
            connector: created.record,
        })
        .into_response(),
        Err(error) => loopback_result::<serde_json::Value>(Err(error)),
    }
}

/// Re-issue a connector in one call (Eschaton, 2026-11-02: the Connected
/// Agents page gets a re-issue button): same label and grant, a new secret
/// shown once, the old record revoked in the same step. The body is
/// optional: `{ expiresInDays }` overrides the original's lifetime.
async fn loopback_reissue_connector(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    Path(token_id): Path<String>,
    body: Option<Json<ReissueLoopbackClientTokenInput>>,
) -> Response {
    if let Err(response) = require_loopback_owner(&headers, &state) {
        return response;
    }
    let input = body.map(|Json(input)| input).unwrap_or_default();
    match reissue_client_token_as(&state.app, &token_id, input.expires_in_days, OWNER_ROUTE_ACTOR) {
        Ok(reissued) => Json(ReissueConnectorResponse {
            mcp_url: state.manifest.mcp_url.clone(),
            token: reissued.token,
            connector: reissued.record,
            previous_token_id: reissued.previous_token_id,
        })
        .into_response(),
        Err(error) => loopback_result::<serde_json::Value>(Err(error)),
    }
}

async fn loopback_revoke_connector(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    Path(token_id): Path<String>,
) -> Response {
    if let Err(response) = require_loopback_owner(&headers, &state) {
        return response;
    }
    loopback_result(revoke_client_token_as(
        &state.app,
        RevokeLoopbackClientTokenInput { token_id },
        OWNER_ROUTE_ACTOR,
    ))
}
