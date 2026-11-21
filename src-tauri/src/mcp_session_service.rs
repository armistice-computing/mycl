use crate::app_runtime::AppHandle;
use crate::{
    clock::timestamp,
    loopback_client_token_service::set_loopback_client_home_graph,
    mcp_utils::{mcp_arg_string, mcp_graph_id_or_default},
    profile_service::ensure_profile,
};

pub(super) fn mcp_local_get_user_location(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_graph_id_or_default(&app, arguments)?;
    let profile = ensure_profile(&app)?;
    Ok(serde_json::json!({
        "graph_id": graph_id,
        "document_id": null,
        "display_name": profile.display_name,
        "source": "fallback_no_active_session",
        "timestamp": timestamp(),
    }))
}

pub(super) fn mcp_local_set_home_graph(
    app: AppHandle,
    token_id: Option<&str>,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let token_id = token_id.ok_or_else(|| {
        "set_home_graph requires a named connector credential, not the owner master token"
            .to_string()
    })?;
    let raw_graph_id = arguments
        .get("graph_id")
        .or_else(|| arguments.get("graphId"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "graph_id is required; pass an empty string to clear it".to_string())?;
    let graph_id = mcp_arg_string(arguments, &["graph_id", "graphId"]);
    let home_graph = set_loopback_client_home_graph(&app, token_id, graph_id.as_deref())?;
    if raw_graph_id.trim().is_empty() {
        Ok(serde_json::json!({
            "home_graph": null,
            "message": "Home graph cleared.",
        }))
    } else {
        Ok(serde_json::json!({ "home_graph": home_graph }))
    }
}
