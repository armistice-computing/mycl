use crate::{
    loopback_scopes::{crdt_operation_scopes, delete_scopes, mcp_tool_scopes},
    loopback_state::LoopbackState,
    mcp_dispatch_registry::{lookup, McpCallCtx},
    mcp_rpc_protocol::{
        mcp_error, mcp_error_with_data, mcp_scope_error, mcp_success, McpRpcResponse,
    },
    mcp_tool_registry::{mcp_tool_accepts_graph_selector, mcp_tool_allowed_for_grant_profile},
};

/// How a call to a tool outside the key's profile is refused. A tool that
/// exists and is withheld is a different answer from a name that is no tool
/// at all: the agent can tell its person what to ask for.
pub(crate) fn tool_outside_profile_message(name: &str) -> String {
    if lookup(name).is_some() {
        format!("this key doesn't allow {name}; the graph's owner can issue a key that does")
    } else {
        format!("unknown tool {name}")
    }
}

pub(crate) async fn handle_mcp_tool_call(
    state: &LoopbackState,
    id: Option<serde_json::Value>,
    params: serde_json::Value,
    token_scopes: &[String],
    grant_profile_id: Option<&str>,
    token_id: Option<&str>,
    home_graph_id: Option<&str>,
    role: crate::cell_graph_boundary::CellRole,
) -> McpRpcResponse {
    if let Some(error) = mcp_scope_error(token_scopes, id.clone(), "mcp.tools.call") {
        return error;
    }
    let name = params
        .get("name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let mut arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    if !mcp_tool_allowed_for_grant_profile(name, grant_profile_id) {
        return mcp_error(id, -32602, tool_outside_profile_message(name));
    }
    // Validate the caller's names BEFORE injecting server-owned arguments and
    // before any handler can enqueue a mutation. Never silently drop a typo.
    if let Err(error) = crate::mcp_argument_validation::validate_argument_names(name, &arguments) {
        return mcp_error(id, -32602, error);
    }
    let response_view = match crate::mcp_agent_presentation::ResponseView::from_params(&params) {
        Ok(view) => view,
        Err(error) => return mcp_error(id, -32602, error),
    };
    inject_home_graph(name, &mut arguments, home_graph_id);
    crate::mcp_profile_defaults::apply_profile_geist_defaults(
        name,
        &mut arguments,
        grant_profile_id,
    );
    if let Err(error) = crate::mcp_argument_schema::validate_argument_values(name, &arguments) {
        return mcp_error(id, -32602, error);
    }
    if let Err(error) = state.cell_graph.authorize_mcp_role(name, role) {
        return mcp_error(id, error.mcp_code(), error.mcp_message());
    }
    let required_scopes = match mcp_call_required_scopes(name, &arguments) {
        Ok(scopes) => scopes,
        Err(error) => return mcp_error(id, -32602, error),
    };
    if named_credential_would_guess_mutation_graph(
        name,
        &arguments,
        &required_scopes,
        token_id,
        home_graph_id,
    ) {
        // Name only the tools this key was given.
        let has = |tool: &str| mcp_tool_allowed_for_grant_profile(tool, grant_profile_id);
        let how = match (has("list_graphs"), has("set_home_graph")) {
            (true, true) => "call list_graphs, then set_home_graph (or pass graph_id on each call)",
            (true, false) => "call list_graphs and pass graph_id on each call",
            (false, _) => "pass graph_id on each call",
        };
        return mcp_error(
            id,
            -32602,
            format!("no home graph is set, so this write needs a graph: {how}"),
        );
    }
    for required_scope in required_scopes {
        if let Some(error) = mcp_scope_error(token_scopes, id.clone(), required_scope) {
            return error;
        }
    }

    let Some(entry) = lookup(name) else {
        return mcp_error(id, -32602, format!("unknown tool {name}"));
    };
    let arguments = match state
        .cell_graph
        .scope_mcp_arguments(name, arguments, &state.jobs)
    {
        Ok(arguments) => arguments,
        Err(error) => return mcp_error(id, error.mcp_code(), error.mcp_message()),
    };

    let prepared = match crate::mcp_inline_valuations::prepare(name, &arguments) {
        Ok(prepared) => prepared,
        Err(error) => return mcp_error(id, -32602, error),
    };
    if !prepared.ratings.is_empty() {
        if let Some(error) = mcp_scope_error(token_scopes, id.clone(), "salience.write") {
            return error;
        }
    }
    let arguments = &prepared.arguments;
    let ctx = McpCallCtx {
        app: state.app.clone(),
        jobs: &state.jobs,
        // Capture the credential principal BEFORE caller-supplied Chronicle
        // labels refine it. Rememberer is provenance, never an argument claim.
        credential_actor: memory_credential_actor(),
        token_id: token_id.map(str::to_string),
    };
    // The dispatcher boundary is no longer lossy about KIND-adjacent meaning:
    // every registry handler returns `AppResult<Value>`; the message stays
    // verbatim (doctrine: errors surface verbatim) and the taxonomy code
    // (`app_error_codes`), when the error was classified, rides in `data`.
    // The JSON-RPC code stays -32000 so no client's envelope handling changes.
    // Chronicle: the request principal (from the presented credential) is
    // refined by an explicit `agent` argument and scoped over the handler, so
    // any CRDT operation the tool enqueues is stamped with who wrote it.
    let actor = crate::document_chronicle::resolve_mcp_actor(&arguments);
    let result =
        crate::document_chronicle::with_actor(actor, (entry.handler)(&ctx, &arguments)).await;

    match result {
        Ok(mut value) => {
            crate::mcp_inline_valuations::apply(state.app.clone(), name, &prepared, &mut value)
                .await;
            mcp_success(id, response_view.envelope(name, value))
        }
        Err(error) => tool_failure_response(id, error),
    }
}

fn memory_credential_actor() -> Option<crate::document_chronicle::ChronicleActor> {
    crate::document_chronicle::current_actor().filter(|actor| match actor.via.as_deref() {
        Some("master-token") => actor.kind == "human" && actor.name == "owner",
        Some(via) => {
            actor.kind == "agent"
                && via
                    .strip_prefix("client-token:")
                    .is_some_and(|id| !id.is_empty())
        }
        None => false,
    })
}

#[cfg(test)]
mod memory_principal_tests {
    use super::*;
    use crate::document_chronicle::{resolve_mcp_actor, with_actor, ChronicleActor};

    #[tokio::test]
    async fn memory_principal_is_captured_before_argument_refinement() {
        assert!(memory_credential_actor().is_none());
        let principal = ChronicleActor::agent("epsilon", Some("client-token:test-id".into()));
        with_actor(principal.clone(), async {
            let captured = memory_credential_actor();
            let claimed = resolve_mcp_actor(&serde_json::json!({"agent":"delta"}));
            assert_eq!(claimed.name, "delta");
            with_actor(claimed, async {
                assert!(
                    memory_credential_actor().is_none(),
                    "refined claims are not credentials"
                );
                assert_eq!(captured, Some(principal.clone()));
            })
            .await;
        })
        .await;
        let owner = ChronicleActor::human_owner_via("master-token");
        with_actor(owner.clone(), async {
            assert_eq!(memory_credential_actor(), Some(owner.clone()));
        })
        .await;
        for unknown in [
            ChronicleActor::human_owner(),
            ChronicleActor::agent("epsilon", Some("client-token:".into())),
        ] {
            with_actor(unknown, async {
                assert!(memory_credential_actor().is_none());
            })
            .await;
        }
    }
}

fn named_credential_would_guess_mutation_graph(
    name: &str,
    arguments: &serde_json::Value,
    required_scopes: &[&str],
    token_id: Option<&str>,
    home_graph_id: Option<&str>,
) -> bool {
    token_id.is_some()
        && home_graph_id.is_none()
        && name != "create_graph"
        && name != "set_home_graph"
        && mcp_tool_accepts_graph_selector(name)
        && !arguments
            .as_object()
            .is_some_and(|object| object.contains_key("graph_id") || object.contains_key("graphId"))
        && required_scopes.iter().any(|scope| scope_is_mutating(scope))
}

fn scope_is_mutating(scope: &str) -> bool {
    scope.contains(".write")
        || scope.contains(".delete")
        || matches!(
            scope,
            "rdf.update"
                | "rdf.load"
                | "graphs.import"
                | "jobs.cancel"
                | "services.manage"
                | "services.proxy"
                | "time-travel.restore"
                | "sources.rebuild"
        )
}

fn inject_home_graph(name: &str, arguments: &mut serde_json::Value, home_graph_id: Option<&str>) {
    let Some(key) = crate::mcp_argument_schema::home_graph_key(name) else {
        return;
    };
    let Some(home_graph_id) = home_graph_id else {
        return;
    };
    let Some(object) = arguments.as_object_mut() else {
        return;
    };
    if !object.contains_key("graph_id") && !object.contains_key("graphId") {
        object.insert(
            key.to_string(),
            serde_json::Value::String(home_graph_id.to_string()),
        );
    }
}

/// The dispatcher boundary's error→envelope mapping (§4.2.5): the message
/// stays verbatim (doctrine: errors surface verbatim) and the taxonomy code
/// (`app_error_codes`), when the error was classified, rides in `data`. The
/// JSON-RPC code stays `-32000` so no client's envelope handling changes.
/// Factored out of [`handle_mcp_tool_call`] so it is unit-testable without a
/// full `LoopbackState`/tool registry.
fn tool_failure_response(
    id: Option<serde_json::Value>,
    error: crate::app_error::AppError,
) -> McpRpcResponse {
    let data = error.code().map(|code| serde_json::json!({ "code": code }));
    mcp_error_with_data(id, -32000, error.message(), data)
}

fn mcp_call_required_scopes(
    name: &str,
    arguments: &serde_json::Value,
) -> Result<Vec<&'static str>, String> {
    if name == "move_documents" {
        let source = arguments.get("graphId").and_then(serde_json::Value::as_str);
        let target = arguments
            .get("targetGraphId")
            .and_then(serde_json::Value::as_str);
        return Ok(if target.is_some() && target != source {
            crdt_operation_scopes("workspace.moveDocumentsAcrossGraphs")
                .expect("registered cross-graph scopes")
        } else {
            vec!["workspace.write.crdt"]
        });
    }
    if name == "delete" {
        let delete_type = arguments
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        return delete_scopes(&delete_type)
            .ok_or_else(|| format!("unsupported delete target type {delete_type}"));
    }
    if name != "crdt_operation" {
        let mut scopes = mcp_tool_scopes(name);
        if name == "remember"
            && arguments
                .get("anchors")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|a| !a.is_empty())
        {
            scopes.extend(mcp_tool_scopes("read_document"));
        }
        // Naming a folder files or moves the document, which is workspace
        // work: the same scope move_documents asks for.
        if name == "write_document"
            && (arguments.get("parentId").is_some() || arguments.get("parent_id").is_some())
        {
            scopes.push("workspace.write.crdt");
        }
        if matches!(name, "read_document" | "document_digest")
            && arguments
                .get("includeMemories")
                .and_then(serde_json::Value::as_bool)
                == Some(true)
        {
            scopes.extend(mcp_tool_scopes("recall"));
        }
        // create_wires reaches the artifact store when a wire names a
        // region (its bytes are read and hashed) and the roots when it
        // names a path (a referenced record may be made): the same scopes
        // read_artifact and reference_artifact need.
        // insert_blocks from an artifact reads its bytes and makes wires;
        // get_wires about a path looks the file up under the roots.
        let names = |keys: &[&str]| keys.iter().any(|key| arguments.get(*key).is_some_and(|value| !value.is_null()));
        if name == "insert_blocks" && names(&["artifactId", "artifact_id", "artifactPath", "artifact_path"]) {
            scopes.extend(mcp_tool_scopes("read_artifact"));
            scopes.extend(mcp_tool_scopes("create_wires"));
            if names(&["artifactPath", "artifact_path"]) {
                scopes.extend(mcp_tool_scopes("reference_artifact"));
            }
        }
        if name == "get_wires" && names(&["artifactPath", "artifact_path"]) {
            scopes.extend(mcp_tool_scopes("read_artifact"));
        }
        if name == "create_wires" {
            let inputs = crate::mcp_wire_payloads::wire_inputs(arguments);
            let has = |keys: &[&str]| {
                inputs.iter().chain(std::iter::once(arguments)).any(|input| {
                    keys.iter().any(|key| input.get(*key).is_some_and(|value| !value.is_null()))
                })
            };
            if has(&["region", "targetRegion", "target_region"]) {
                scopes.extend(mcp_tool_scopes("read_artifact"));
            }
            if has(&["artifactPath", "artifact_path"]) {
                scopes.extend(mcp_tool_scopes("reference_artifact"));
            }
        }
        scopes.sort_unstable();
        scopes.dedup();
        return Ok(scopes);
    }

    let kind = arguments
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .trim();
    let mut scopes = crdt_operation_scopes(kind)
        .ok_or_else(|| format!("unsupported local CRDT operation kind {kind}"))?;
    // The raw operation reaches the artifact's bytes when it names a
    // region, like the tool does.
    if kind == "workspace.createWire"
        && crate::artifact_regions::payload_names_region(arguments.get("payload").unwrap_or(&serde_json::Value::Null))
    {
        scopes.extend(mcp_tool_scopes("read_artifact"));
        scopes.sort_unstable();
        scopes.dedup();
    }
    Ok(scopes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_write_that_names_a_folder_needs_the_workspace_scope() {
        let plain = mcp_call_required_scopes(
            "write_document",
            &serde_json::json!({ "graphId": "g", "documentId": "d", "content": "x" }),
        )
        .unwrap();
        assert!(plain.contains(&"documents.write.crdt"));
        assert!(!plain.contains(&"workspace.write.crdt"));
        for placement in [
            serde_json::json!({ "graphId": "g", "documentId": "d", "content": "x", "parentId": "folder-a" }),
            serde_json::json!({ "graphId": "g", "documentId": "d", "content": "x", "parentId": null }),
            serde_json::json!({ "graphId": "g", "documentId": "d", "content": "x", "parent_id": "folder-a" }),
        ] {
            let scopes = mcp_call_required_scopes("write_document", &placement).unwrap();
            assert!(scopes.contains(&"documents.write.crdt"), "{placement}");
            assert!(scopes.contains(&"workspace.write.crdt"), "{placement}");
        }
    }

    #[test]
    fn cross_graph_move_requires_content_and_retirement_authority() {
        let local = mcp_call_required_scopes(
            "move_documents",
            &serde_json::json!({"graphId":"a", "targetGraphId":"a"}),
        )
        .unwrap();
        assert_eq!(local, vec!["workspace.write.crdt"]);
        let cross = mcp_call_required_scopes(
            "move_documents",
            &serde_json::json!({"graphId":"a", "targetGraphId":"b"}),
        )
        .unwrap();
        for scope in [
            "documents.read",
            "documents.write.crdt",
            "documents.delete.crdt",
            "workspace.delete.crdt",
            "wires.delete",
            "salience.write",
        ] {
            assert!(cross.contains(&scope));
        }
        assert_eq!(
            cross,
            mcp_call_required_scopes(
                "crdt_operation",
                &serde_json::json!({"kind":"workspace.moveDocumentsAcrossGraphs"})
            )
            .unwrap()
        );
        let cell = crate::cell_graph_boundary::CellGraphBoundary::for_test(Some("a"));
        assert!(cell
            .authorize_crdt_operation(
                "workspace.moveDocumentsAcrossGraphs",
                "a",
                &serde_json::json!({"targetGraphId":"b"})
            )
            .is_err());
    }

    #[test]
    fn memory_context_does_not_bypass_the_other_read_scope() {
        assert!(
            !mcp_call_required_scopes("remember", &serde_json::json!({}))
                .unwrap()
                .contains(&"documents.read")
        );
        assert!(mcp_call_required_scopes(
            "remember",
            &serde_json::json!({"anchors":[{"documentId":"d"}]})
        )
        .unwrap()
        .contains(&"documents.read"));
        for tool in ["read_document", "document_digest"] {
            assert!(!mcp_call_required_scopes(tool, &serde_json::json!({}))
                .unwrap()
                .contains(&"memory.read"));
            assert!(
                mcp_call_required_scopes(tool, &serde_json::json!({"includeMemories":true}))
                    .unwrap()
                    .contains(&"memory.read")
            );
        }
    }

    #[test]
    fn mcp_crdt_operation_scopes_are_selected_from_kind_argument() {
        assert_eq!(
            mcp_call_required_scopes(
                "crdt_operation",
                &serde_json::json!({"kind": " workspace.deleteDocument "}),
            ),
            Ok(vec!["documents.delete.crdt", "workspace.delete.crdt"])
        );
        assert_eq!(
            mcp_call_required_scopes("write_document", &serde_json::json!({})),
            Ok(vec!["documents.write.crdt"])
        );
        assert!(mcp_call_required_scopes(
            "crdt_operation",
            &serde_json::json!({"kind": "unknown.operation"}),
        )
        .is_err());
    }

    #[test]
    fn mcp_delete_scopes_are_selected_from_type_argument() {
        assert_eq!(
            mcp_call_required_scopes("delete", &serde_json::json!({"type": "blocks"})),
            Ok(vec!["documents.delete.crdt"])
        );
        assert_eq!(
            mcp_call_required_scopes("delete", &serde_json::json!({"type": "documents"})),
            Ok(vec!["documents.delete.crdt", "workspace.delete.crdt"])
        );
        assert_eq!(
            mcp_call_required_scopes("delete", &serde_json::json!({"type": "wires"})),
            Ok(vec!["wires.delete"])
        );
        assert!(
            mcp_call_required_scopes("delete", &serde_json::json!({"type": "unknown"}),).is_err()
        );
    }

    #[test]
    fn home_graph_is_injected_only_when_the_caller_did_not_choose_a_graph() {
        let mut create = serde_json::json!({"title":"T"});
        inject_home_graph("create_document", &mut create, Some("home"));
        assert_eq!(create["graphId"], "home");
        assert!(
            crate::mcp_argument_schema::validate_argument_values("create_document", &create)
                .is_ok()
        );
        for tool in ["create_graph", "set_home_graph", "list_graphs"] {
            let mut arguments = serde_json::json!({});
            inject_home_graph(tool, &mut arguments, Some("home"));
            assert_eq!(arguments, serde_json::json!({}));
        }
        let mut omitted = serde_json::json!({"document_id": "doc"});
        inject_home_graph("read_document", &mut omitted, Some("home"));
        assert_eq!(omitted["graphId"], "home");

        let mut snake_case = serde_json::json!({"graph_id": "explicit"});
        inject_home_graph("read_document", &mut snake_case, Some("home"));
        assert_eq!(snake_case["graph_id"], "explicit");

        let mut camel_case = serde_json::json!({"graphId": "explicit"});
        inject_home_graph("read_document", &mut camel_case, Some("home"));
        assert_eq!(camel_case["graphId"], "explicit");
        assert!(camel_case.get("graph_id").is_none());
    }

    #[test]
    fn named_credentials_never_guess_a_graph_for_mutations() {
        let write_scopes = vec!["documents.write.crdt"];
        assert!(named_credential_would_guess_mutation_graph(
            "write_document",
            &serde_json::json!({"document_id": "doc"}),
            &write_scopes,
            Some("token-id"),
            None,
        ));
        assert!(!named_credential_would_guess_mutation_graph(
            "write_document",
            &serde_json::json!({"graph_id": "chosen", "document_id": "doc"}),
            &write_scopes,
            Some("token-id"),
            None,
        ));
        assert!(!named_credential_would_guess_mutation_graph(
            "write_document",
            &serde_json::json!({"document_id": "doc"}),
            &write_scopes,
            Some("token-id"),
            Some("home"),
        ));
        assert!(!named_credential_would_guess_mutation_graph(
            "write_document",
            &serde_json::json!({"document_id": "doc"}),
            &write_scopes,
            None,
            None,
        ));
    }

    #[test]
    fn named_credentials_may_use_read_fallback_and_create_or_set_graphs() {
        assert!(!named_credential_would_guess_mutation_graph(
            "read_document",
            &serde_json::json!({"document_id": "doc"}),
            &["documents.read"],
            Some("token-id"),
            None,
        ));
        assert!(!named_credential_would_guess_mutation_graph(
            "create_graph",
            &serde_json::json!({"title": "New"}),
            &["graphs.write"],
            Some("token-id"),
            None,
        ));
        assert!(!named_credential_would_guess_mutation_graph(
            "set_home_graph",
            &serde_json::json!({"graph_id": null}),
            &["orientation.write"],
            Some("token-id"),
            None,
        ));
    }

    #[test]
    fn tool_failure_carries_the_code_in_error_data() {
        let response = tool_failure_response(
            Some(serde_json::json!(7)),
            crate::app_error::AppError::conflict(
                "stale graph incarnation for g: expected a, actual b",
            )
            .with_code(crate::app_error_codes::STALE_GRAPH_INCARNATION),
        );
        let value = serde_json::to_value(&response).unwrap();
        assert_eq!(value["error"]["code"], -32000);
        assert_eq!(
            value["error"]["message"],
            "stale graph incarnation for g: expected a, actual b"
        );
        assert_eq!(value["error"]["data"]["code"], "stale_graph_incarnation");
    }

    #[test]
    fn unclassified_tool_failure_omits_error_data() {
        let response = tool_failure_response(None, crate::app_error::AppError::internal("boom"));
        let value = serde_json::to_value(&response).unwrap();
        assert_eq!(value["error"]["code"], -32000);
        assert_eq!(value["error"]["message"], "boom");
        assert!(
            value["error"].get("data").is_none(),
            "data must be absent, not null, when the error carries no code"
        );
    }
}
