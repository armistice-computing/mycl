use crate::{
    loopback_http::{authorized_loopback_token, loopback_error, require_loopback_scope},
    loopback_state::LoopbackState,
    mcp_rpc_protocol::{mcp_error, mcp_scope_error, mcp_success, McpRpcRequest, McpRpcResponse},
    mcp_tool_dispatch::handle_mcp_tool_call,
    mcp_tool_registry::mcp_tools_list_result_for_boundary,
};
use axum::{
    extract::{Extension, State},
    http::{HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use std::sync::Arc;

pub(super) fn loopback_mcp_router() -> Router<Arc<LoopbackState>> {
    Router::new().route("/mcp", get(loopback_mcp_info).post(loopback_mcp))
}

pub(super) async fn loopback_mcp_info(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = require_loopback_scope(&headers, &state, "mcp.tools.read") {
        return response;
    }
    Json(serde_json::json!({
        "protocol": "mcp",
        "transport": "streamable-http-json",
        "endpoint": state.manifest.mcp_url,
        "methods": ["initialize", "tools/list", "tools/call"],
    }))
    .into_response()
}

pub(super) async fn loopback_mcp(
    State(state): State<Arc<LoopbackState>>,
    headers: HeaderMap,
    method: Method,
    lease: Option<Extension<crate::cell_graph_boundary::VerifiedCellLease>>,
    Json(request): Json<McpRpcRequest>,
) -> Response {
    if method != Method::POST {
        return loopback_error(StatusCode::METHOD_NOT_ALLOWED, "MCP endpoint accepts POST");
    }
    let entry_scope = match request.method.as_str() {
        "tools/call" => "mcp.tools.call",
        _ => "mcp.tools.read",
    };
    let authorization = match authorized_loopback_token(&headers, &state, &[entry_scope]) {
        Ok(authorization) => authorization,
        Err(response) => return response,
    };
    let role = lease
        .map(|Extension(lease)| lease.role)
        .unwrap_or(crate::cell_graph_boundary::CellRole::Owner);
    Json(
        handle_mcp_request(
            &state,
            request,
            &authorization.scopes,
            authorization.grant_profile_id.as_deref(),
            authorization.token_id.as_deref(),
            authorization.home_graph_id.as_deref(),
            role,
        )
        .await,
    )
    .into_response()
}

async fn handle_mcp_request(
    state: &LoopbackState,
    request: McpRpcRequest,
    token_scopes: &[String],
    grant_profile_id: Option<&str>,
    token_id: Option<&str>,
    home_graph_id: Option<&str>,
    role: crate::cell_graph_boundary::CellRole,
) -> McpRpcResponse {
    if request.jsonrpc.as_deref() != Some("2.0") {
        return mcp_error(request.id, -32600, "expected JSON-RPC 2.0 request");
    }

    match request.method.as_str() {
        "initialize" => {
            if let Some(error) = mcp_scope_error(token_scopes, request.id.clone(), "mcp.tools.read")
            {
                return error;
            }
            // The welcome names only tools this key can call.
            let callable = crate::mcp_tool_registry::mcp_tool_names_for_boundary(
                &state.cell_graph,
                role,
                token_scopes,
                grant_profile_id,
            );
            mcp_success(
                request.id,
                serde_json::json!({
                    "protocolVersion": "2025-03-26",
                    "serverInfo": {
                        "name": crate::mcp_welcome::MCP_SERVER_NAME,
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                    "capabilities": {
                        "tools": {},
                    },
                    "instructions": crate::mcp_welcome::mcp_instructions(
                        |name| callable.contains(name),
                    ),
                }),
            )
        }
        "tools/list" => {
            if let Some(error) = mcp_scope_error(token_scopes, request.id.clone(), "mcp.tools.read")
            {
                return error;
            }
            mcp_success(
                request.id,
                serde_json::to_value(mcp_tools_list_result_for_boundary(
                    &state.cell_graph,
                    role,
                    token_scopes,
                    grant_profile_id,
                ))
                .expect("McpToolsListResult always serializes"),
            )
        }
        "tools/call" => {
            handle_mcp_tool_call(
                state,
                request.id,
                request.params,
                token_scopes,
                grant_profile_id,
                token_id,
                home_graph_id,
                role,
            )
            .await
        }
        _ => mcp_error(
            request.id,
            -32601,
            format!("unknown MCP method {}", request.method),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        cell_graph_boundary::CellGraphBoundary, cell_lifecycle::CellLifecycle,
        local_jobs::LocalJobRegistry,
        loopback_client_token_service::create_client_token,
        loopback_client_token_types::CreateLoopbackClientTokenInput,
        loopback_http::authorized_loopback_token, loopback_state::LoopbackManifest,
        loopback_token_grants::session_all_token_grant,
    };
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;
    use uuid::Uuid;

    const OWNER_TOKEN: &str = "test-owner-token";

    fn env_serial() -> &'static std::sync::Mutex<()> {
        crate::tauri_runtime::profile_env_serial()
    }

    fn test_state() -> Arc<LoopbackState> {
        let app = crate::tauri_runtime::build_mock_app_for_tests(false);
        let jobs_dir = std::env::temp_dir().join(format!("mcp-route-jobs-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&jobs_dir).expect("create test jobs dir");
        let jobs = Arc::new(LocalJobRegistry::new(jobs_dir).expect("build test job registry"));
        let grant = session_all_token_grant();
        let manifest = LoopbackManifest {
            runtime_profile: "test",
            bind_host: "127.0.0.1",
            port: 0,
            api_url: "http://127.0.0.1:0".to_string(),
            mcp_url: "http://127.0.0.1:0/mcp".to_string(),
            openapi_url: "http://127.0.0.1:0/openapi.json".to_string(),
            token: OWNER_TOKEN.to_string(),
            pid: 0,
            started_at: "1970-01-01T00:00:00Z".to_string(),
            manifest_path: "test".to_string(),
            auth_header: "Authorization: Bearer <token>",
            token_audience: "test",
            token_storage: "test",
            security_warning: "test",
            cell_graph_id: None,
            cell_owner: None,
            cell_generation: None,
            cell_registry_revision: None,
            capabilities: grant.scopes.clone(),
            token_scope_mode: grant.scope_mode,
            token_scopes: grant.scopes,
            scope_details: grant.scope_details,
            grant_profiles: grant.grant_profiles,
        };
        Arc::new(LoopbackState {
            app,
            token: OWNER_TOKEN.to_string(),
            manifest,
            jobs,
            lifecycle: Arc::new(CellLifecycle::new()),
            cell_graph: Arc::new(CellGraphBoundary::for_test(None)),
        })
    }

    async fn initialize_as(state: &Arc<LoopbackState>, token: &str) -> serde_json::Value {
        let request = Request::builder()
            .method(Method::POST)
            .uri("/mcp")
            .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" })
                    .to_string(),
            ))
            .unwrap();
        let response = loopback_mcp_router()
            .with_state(state.clone())
            .oneshot(request)
            .await
            .expect("route response");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body");
        serde_json::from_slice(&body).expect("JSON response")
    }

    #[tokio::test]
    async fn initialize_welcomes_each_key_with_the_tools_it_has() {
        let _guard = env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile_dir =
            std::env::temp_dir().join(format!("mcp-route-welcome-{}", Uuid::new_v4()));
        std::env::set_var("GARDEN_PROFILE_DIR", &profile_dir);
        let state = test_state();
        let mut tokens = std::collections::HashMap::new();
        for profile in ["garden", "read-only"] {
            let created = create_client_token(
                &state.app,
                CreateLoopbackClientTokenInput {
                    label: Some(format!("stranger-{profile}")),
                    grant_profile_id: Some(profile.to_string()),
                    scopes: None,
                    expires_in_days: None,
                },
            )
            .expect("create named token");
            tokens.insert(profile, created.token);
        }

        let full = initialize_as(&state, &tokens["garden"]).await;
        assert_eq!(full["result"]["serverInfo"]["name"], "mycl-local");
        let welcome = full["result"]["instructions"].as_str().expect("instructions");
        assert!(welcome.starts_with("Mycl is a local knowledge graph"), "{welcome}");
        assert!(welcome.contains("call list_graphs, then set_home_graph."), "{welcome}");
        assert!(welcome.contains("- Song (music and sing)"), "{welcome}");

        let read_only = initialize_as(&state, &tokens["read-only"]).await;
        let welcome = read_only["result"]["instructions"].as_str().expect("instructions");
        assert!(welcome.starts_with("Mycl is a local knowledge graph"), "{welcome}");
        assert!(!welcome.contains("edit_block_text"), "{welcome}");
        assert!(!welcome.contains("wire them"), "{welcome}");

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(profile_dir);
    }

    #[tokio::test]
    async fn stored_profile_reaches_auth_and_excluded_tool_fails_through_route() {
        let _guard = env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile_dir =
            std::env::temp_dir().join(format!("mcp-route-profile-{}", Uuid::new_v4()));
        std::env::set_var("GARDEN_PROFILE_DIR", &profile_dir);
        let state = test_state();
        let created = create_client_token(
            &state.app,
            CreateLoopbackClientTokenInput {
                label: Some("epsilon".to_string()),
                grant_profile_id: Some("read-only".to_string()),
                scopes: None,
                expires_in_days: None,
            },
        )
        .expect("create named token");

        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {}", created.token).parse().unwrap(),
        );
        let principal = authorized_loopback_token(&headers, &state, &["mcp.tools.call"])
            .expect("named token authorizes MCP calls");
        assert_eq!(principal.label.as_deref(), Some("epsilon"));
        assert_eq!(principal.grant_profile_id.as_deref(), Some("read-only"));

        let request = Request::builder()
            .method(Method::POST)
            .uri("/mcp")
            .header(
                axum::http::header::AUTHORIZATION,
                format!("Bearer {}", created.token),
            )
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/call",
                    "params": {
                        "name": "write_document",
                        "arguments": {
                            "graph_id": "default",
                            "document_id": "doc",
                            "content": "must not run"
                        }
                    }
                })
                .to_string(),
            ))
            .unwrap();
        let response = loopback_mcp_router()
            .with_state(state)
            .oneshot(request)
            .await
            .expect("route response");
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body");
        let body: serde_json::Value = serde_json::from_slice(&body).expect("JSON response");
        assert_eq!(body["error"]["code"], -32602);
        assert_eq!(
            body["error"]["message"],
            "this key doesn't allow write_document; the graph's owner can issue a key that does"
        );

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(profile_dir);
    }
}
