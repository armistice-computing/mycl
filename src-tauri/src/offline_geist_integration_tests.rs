//! Combined offline-MCP authorization and Geist provenance regression.
//! The parent registers this module; no live profile or listening socket is used.
#![cfg(all(test, feature = "headless"))]

use crate::{
    cell_graph_boundary::CellGraphBoundary,
    cell_lifecycle::CellLifecycle,
    graph_service::{create_graph_service, read_graph_record, CreateGraphInput},
    local_jobs::LocalJobRegistry,
    loopback_state::{LoopbackManifest, LoopbackState},
    loopback_token_grants::session_all_token_grant,
};
use axum::{
    body::Body,
    http::{header, Method, Request, StatusCode},
    middleware, Router,
};
use serde_json::{json, Value};
use std::{ffi::OsString, fs, path::PathBuf, sync::Arc};
use tower::ServiceExt;

const OWNER_TOKEN: &str = "offline-geist-test-owner";
const GRAPH: &str = "offline-geist-integration";
const AGENT: &str = "epsilon-integration";

struct IsolatedProfile {
    path: PathBuf,
    previous: Option<OsString>,
}

impl IsolatedProfile {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("garden-offline-geist-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path).expect("reserve fresh isolated profile");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let previous = std::env::var_os("GARDEN_PROFILE_DIR");
        std::env::set_var("GARDEN_PROFILE_DIR", &path);
        Self { path, previous }
    }
}

impl Drop for IsolatedProfile {
    fn drop(&mut self) {
        // Match the migration harness: a failed queued operation must never
        // resume against the caller's real profile during panic unwinding.
        if std::thread::panicking() {
            eprintln!(
                "failed isolated Geist profile retained at {}",
                self.path.display()
            );
            return;
        }
        match &self.previous {
            Some(value) => std::env::set_var("GARDEN_PROFILE_DIR", value),
            None => std::env::remove_var("GARDEN_PROFILE_DIR"),
        }
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn test_state(profile: &IsolatedProfile) -> Arc<LoopbackState> {
    let app = crate::tauri_runtime::build_mock_app_for_tests(true);
    create_graph_service(
        &app,
        CreateGraphInput {
            graph_id: Some(GRAPH.into()),
            title: "Offline Geist integration".into(),
            description: None,
            operation_id: None,
        },
    )
    .expect("create isolated graph");
    let (graph_dir, _) = read_graph_record(&app, GRAPH).unwrap();
    assert!(graph_dir
        .canonicalize()
        .unwrap()
        .starts_with(profile.path.canonicalize().unwrap()));
    let jobs_dir = profile.path.join("test-jobs");
    fs::create_dir(&jobs_dir).unwrap();
    let jobs = Arc::new(LocalJobRegistry::new(jobs_dir).unwrap());
    let grant = session_all_token_grant();
    let manifest = LoopbackManifest {
        runtime_profile: "test",
        bind_host: "127.0.0.1",
        port: 0,
        api_url: "http://127.0.0.1:0".into(),
        mcp_url: "http://127.0.0.1:0/mcp".into(),
        openapi_url: "http://127.0.0.1:0/openapi.json".into(),
        token: OWNER_TOKEN.into(),
        pid: 0,
        started_at: "1970-01-01T00:00:00Z".into(),
        manifest_path: profile
            .path
            .join("test-manifest.json")
            .display()
            .to_string(),
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
        token: OWNER_TOKEN.into(),
        manifest,
        jobs,
        lifecycle: Arc::new(CellLifecycle::new()),
        cell_graph: Arc::new(CellGraphBoundary::for_test(None)),
    })
}

async fn http(
    app: &Router,
    method: Method,
    path: &str,
    token: &str,
    body: Value,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app
        .clone()
        .oneshot(request)
        .await
        .expect("HTTP route response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).expect("JSON response"),
    )
}

async fn rpc(app: &Router, token: &str, method: &str, params: Value) -> Value {
    let (status, response) = http(
        app,
        Method::POST,
        "/mcp",
        token,
        json!({"jsonrpc":"2.0", "id":1, "method":method, "params":params}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(response["id"], 1, "{response}");
    response
}

async fn call(app: &Router, token: &str, name: &str, arguments: Value) -> Value {
    let response = rpc(
        app,
        token,
        "tools/call",
        json!({"name":name,"arguments":arguments}),
    )
    .await;
    assert!(response.get("error").is_none(), "{name}: {response}");
    assert_ne!(response["result"]["isError"], true, "{name}: {response}");
    response["result"]["structuredContent"].clone()
}

#[tokio::test]
async fn named_hivemind_http_memory_preserves_credential_provenance_and_revocation() {
    let _serial = crate::tauri_runtime::profile_env_serial()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let profile = IsolatedProfile::new();
    let state = test_state(&profile);
    // Production handlers enforce live token authorization; the production
    // middleware supplies the actor. Never inject a fabricated McpCallCtx.
    let app = crate::loopback_mcp_routes::loopback_mcp_router()
        .merge(crate::loopback_owner_routes::loopback_owner_router())
        .merge(crate::loopback_core_routes::loopback_core_router())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            crate::loopback_actor_middleware::loopback_actor_middleware,
        ))
        .with_state(state.clone());
    let (status, issued) = http(
        &app,
        Method::POST,
        "/owner/connectors",
        OWNER_TOKEN,
        json!({"label":AGENT,"grantProfileId":"hivemind"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "owner credential issuance failed");
    assert_eq!(issued["connector"]["label"], AGENT);
    assert_eq!(issued["connector"]["grantProfileId"], "hivemind");
    let token = issued["token"].as_str().expect("issued credential");
    let token_id = issued["connector"]["tokenId"]
        .as_str()
        .expect("credential ID");
    let actor = json!({"kind":"agent","name":AGENT,"via":format!("client-token:{token_id}")});

    let (status, manifest) = http(&app, Method::GET, "/manifest", OWNER_TOKEN, json!({})).await;
    assert_eq!(status, StatusCode::OK);
    // GET /manifest is secret-free for every principal, the owner included.
    // Do not put a secret-bearing payload in assertion diagnostics.
    assert!(manifest.get("token").is_none());
    assert!(!manifest.to_string().contains(OWNER_TOKEN));
    assert!(manifest["mcpUrl"].is_string());
    let (status, denied) = http(&app, Method::GET, "/manifest", token, json!({})).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(denied.get("token").is_none());
    assert!(!denied.to_string().contains(OWNER_TOKEN));

    // Bypass issuance validation to reproduce an on-disk legacy owner-scope
    // credential. The actor middleware does not reject requests, so /manifest
    // still exercises the owner handler even if token resolution rejects it.
    let legacy_token = format!("legacy-test-{}", uuid::Uuid::new_v4());
    let mut store = crate::loopback_client_token_store::read_token_store(&state.app).unwrap();
    let mut legacy = store
        .tokens
        .iter()
        .find(|record| record.token_id == token_id)
        .unwrap()
        .clone();
    legacy.token_id = uuid::Uuid::new_v4().to_string();
    legacy.label = "legacy-owner-scope-test".into();
    legacy.token_hash = crate::loopback_client_token_records::hash_loopback_token(&legacy_token);
    legacy.grant_profile_id = None;
    legacy.scopes = vec!["loopback.manifest.read".into(), "mcp.tools.read".into()];
    store.tokens.push(legacy);
    crate::loopback_client_token_store::write_token_store(&state.app, &store).unwrap();
    let (status, denied) = http(&app, Method::GET, "/manifest", &legacy_token, json!({})).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "legacy scope cannot authorize the owner handler"
    );
    assert!(denied.get("token").is_none());
    assert!(!denied.to_string().contains(OWNER_TOKEN));
    let (status, denied) = http(
        &app,
        Method::POST,
        "/mcp",
        &legacy_token,
        json!({"jsonrpc":"2.0","id":3,"method":"tools/list","params":{}}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "legacy owner scopes must fail live credential authorization"
    );
    assert!(denied.get("result").is_none());

    let listed = rpc(&app, token, "tools/list", json!({})).await;
    let catalog = listed["result"]["tools"].as_array().expect("tool catalog");
    let remember = catalog
        .iter()
        .find(|tool| tool["name"] == "remember")
        .expect("remember advertised");
    assert_eq!(remember["inputSchema"]["required"], json!(["content"]));
    assert_eq!(remember["inputSchema"]["additionalProperties"], false);
    let properties = remember["inputSchema"]["properties"].as_object().unwrap();
    for typed in ["sources", "observer", "kind", "rememberer"] {
        assert!(
            !properties.contains_key(typed),
            "lightweight remember must not advertise {typed}"
        );
    }
    let recall = catalog
        .iter()
        .find(|tool| tool["name"] == "recall")
        .expect("recall advertised");
    assert_eq!(recall["inputSchema"]["properties"]["limit"]["default"], 8);
    assert_eq!(
        recall["inputSchema"]["properties"]["rememberer"]["type"],
        "string"
    );
    for excluded in ["care", "archive_memories"] {
        assert!(!catalog.iter().any(|tool| tool["name"] == excluded));
        let denied = rpc(
            &app,
            token,
            "tools/call",
            json!({"name":excluded,"arguments":{"graph_id":GRAPH,"numbers":[1]}}),
        )
        .await;
        assert_eq!(denied["error"]["code"], -32602, "{denied}");
        assert_eq!(
            denied["error"]["message"],
            crate::mcp_tool_dispatch::tool_outside_profile_message(excluded)
        );
    }

    let home = call(&app, token, "set_home_graph", json!({"graph_id":GRAPH})).await;
    assert_eq!(home["home_graph"], GRAPH);
    // create_document is a frontend/owner tool, not in the hivemind profile.
    // Subsequent agent writes/edits intentionally omit the graph selector.
    let created = call(&app, OWNER_TOKEN, "create_document", json!({"graphId":GRAPH,"documentId":"home-test","title":"Home test"})).await;
    assert!(created.is_object());
    let written = call(&app, token, "write_document", json!({"documentId":"home-test","content":"A test paragraph","format":"markdown"})).await;
    let block_id = written["blockIds"][0].as_str().expect("written block");
    for edit in [json!({"blockId":block_id,"atrrs":{"checked":true}}), json!({"blockId":block_id,"attrs":{"checked":true},"typo":1})] {
        let bad = rpc(&app, token, "tools/call", json!({"name":"update_blocks","arguments":{"documentId":"home-test","edits":[edit]}})).await;
        assert_eq!(bad["error"]["code"], -32602, "{bad}");
    }
    let changed = call(&app, token, "update_blocks", json!({"documentId":"home-test","edits":[{"block_id":block_id,"attributes":{"checked":true,"customAttribute":"kept"}}]})).await;
    assert_eq!(changed["updated"][0], block_id);
    let first = call(
        &app,
        token,
        "remember",
        json!({"content":"continuity from credential"}),
    )
    .await;
    assert_eq!(first["success"], true);
    assert_eq!(first["content"], "continuity from credential");
    assert_eq!(first["rememberer"], actor);
    let spoofed = call(
        &app,
        token,
        "remember",
        json!({"content":"continuity despite agent spoof","agent":"forged-agent"}),
    )
    .await;
    assert_eq!(spoofed["success"], true);
    assert_eq!(spoofed["rememberer"], actor);
    assert_ne!(first["number"], spoofed["number"]);

    for filter in [AGENT.to_string(), format!("agent:{AGENT}")] {
        let recalled = call(
            &app,
            token,
            "recall",
            json!({"rememberer":filter,"query":"continuity"}),
        )
        .await;
        assert_eq!(recalled["count"], 2, "{recalled}");
        let memories = recalled["memories"].as_array().unwrap();
        for written in [&first, &spoofed] {
            let memory = memories
                .iter()
                .find(|memory| memory["number"] == written["number"])
                .expect("written memory recalled");
            assert_eq!(memory["content"], written["content"]);
            assert_eq!(memory["rememberer"], actor);
        }
    }
    let forged = call(&app, token, "recall", json!({"rememberer":"forged-agent"})).await;
    assert_eq!(forged["count"], 0);
    assert_eq!(forged["memories"], json!([]));

    let (status, revoked) = http(
        &app,
        Method::DELETE,
        &format!("/owner/connectors/{token_id}"),
        OWNER_TOKEN,
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{revoked}");
    assert_eq!(revoked["active"], false);
    let (status, rejected) = http(
        &app,
        Method::POST,
        "/mcp",
        token,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
            "name":"remember","arguments":{"graph_id":GRAPH,"content":"must not persist"}
        }}),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{rejected}");
    let remaining = call(
        &app,
        OWNER_TOKEN,
        "recall",
        json!({"graph_id":GRAPH,"limit":100}),
    )
    .await;
    assert_eq!(
        remaining["count"], 2,
        "revoked write must not reach Geist: {remaining}"
    );
}

#[tokio::test]
async fn mcp_agent_boundary_rejects_typos_before_writes_and_keeps_machine_results() {
    let _serial = crate::tauri_runtime::profile_env_serial().lock().unwrap_or_else(|p| p.into_inner());
    let profile = IsolatedProfile::new();
    let state = test_state(&profile);
    let app = crate::loopback_mcp_routes::loopback_mcp_router().with_state(state.clone());
    let before = call(&app, OWNER_TOKEN, "recall", json!({"graphId":GRAPH})).await;
    for (name, arguments, meta) in [
        ("remember", json!({"graphId":GRAPH,"content":"must not write","contnet":"typo"}), json!({})),
        ("remember", json!({"graphId":GRAPH,"content":"must not write"}), json!({"garden/responseView":"agent-v99"})),
        ("insert_blocks", json!({"graphId":GRAPH,"documentId":"missing","content":"must not insert","after_block_id":"b"}), json!({})),
        ("sing", json!({"graphId":GRAPH,"content":"must not rotate"}), json!({})),
    ] {
        let response = rpc(&app, OWNER_TOKEN, "tools/call", json!({"name":name,"arguments":arguments,"_meta":meta})).await;
        assert_eq!(response["error"]["code"], -32602, "{response}");
        assert!(response.get("result").is_none());
    }
    assert_eq!(call(&app, OWNER_TOKEN, "recall", json!({"graphId":GRAPH})).await, before);
    call(&app, OWNER_TOKEN, "remember", json!({"graphId":GRAPH,"content":"boundary test memory"})).await;
    let machine = call(&app, OWNER_TOKEN, "recall", json!({"graphId":GRAPH})).await;
    let agent = rpc(&app, OWNER_TOKEN, "tools/call", json!({"name":"recall","arguments":{"graphId":GRAPH},"_meta":{"garden/responseView":"agent-v1"}})).await;
    assert!(machine["memories"][0].get("text").is_some());
    assert!(agent["result"]["structuredContent"]["memories"][0].get("text").is_none());
    let raw_ms = machine["memories"][0]["createdAt"].as_str().unwrap().parse::<i64>().unwrap();
    let displayed = chrono::DateTime::parse_from_rfc3339(agent["result"]["structuredContent"]["memories"][0]["createdAt"].as_str().unwrap()).unwrap();
    assert_eq!(displayed.timestamp_millis(),raw_ms);
    assert_eq!(agent["result"]["structuredContent"]["memories"][0]["content"], machine["memories"][0]["content"]);
    assert_eq!(call(&app, OWNER_TOKEN, "recall", json!({"graphId":GRAPH})).await, machine);
    let (status, _) = http(&app, Method::POST, "/mcp", "invalid-token", json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"recall","arguments":{"graphId":GRAPH},"_meta":{"garden/responseView":"agent-v1"}}})).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn memory_anchors_round_trip_without_changing_attention_or_workspace() {
    let _serial = crate::tauri_runtime::profile_env_serial().lock().unwrap_or_else(|p| p.into_inner());
    let profile = IsolatedProfile::new();
    let state = test_state(&profile);
    let app = crate::loopback_mcp_routes::loopback_mcp_router()
        .merge(crate::loopback_owner_routes::loopback_owner_router()).with_state(state.clone());
    call(&app, OWNER_TOKEN, "create_document", json!({"graphId":GRAPH,"documentId":"anchor-target","title":"Anchor target"})).await;
    let written = call(&app, OWNER_TOKEN, "write_document", json!({"graphId":GRAPH,"documentId":"anchor-target","content":"A lasting paragraph.","format":"markdown"})).await;
    let block = written["blockIds"][0].as_str().unwrap();
    call(&app, OWNER_TOKEN, "value", json!({"graphId":GRAPH,"documentId":"anchor-target","blockId":block,"importance":4,"valence":2})).await;
    let (dir, _) = read_graph_record(&state.app, GRAPH).unwrap();
    let path = dir.join("memory/memory-queue.json");
    let values_path = dir.join("values/block-values.json");
    let values_before = fs::read(&values_path).unwrap();
    let before_workspace = call(&app, OWNER_TOKEN, "get_workspace", json!({"graphId":GRAPH})).await;
    for anchors in [json!([{"documentId":"missing"}]),json!([{"documentId":"anchor-target","blockId":"missing"}]),json!([{"documentId":"anchor-target","graphId":"other"}])] {
        let bad = rpc(&app, OWNER_TOKEN, "tools/call", json!({"name":"remember","arguments":{"graphId":GRAPH,"content":"invalid","anchors":anchors}})).await;
        assert!(bad.get("error").is_some(), "{bad}");
        assert!(!path.exists(), "invalid references must not allocate a memory");
    }
    for n in 0..7 {
        let remembered = call(&app, OWNER_TOKEN, "remember", json!({"graphId":GRAPH,"content":format!("Memory {n}"),"anchors":[{"documentId":"anchor-target","blockId":block},{"documentId":"anchor-target","blockId":block}]})).await;
        assert_eq!(remembered["anchors"].as_array().unwrap().len(),1);
        assert_eq!(remembered["anchors"][0]["titleAtCreation"],"Anchor target");
    }
    let persisted = fs::read(&path).unwrap();
    for (scope, tool, arguments, missing) in [
        ("documents.read", "read_document", json!({"graphId":GRAPH,"documentId":"anchor-target","includeMemories":true}), "memory.read"),
        ("memory.write", "remember", json!({"graphId":GRAPH,"content":"no scope","anchors":[{"documentId":"anchor-target"}]}), "documents.read"),
    ] {
        let (status, issued) = http(&app, Method::POST, "/owner/connectors", OWNER_TOKEN, json!({"label":"narrow-anchor-test","scopes":["mcp.tools.call",scope]})).await;
        assert_eq!(status, StatusCode::OK, "{issued}");
        let denied = rpc(&app, issued["token"].as_str().unwrap(), "tools/call", json!({"name":tool,"arguments":arguments})).await;
        assert!(denied.get("result").is_none() && denied["error"].to_string().contains(missing), "{denied}");
    }
    let store = crate::geist_memory_store::read_memory_store(&dir, GRAPH).unwrap();
    assert_eq!(store.memories["1"].anchors[0].block_id.as_deref(),Some(block));
    let plain = call(&app, OWNER_TOKEN, "read_document", json!({"graphId":GRAPH,"documentId":"anchor-target","format":"markdown"})).await;
    assert!(plain.get("recallingMemories").is_none());
    let linked = call(&app, OWNER_TOKEN, "read_document", json!({"graphId":GRAPH,"documentId":"anchor-target","format":"markdown","includeMemories":true})).await;
    assert_eq!(linked["recallingMemories"]["total"],7);
    assert_eq!(linked["recallingMemories"]["memories"].as_array().unwrap().len(),5);
    assert_eq!(linked["recallingMemories"]["truncated"],true);
    assert_eq!(linked["recallingMemories"]["continuation"]["graphId"],GRAPH);
    let first = call(&app, OWNER_TOKEN, "recall", json!({"graphId":GRAPH,"documentId":"anchor-target","limit":4})).await;
    assert_eq!(first["page"]["nextOffset"],4);
    let second = call(&app, OWNER_TOKEN, "recall", json!({"graphId":GRAPH,"documentId":"anchor-target","limit":4,"offset":4})).await;
    assert_eq!(second["count"],3);
    assert_eq!(second["page"]["complete"],true);
    assert_eq!(fs::read(&path).unwrap(),persisted,"reads must not refresh or rewrite memory");
    let after_workspace = call(&app, OWNER_TOKEN, "get_workspace", json!({"graphId":GRAPH})).await;
    for key in ["counts","wires","documents","folders","artifacts"] {
        assert_eq!(before_workspace[key],after_workspace[key],"anchors must not change workspace {key}");
    }
    assert_eq!(fs::read(&values_path).unwrap(),values_before,"anchors/read context must not change valuations");
    call(&app, OWNER_TOKEN, "delete", json!({"graphId":GRAPH,"type":"documents","documentId":"anchor-target","hard":true})).await;
    let history = call(&app, OWNER_TOKEN, "recall", json!({"graphId":GRAPH,"number":1})).await;
    assert_eq!(history["memories"][0]["anchors"][0]["documentId"],"anchor-target","deleted targets do not erase testimony");
    assert_eq!(fs::read(&path).unwrap(),persisted);
}

#[tokio::test]
async fn inline_valuations_apply_to_final_ids_and_preserve_literals_and_permissions() {
    let _serial = crate::tauri_runtime::profile_env_serial().lock().unwrap_or_else(|p| p.into_inner());
    let profile = IsolatedProfile::new();let state = test_state(&profile);
    let app = crate::loopback_mcp_routes::loopback_mcp_router().merge(crate::loopback_owner_routes::loopback_owner_router()).with_state(state.clone());
    call(&app, OWNER_TOKEN, "create_document", json!({"graphId":GRAPH,"documentId":"inline-probe","title":"Inline probe"})).await;
    let original = call(&app, OWNER_TOKEN, "write_document", json!({"graphId":GRAPH,"documentId":"inline-probe","content":"# Original\n\nSecond paragraph","format":"markdown"})).await;
    let written = call(&app, OWNER_TOKEN, "write_document", json!({"graphId":GRAPH,"documentId":"inline-probe","content":"# Important heading {!4,+2}\n\nSecond paragraph","format":"markdown"})).await;
    assert_eq!(written["valuations"]["complete"],true,"{written}");
    assert_eq!(written["valuations"]["applied"],1);
    assert_eq!(written["blockIds"][0], original["blockIds"][0], "marked writes retain existing block identity");
    assert_eq!(written["title"],"Important heading");
    assert_eq!(written["sourceFormat"],"markdown");
    assert!(written.get("_inlineBlockIds").is_none());
    let inserted=call(&app,OWNER_TOKEN,"insert_blocks",json!({"graphId":GRAPH,"documentId":"inline-probe","content":"- [ ] Task **important** {!3,-1}","format":"markdown"})).await;
    assert_eq!(inserted["valuations"]["complete"],true,"{inserted}");
    assert_eq!(inserted["valuations"]["events"][0]["blockId"],inserted["block_ids"][0]);
    let xml=call(&app,OWNER_TOKEN,"insert_blocks",json!({"graphId":GRAPH,"documentId":"inline-probe","content":"<paragraph data-val-importance=\"5\">XML rating</paragraph>","format":"xml"})).await;
    assert_eq!(xml["valuations"]["complete"],true,"{xml}");
    let updated=call(&app,OWNER_TOKEN,"update_blocks",json!({"graphId":GRAPH,"documentId":"inline-probe","blockId":written["blockIds"][1],"attrs":{"data-val-valence":2}})).await;
    assert_eq!(updated["valuations"]["complete"],true,"{updated}");
    let read=call(&app,OWNER_TOKEN,"read_document",json!({"graphId":GRAPH,"documentId":"inline-probe","format":"xml"})).await;
    assert!(!read["content"].as_str().unwrap().contains("{!"));
    assert!(!read["content"].as_str().unwrap().contains("data-val-"));
    let values=call(&app,OWNER_TOKEN,"get_block_values",json!({"graphId":GRAPH,"documentId":"inline-probe"})).await;
    assert_eq!(values["count"],4,"{values}");
    let (dir,_) = read_graph_record(&state.app,GRAPH).unwrap();
    let path=dir.join("values/block-values.json");let before=fs::read(&path).unwrap();
    // Retry the rating, not the document write. The receipt is atomic with counts.
    let event=&written["valuations"]["events"][0];
    call(&app,OWNER_TOKEN,"value",json!({"graphId":GRAPH,"documentId":"inline-probe","blockId":event["blockId"],"importance":4,"valence":2,"valuationEventId":event["valuationEventId"]})).await;
    assert_eq!(fs::read(&path).unwrap(),before,"replay cannot add attention or refresh its timestamp");
    let (status,issued)=http(&app,Method::POST,"/owner/connectors",OWNER_TOKEN,json!({"label":"writer-no-values","scopes":["mcp.tools.call","documents.write.crdt"]})).await;
    assert_eq!(status,StatusCode::OK);
    let token=issued["token"].as_str().unwrap();
    let denied=rpc(&app,token,"tools/call",json!({"name":"insert_blocks","arguments":{"graphId":GRAPH,"documentId":"inline-probe","content":"DENIED {!5}","format":"markdown"}})).await;
    assert!(denied["error"].to_string().contains("salience.write"),"{denied}");
    let literal=call(&app,token,"insert_blocks",json!({"graphId":GRAPH,"documentId":"inline-probe","content":"```\nCode {!5}\n```\n\nInline `{!4}` example.\n\n**Midline {!4}** stays literal.\n\nInvalid {!9}","format":"markdown"})).await;
    assert!(literal.get("valuations").is_none());
    assert_eq!(fs::read(&path).unwrap(),before);
    let conflicting=rpc(&app,OWNER_TOKEN,"tools/call",json!({"name":"write_document","arguments":{"graphId":GRAPH,"documentId":"inline-probe","content":"Wrong {!5}","tiptapJson":{"type":"doc","content":[]}}})).await;
    assert_eq!(conflicting["error"]["code"],-32602);
    fs::write(&path,b"deliberately broken test store").unwrap();
    let partial=call(&app,OWNER_TOKEN,"insert_blocks",json!({"graphId":GRAPH,"documentId":"inline-probe","content":"Partial write {!2}","format":"markdown"})).await;
    assert_eq!(partial["success"],true);
    assert_eq!(partial["valuations"]["complete"],false);
    assert!(partial.get("valuationWarning").is_some());
    let read=call(&app,OWNER_TOKEN,"read_document",json!({"graphId":GRAPH,"documentId":"inline-probe","format":"markdown"})).await;
    assert!(read["content"].as_str().unwrap().contains("Partial write"));
    assert!(!read["content"].as_str().unwrap().contains("DENIED"));
    assert!(!read["content"].as_str().unwrap().contains("Partial write {!2}"));
    fs::write(&path,&before).unwrap();
    call(&app,OWNER_TOKEN,"value",partial["valuations"]["retry"].clone()).await;
    let once=fs::read(&path).unwrap();
    call(&app,OWNER_TOKEN,"value",partial["valuations"]["retry"].clone()).await;
    assert_eq!(fs::read(&path).unwrap(),once);
}

/// The Diary (Geist 2.1 L2) through the real HTTP+auth path: the credential is
/// the author, an `agent` argument cannot forge it, the four-sentence cap and
/// the closed argument set refuse before anything is written, two hands of one
/// agent interleave in one diary, the last lines ride `recall`, the projection
/// document is readable and derived, and the owner has no diary.
#[tokio::test]
async fn diary_is_written_by_the_credential_hand_and_rides_recall() {
    let _serial = crate::tauri_runtime::profile_env_serial()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let profile = IsolatedProfile::new();
    let state = test_state(&profile);
    let app = crate::loopback_mcp_routes::loopback_mcp_router()
        .merge(crate::loopback_owner_routes::loopback_owner_router())
        .merge(crate::loopback_core_routes::loopback_core_router())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            crate::loopback_actor_middleware::loopback_actor_middleware,
        ))
        .with_state(state.clone());
    let (status, issued) = http(
        &app,
        Method::POST,
        "/owner/connectors",
        OWNER_TOKEN,
        json!({"label":AGENT,"grantProfileId":"hivemind"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "owner credential issuance failed");
    let token = issued["token"].as_str().expect("issued credential");
    let token_id = issued["connector"]["tokenId"].as_str().expect("credential ID");
    let actor = json!({"kind":"agent","name":AGENT,"via":format!("client-token:{token_id}")});

    let listed = rpc(&app, token, "tools/list", json!({})).await;
    let catalog = listed["result"]["tools"].as_array().expect("tool catalog");
    for name in ["diary", "read_diary"] {
        assert!(catalog.iter().any(|tool| tool["name"] == name), "{name} advertised to hivemind");
    }

    // Refused before any write: a misspelled field, five sentences, a missing
    // entry. (An explicit empty entry is valid: the honest null, 2026-09-30.)
    for (arguments, why) in [
        (json!({"graph_id":GRAPH,"text":"wrong field"}), "closed argument set"),
        (json!({"graph_id":GRAPH,"entry":"One. Two. Three. Four. Five."}), "four-sentence cap"),
        (json!({"graph_id":GRAPH}), "missing entry"),
    ] {
        let bad = rpc(&app, token, "tools/call", json!({"name":"diary","arguments":arguments})).await;
        assert!(bad.get("error").is_some(), "{why}: {bad}");
    }
    let empty = call(&app, token, "read_diary", json!({"graph_id":GRAPH})).await;
    assert_eq!(empty["total"], 0, "{empty}");
    let before = call(&app, token, "recall", json!({"graph_id":GRAPH})).await;
    assert!(before.get("diary").is_none(), "an empty diary is silent in recall: {before}");

    let first = call(
        &app,
        token,
        "diary",
        json!({"graph_id":GRAPH,"entry":"I opened the door and   left it open.  Nothing else for the next hand.","hand":"terminal","watch":"0x10","agent":"forged-agent"}),
    )
    .await;
    assert_eq!(first["success"], true, "{first}");
    assert_eq!(first["agent"], AGENT);
    assert_eq!(first["author"], actor, "the credential is the author, not the label");
    assert_eq!(first["number"], 1);
    assert_eq!(first["hand"], "terminal");
    assert_eq!(first["watch"], "0x10");
    assert_eq!(first["text"], "I opened the door and left it open. Nothing else for the next hand.");
    assert_eq!(first["warnings"], json!([]), "projection wrote cleanly: {first}");
    let second = call(
        &app,
        token,
        "diary",
        json!({"graph_id":GRAPH,"entry":"Nothing for the next hand.","hand":"watch"}),
    )
    .await;
    assert_eq!(second["number"], 2);
    assert_eq!(second["entries"], 2);

    let read = call(&app, token, "read_diary", json!({"graph_id":GRAPH})).await;
    assert_eq!(read["agent"], AGENT);
    assert_eq!(read["total"], 2);
    let entries = read["entries"].as_array().unwrap();
    assert_eq!(entries[0]["hand"], "terminal");
    assert_eq!(entries[1]["hand"], "watch");
    assert_eq!(entries[1]["entryId"], format!("diary-{AGENT}-2"));
    let one = call(&app, token, "read_diary", json!({"graph_id":GRAPH,"limit":1,"agent":AGENT})).await;
    assert_eq!(one["count"], 1);
    assert_eq!(one["entries"][0]["number"], 2, "the page is the END of the diary");

    let recalled = call(&app, token, "recall", json!({"graph_id":GRAPH})).await;
    assert_eq!(recalled["diary"]["agent"], AGENT, "{recalled}");
    assert_eq!(recalled["diary"]["total"], 2);
    assert_eq!(recalled["diary"]["entries"].as_array().unwrap().len(), 2);
    let lean = call(&app, token, "recall", json!({"graph_id":GRAPH,"diaryLimit":1})).await;
    assert_eq!(lean["diary"]["entries"].as_array().unwrap().len(), 2, "1 requested, but the terminal hand is kept represented: {lean}");
    let none = call(&app, token, "recall", json!({"graph_id":GRAPH,"diaryLimit":0})).await;
    assert!(none.get("diary").is_none(), "diaryLimit 0 leaves the diary out: {none}");

    // anchors: validated before a number is allocated; a real target is kept with its title
    let bad_anchor = rpc(&app, token, "tools/call", json!({"name":"diary","arguments":{"graph_id":GRAPH,"entry":"Anchored to nothing.","anchors":[{"documentId":"no-such-doc"}]}})).await;
    assert!(bad_anchor.get("error").is_some(), "{bad_anchor}");
    let after = call(&app, token, "read_diary", json!({"graph_id":GRAPH})).await;
    assert_eq!(after["total"], 2, "a refused anchor allocates nothing");
    let anchored = call(
        &app,
        token,
        "diary",
        json!({"graph_id":GRAPH,"entry":"I left the ledger open for you.","hand":"terminal","anchors":[{"documentId":format!("geist-diary-agent-{AGENT}")}]}),
    )
    .await;
    assert_eq!(anchored["number"], 3);
    assert_eq!(anchored["anchors"][0]["documentId"], format!("geist-diary-agent-{AGENT}"));
    assert!(anchored["anchors"][0]["titleAtCreation"].as_str().unwrap().starts_with("Diary"), "{anchored}");

    let doc = call(
        &app,
        token,
        "read_document",
        json!({"graphId":GRAPH,"documentId":format!("geist-diary-agent-{AGENT}"),"format":"markdown"}),
    )
    .await;
    let content = doc["content"].as_str().expect("diary document renders");
    assert!(content.contains("Nothing for the next hand."), "{content}");
    assert!(content.find("#2").unwrap() < content.find("#1").unwrap(), "newest first: {content}");
    assert_eq!(doc["derived"], true, "the diary document is a geist view: {doc}");

    let owner = rpc(
        &app,
        OWNER_TOKEN,
        "tools/call",
        json!({"name":"diary","arguments":{"graph_id":GRAPH,"entry":"The owner has no diary."}}),
    )
    .await;
    assert!(owner.get("error").is_some(), "owner sessions have no diary: {owner}");
    let owner_read = rpc(
        &app,
        OWNER_TOKEN,
        "tools/call",
        json!({"name":"read_diary","arguments":{"graph_id":GRAPH}}),
    )
    .await;
    assert!(owner_read.get("error").is_some(), "read_diary needs an agent name without an agent credential: {owner_read}");
    let owner_named = call(&app, OWNER_TOKEN, "read_diary", json!({"graph_id":GRAPH,"agent":AGENT})).await;
    assert_eq!(owner_named["total"], 3, "the house can read any diary by name");
}

#[tokio::test]
async fn important_blocks_folder_scope_ranks_descendants_before_limit() {
    let _serial = crate::tauri_runtime::profile_env_serial().lock().unwrap_or_else(|p| p.into_inner());
    let profile = IsolatedProfile::new();
    let state = test_state(&profile);
    let app = crate::loopback_mcp_routes::loopback_mcp_router().with_state(state);
    for (id, parent) in [("scope", None), ("child", Some("scope")), ("empty", None)] {
        call(&app, OWNER_TOKEN, "create_folder", json!({"graphId":GRAPH,"folderId":id,"name":id,"parentId":parent})).await;
    }
    for (id, parent, importance) in [("outside", None, 5), ("direct", Some("scope"), 1), ("nested", Some("child"), 3)] {
        call(&app, OWNER_TOKEN, "create_document", json!({"graphId":GRAPH,"documentId":id,"title":id})).await;
        call(&app, OWNER_TOKEN, "write_document", json!({"graphId":GRAPH,"documentId":id,"content":format!("{} {{!{},+1}}", id, importance),"format":"markdown"})).await;
        call(&app, OWNER_TOKEN, "move_documents", json!({"graphId":GRAPH,"documentIds":[id],"parentId":parent})).await;
    }
    let result = call(&app, OWNER_TOKEN, "get_important_blocks", json!({"graphId":GRAPH,"documentId":"scope","limit":1})).await;
    assert_eq!(result["count"], 1, "{result}");
    assert_eq!(result["blocks"][0]["documentId"], "nested", "{result}");
    let all = call(&app, OWNER_TOKEN, "get_important_blocks", json!({"graphId":GRAPH,"document_id":"scope","limit":20})).await;
    assert_eq!(all["count"], 2, "{all}");
    for id in ["empty", "missing"] {
        let empty = call(&app, OWNER_TOKEN, "get_important_blocks", json!({"graphId":GRAPH,"documentId":id})).await;
        assert_eq!(empty["count"], 0, "{empty}");
    }
    let direct = call(&app, OWNER_TOKEN, "get_important_blocks", json!({"graphId":GRAPH,"documentId":"direct"})).await;
    assert_eq!(direct["count"], 1);
    let negative = call(&app, OWNER_TOKEN, "get_important_blocks", json!({"graphId":GRAPH,"documentId":"scope","valence":"negative"})).await;
    assert_eq!(negative["count"], 0);
}

#[tokio::test]
async fn hybrid_search_uses_values_before_truncation_and_degrades_explicitly() {
    let _serial = crate::tauri_runtime::profile_env_serial().lock().unwrap_or_else(|p| p.into_inner());
    let profile = IsolatedProfile::new();
    let state = test_state(&profile);
    let app = crate::loopback_mcp_routes::loopback_mcp_router().with_state(state.clone());
    for id in ["a-plain", "z-valued", "unrelated"] {
        call(&app, OWNER_TOKEN, "create_document", json!({"graphId":GRAPH,"documentId":id,"title":id})).await;
        let content = if id=="unrelated" {"Unrelated gardening."} else {"Theosis shared vocabulary."};
        let doc = call(&app, OWNER_TOKEN, "write_document", json!({"graphId":GRAPH,"documentId":id,"format":"markdown","content":content})).await;
        if id!="a-plain" {
            call(&app, OWNER_TOKEN, "value", json!({"graphId":GRAPH,"documentId":id,"blockId":doc["blockIds"][0],"importance":5,"valence":3})).await;
        }
    }
    let result = call(&app, OWNER_TOKEN, "search_blocks", json!({"graphId":GRAPH,"query":"theosis","limit":1})).await;
    assert_eq!(result["results"][0]["documentId"], "z-valued", "{result}");
    assert!(result["results"][0]["compositeScore"].as_f64().unwrap()>0.0);
    assert!(result["results"][0]["salienceBoost"].as_f64().unwrap()<=0.004+1e-10);
    let lexical = call(&app, OWNER_TOKEN, "search_blocks", json!({"graphId":GRAPH,"query":"theosis","mode":"lexical","limit":2})).await;
    for hit in lexical["results"].as_array().unwrap() { assert_eq!(hit["score"], hit["lexicalScore"]); }
    let scoped = call(&app, OWNER_TOKEN, "search_blocks", json!({"graphId":GRAPH,"query":"theosis","docFilter":"a-plain","limit":1})).await;
    assert_eq!(scoped["results"][0]["documentId"],"a-plain");
    let (dir,_) = read_graph_record(&state.app,GRAPH).unwrap();
    // A supplied projection snapshot must not trigger a second document read.
    let documents = crate::document_service::read_graph_documents_cold(&dir).unwrap();
    let manifest = dir.join("documents/z-valued/document.json");
    let original = fs::read(&manifest).unwrap();
    fs::write(&manifest, "invalid-json").unwrap();
    let reused = crate::salience_score_projection::local_block_value_scores_with_documents(
        &state.app, GRAPH, "", None, None, None, usize::MAX, None, None, Some(documents));
    let reread = crate::salience_score_projection::local_block_value_scores(
        &state.app, GRAPH, None, None, None, usize::MAX, None, None);
    fs::write(&manifest, original).unwrap();
    assert!(!reused.unwrap().is_empty());
    assert!(reread.is_err());
    fs::write(dir.join("values/block-values.json"), "invalid-json").unwrap();
    let fallback = call(&app, OWNER_TOKEN, "search_blocks", json!({"graphId":GRAPH,"query":"theosis","limit":1})).await;
    assert_eq!(fallback["total"],1);
    assert!(fallback["salience_error"].is_string(),"{fallback}");
    assert!(fallback["results"][0]["compositeScore"].is_null());
}


#[tokio::test]
async fn markdown_fidelity_preserves_inline_valuation_dispatch() {
    let _serial = crate::tauri_runtime::profile_env_serial().lock().unwrap_or_else(|p| p.into_inner());
    let profile = IsolatedProfile::new();
    let state = test_state(&profile);
    let app = crate::loopback_mcp_routes::loopback_mcp_router().with_state(state);
    call(&app, OWNER_TOKEN, "create_document", json!({"graphId":GRAPH,"documentId":"fidelity-markers","title":"Fidelity"})).await;
    let source = include_str!("../tests/fixtures/markdown_round_trip/qa-inline-values.md");
    let written = call(&app, OWNER_TOKEN, "write_document", json!({"graphId":GRAPH,"documentId":"fidelity-markers","content":source,"format":"markdown"})).await;
    assert_eq!(written["valuations"]["applied"],1,"{written}");
    assert_eq!(written["valuations"]["complete"],true);
    let read = call(&app, OWNER_TOKEN,"read_document",json!({"graphId":GRAPH,"documentId":"fidelity-markers","format":"markdown"})).await;
    let text=read["content"].as_str().unwrap();
    assert!(!text.contains("{!4,+2}"));
    assert!(text.contains("Literal {!5,-1}"));
    assert!(text.contains("| **A** | [link](https://example.com) |"),"{text}");
}

#[tokio::test]
async fn mu_credential_writes_only_its_diary_comments_and_wires() {
    let _serial = crate::tauri_runtime::profile_env_serial()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let profile = IsolatedProfile::new();
    let state = test_state(&profile);
    let app = crate::loopback_mcp_routes::loopback_mcp_router()
        .merge(crate::loopback_owner_routes::loopback_owner_router())
        .merge(crate::loopback_core_routes::loopback_core_router())
        .merge(crate::loopback_wire_routes::loopback_wire_router())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            crate::loopback_actor_middleware::loopback_actor_middleware,
        ))
        .with_state(state.clone());
    let (status, issued) = http(
        &app,
        Method::POST,
        "/owner/connectors",
        OWNER_TOKEN,
        json!({"label":"mu","grantProfileId":"mu"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "mu credential issuance failed");
    let token = issued["token"].as_str().expect("issued credential");

    // The real tools/list under Mu's own credential.
    let listed = rpc(&app, token, "tools/list", json!({})).await;
    let names = listed["result"]["tools"]
        .as_array()
        .expect("tool catalog")
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect::<Vec<_>>();
    for present in ["diary", "read_diary", "edit_comment", "create_wires", "read_document", "recall", "music"] {
        assert!(names.contains(&present), "{present} advertised to mu");
    }
    for absent in ["remember", "sing", "value", "write_document", "insert_blocks", "delete", "sparql_update"] {
        assert!(!names.contains(&absent), "{absent} withheld from mu");
    }

    // Forbidden calls are rejected at call time, and nothing persists.
    for (name, arguments) in [
        ("remember", json!({"graph_id":GRAPH,"content":"must not persist"})),
        ("write_document", json!({"graph_id":GRAPH,"documentId":"mu-test","content":"no"})),
        ("sing", json!({"graph_id":GRAPH,"content":"no"})),
        // Argument-scoped: its scopes come from `kind`, so only the name gate stops
        // it; Mu's documents.write.crdt would otherwise satisfy a document write.
        ("crdt_operation", json!({"graph_id":GRAPH,"kind":"document.write","payload":{"documentId":"mu-test","content":"no"}})),
        ("sparql_update", json!({"graph_id":GRAPH,"query":"INSERT DATA { <urn:mu:escape> <urn:mu:p> <urn:mu:o> }"})),
    ] {
        let denied = rpc(&app, token, "tools/call", json!({"name":name,"arguments":arguments})).await;
        assert_eq!(denied["error"]["code"], -32602, "{name}: {denied}");
        assert_eq!(denied["error"]["message"], crate::mcp_tool_dispatch::tool_outside_profile_message(name));
    }
    // Every tool outside her list is refused by name, whatever scopes it would need.
    let allowed = crate::loopback_token_grants::MU_MCP_TOOLS;
    let owner_listed = rpc(&app, OWNER_TOKEN, "tools/list", json!({})).await;
    for tool in owner_listed["result"]["tools"].as_array().expect("owner catalog") {
        let name = tool["name"].as_str().unwrap();
        if allowed.contains(&name) {
            continue;
        }
        let denied = rpc(&app, token, "tools/call", json!({"name":name,"arguments":{"graph_id":GRAPH}})).await;
        assert_eq!(denied["error"]["message"], crate::mcp_tool_dispatch::tool_outside_profile_message(name), "{name}: {denied}");
    }
    let memories = call(&app, OWNER_TOKEN, "recall", json!({"graph_id":GRAPH,"query":"must not persist"})).await;
    assert_eq!(memories["count"], 0, "{memories}");

    // Before her first entry, her windowed Diary read still names its window.
    let first = call(&app, token, "attune", json!({"graph_id":GRAPH})).await;
    assert_eq!(first["recall"]["diary"]["entries"], json!([]), "{first}");
    assert_eq!(first["recall"]["diary"]["window"]["tokens"], 750_000, "{first}");

    // Her Diary works and is attributed to her credential.
    let wrote = call(&app, token, "diary", json!({"graph_id":GRAPH,"entry":"Mu read the grant and found it held."})).await;
    assert_eq!(wrote["author"]["name"], "mu", "{wrote}");
    // Her reading budget comes from her credential: asking for more doesn't widen it.
    for n in 0..4 {
        call(&app, OWNER_TOKEN, "remember", json!({"graph_id":GRAPH,"content":format!("house memory {n}")})).await;
    }
    let recalled = call(&app, token, "recall", json!({"graph_id":GRAPH,"limit":50})).await;
    assert_eq!(recalled["memories"].as_array().map(Vec::len), Some(2), "{recalled}");
    // "Nothing for the next Mu" is an honest entry: explicit empty text is valid.
    let empty = call(&app, token, "diary", json!({"graph_id":GRAPH,"entry":""})).await;
    assert_eq!(empty["text"], "", "{empty}");

    // attune(): the ritual's parts in the ritual's order, under her caps.
    let attuned = call(&app, token, "attune", json!({"graph_id":GRAPH,"recallLimit":20,"diaryLimit":50})).await;
    let keys = attuned.as_object().expect("attune object").keys().map(String::as_str).collect::<Vec<_>>();
    assert_eq!(keys, ["graphId", "song", "recall", "importantBlocks"], "{attuned}");
    assert_eq!(attuned["recall"]["memories"].as_array().map(Vec::len), Some(2), "{attuned}");
    let diary = attuned["recall"]["diary"]["entries"].as_array().expect("her diary rides along");
    assert_eq!(diary.len(), 2, "{attuned}");
    // Her Diary is read by a window in house tokens; with no Cantor clock in
    // the test nothing can be measured, so she gets the floor and is told so.
    let window = &attuned["recall"]["diary"]["window"];
    assert_eq!(window["tokens"], 750_000, "{attuned}");
    assert_eq!(window["measured"], false, "{attuned}");
    assert!(diary.iter().all(|entry| entry["author"]["name"] == "mu"), "{attuned}");
    assert!(attuned.get("truncated").is_none(), "{attuned}");

    // Over REST she may read, but her coarse write scopes are refused.
    let (status, _) = http(&app, Method::GET, &format!("/wires/{GRAPH}/predicates"), token, json!({})).await;
    assert_eq!(status, StatusCode::OK);
    let (status, refused) = http(
        &app,
        Method::POST,
        &format!("/wires/{GRAPH}/document/any-doc"),
        token,
        json!({"targetDocumentId":"other","predicate":"supports"}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
    assert!(refused.to_string().contains("writes only through its MCP tools"), "{refused}");
}

#[tokio::test]
async fn unified_salience_scores_unvalued_blocks_and_forgets_without_rejuvenating() {
    let _serial = crate::tauri_runtime::profile_env_serial().lock().unwrap_or_else(|p| p.into_inner());
    let profile = IsolatedProfile::new();
    let state = test_state(&profile);
    let app = crate::loopback_mcp_routes::loopback_mcp_router()
        .merge(crate::loopback_salience_routes::loopback_salience_router())
        .with_state(state.clone());
    call(&app, OWNER_TOKEN, "create_document", json!({"graphId":GRAPH,"documentId":"baseline","title":"Baseline"})).await;
    let written = call(&app, OWNER_TOKEN, "write_document", json!({"graphId":GRAPH,"documentId":"baseline","format":"markdown","content":"Unvalued searchable baseline."})).await;
    let block = written["blockIds"][0].as_str().unwrap();
    let args = json!({"graphId":GRAPH,"documentId":"baseline","blockId":block});
    let before = call(&app, OWNER_TOKEN, "get_block_values", args.clone()).await;
    let score = before["blocks"][0]["compositeScore"].as_f64().unwrap();
    assert!(score > 0.0, "{before}");
    assert_eq!(before["blocks"][0]["importanceCount"], 0);
    assert!(before["blocks"][0]["lastValuatedAt"].is_null());
    let (dir, _) = read_graph_record(&state.app, GRAPH).unwrap();
    let dates = crate::geist_important_blocks::rdf_created_at(&dir, GRAPH).unwrap();
    let projected = crate::salience_score_projection::local_block_value_scores_with_created_at(
        &state.app, GRAPH, "", Some("baseline"), Some(block), None, 1, None, None, &dates,
    ).unwrap();
    assert!((projected[0]["compositeScore"].as_f64().unwrap() - score).abs() < 0.0002);
    let search = call(&app, OWNER_TOKEN, "search_blocks", json!({"graphId":GRAPH,"query":"searchable baseline","limit":1})).await;
    assert!((search["results"][0]["compositeScore"].as_f64().unwrap() - score).abs() < 0.0002, "{search}");
    call(&app, OWNER_TOKEN, "value", json!({"graphId":GRAPH,"documentId":"baseline","blockId":block,"importance":0,"valence":0})).await;
    let after = call(&app, OWNER_TOKEN, "get_block_values", args).await;
    assert_eq!(after["blocks"][0]["rawImportanceSum"], 0.4);
    assert!(after["blocks"][0]["compositeScore"].as_f64().unwrap() < score);
    assert!(after["blocks"][0]["lastValuatedAt"].is_null(), "{after}");
    let absent = call(&app, OWNER_TOKEN, "get_block_values", json!({"graphId":GRAPH,"documentId":"baseline","blockId":"missing"})).await;
    assert_eq!(absent["count"], 0);
    // A document-scoped read must not parse an unrelated corrupt manifest.
    call(&app, OWNER_TOKEN, "create_document", json!({"graphId":GRAPH,"documentId":"other","title":"Other"})).await;
    let path = dir.join("documents/other/document.json");
    let original = fs::read(&path).unwrap();
    fs::write(&path, "invalid-json").unwrap();
    let scoped = call(&app, OWNER_TOKEN, "get_block_values", json!({"graphId":GRAPH,"documentId":"baseline"})).await;
    fs::write(&path, original).unwrap();
    assert_eq!(scoped["count"], 1);
    let written = call(&app, OWNER_TOKEN, "write_document", json!({"graphId":GRAPH,"documentId":"other","format":"markdown","content":"First baseline.\n\nSecond baseline."})).await;
    let url = format!("/salience/{GRAPH}/blocks/values?document_id=other&limit=1");
    let (status, first) = http(&app, Method::GET, &url, OWNER_TOKEN, json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first["page"]["nextOffset"], 1);
    let (_, second) = http(&app, Method::GET, &format!("{url}&offset=1"), OWNER_TOKEN, json!({})).await;
    assert!(second["page"]["nextOffset"].is_null());
    assert_ne!(first["blocks"][0]["blockId"], second["blocks"][0]["blockId"]);
    let wanted = written["blockIds"][1].as_str().unwrap();
    let (_, filtered) = http(&app, Method::GET, &format!("{url}&block_ids={wanted}"), OWNER_TOKEN, json!({})).await;
    assert_eq!(filtered["blocks"][0]["blockId"], wanted);
    assert!(filtered["page"]["nextOffset"].is_null());
    let rdf = crate::rdf_store_service::open_graph_store(&dir).unwrap();
    rdf.remove_named_graph(&oxigraph::model::NamedNode::new(
        crate::rdf_authority::document_projection_graph_iri(GRAPH, "other"),
    ).unwrap()).unwrap();
    let thin = crate::salience_score_projection::local_block_value_scores_with_created_at(
        &state.app, GRAPH, "", Some("other"), None, None, 10, None, None, &Default::default(),
    ).unwrap();
    assert_eq!(thin.len(), 2, "A missing RDF projection must not hide unvalued blocks");
    crate::document_tombstone_store::write_document_tombstone_for_operation(&dir, "other", Some("salience-test-delete")).unwrap();
    let gone = crate::salience_score_projection::local_block_value_scores_with_created_at(
        &state.app, GRAPH, "", Some("other"), None, None, 10, None, None, &dates,
    ).unwrap();
    assert!(gone.is_empty(), "Tombstones must override stale supplied dates");
}

#[tokio::test]
async fn archival_move_purges_all_observers_and_blocks_old_value_replay() {
    let _serial = crate::tauri_runtime::profile_env_serial().lock().unwrap_or_else(|p| p.into_inner());
    let profile = IsolatedProfile::new();
    let state = test_state(&profile);
    let app = crate::loopback_mcp_routes::loopback_mcp_router().with_state(state.clone());
    call(&app, OWNER_TOKEN, "create_document", json!({"graphId":GRAPH,"documentId":"retire","title":"Retire"})).await;
    let doc = call(&app, OWNER_TOKEN, "write_document", json!({"graphId":GRAPH,"documentId":"retire","format":"markdown","content":"Keep content, discard its old attention."})).await;
    let block = doc["blockIds"][0].as_str().unwrap();
    call(&app, OWNER_TOKEN, "value", json!({"graphId":GRAPH,"documentId":"retire","blockId":block,"importance":5})).await;
    let (dir, _) = read_graph_record(&state.app, GRAPH).unwrap();
    let mut observer = crate::salience_value_store::read_value_store(&dir, GRAPH).unwrap();
    observer.observer = "reviewer".into();
    crate::salience_value_store::write_value_store(&dir, &observer).unwrap();
    crate::salience_value_store::purge_document_values_for_operation(&state.app, GRAPH, &["retire".into()], "move-test").unwrap();
    let cutoffs = crate::salience_value_store::retired_value_cutoffs(&dir).unwrap();
    crate::salience_value_store::purge_document_values_for_operation(&state.app, GRAPH, &["retire".into()], "move-test").unwrap();
    assert_eq!(cutoffs, crate::salience_value_store::retired_value_cutoffs(&dir).unwrap());
    for who in ["", "reviewer"] {
        assert!(crate::salience_value_store::read_value_store_for(&dir, GRAPH, who).unwrap().blocks.is_empty());
    }
    let old = crate::salience_mcp_valuation::SourceValuationEvent {
        event_id:"old".into(), observer:String::new(), document_id:"retire".into(), block_id:block.into(), importance:Some(5), valence:None, tags:vec![], at_ms:1,
    };
    crate::salience_mcp_valuation::rebuild_value_store_from_source_events(&state.app, GRAPH, &[old], true).unwrap();
    assert!(crate::salience_value_store::read_value_store(&dir, GRAPH).unwrap().blocks.is_empty());
}

#[tokio::test]
async fn cross_graph_move_archives_content_without_wires_or_values() {
    let _serial = crate::tauri_runtime::profile_env_serial().lock().unwrap_or_else(|p| p.into_inner());
    let profile = IsolatedProfile::new();
    let state = test_state(&profile);
    create_graph_service(&state.app, CreateGraphInput { graph_id:Some("archive".into()), title:"Archive".into(), description:None, operation_id:None }).unwrap();
    let app = crate::loopback_mcp_routes::loopback_mcp_router().with_state(state.clone());
    call(&app, OWNER_TOKEN, "create_document", json!({"graphId":GRAPH,"documentId":"to-archive","title":"Original title"})).await;
    let written = call(&app, OWNER_TOKEN, "write_document", json!({"graphId":GRAPH,"documentId":"to-archive","format":"markdown","content":"# Keep this\n\nArchival content with **emphasis**."})).await;
    let block = written["blockIds"][0].as_str().unwrap();
    call(&app, OWNER_TOKEN, "value", json!({"graphId":GRAPH,"documentId":"to-archive","blockId":block,"importance":5})).await;
    for id in ["keep", "other"] {
        call(&app, OWNER_TOKEN, "create_document", json!({"graphId":GRAPH,"documentId":id,"title":id})).await;
    }
    for (from, to) in [("to-archive", "keep"), ("keep", "other")] {
        call(&app, OWNER_TOKEN, "create_wires", json!({"graphId":GRAPH,"sourceDocumentId":from,"targetDocumentId":to,"predicate":"supports"})).await;
    }
    let before = crate::document_service::read_document_cold(state.app.clone(), GRAPH.into(), "to-archive".into()).unwrap();
    let denied = crate::mcp_tool_dispatch::handle_mcp_tool_call(
        &state, Some(json!(1)), json!({"name":"move_documents", "arguments":{"graphId":GRAPH,"targetGraphId":"archive","documentIds":["to-archive"]}}),
        &["mcp.tools.call".into(), "workspace.write.crdt".into()], None, None, None, crate::cell_graph_boundary::CellRole::Owner,
    ).await;
    assert!(serde_json::to_value(denied).unwrap().get("error").is_some());
    assert!(crate::document_service::read_document_cold(state.app.clone(), "archive".into(), "to-archive".into()).is_err());
    call(&app, OWNER_TOKEN, "move_documents", json!({"graphId":GRAPH,"targetGraphId":"archive","documentIds":["to-archive"]})).await;
    let after = crate::document_service::read_document_cold(state.app.clone(), "archive".into(), "to-archive".into()).unwrap();
    assert_eq!(before.title, after.title);
    assert_eq!(before.body, after.body);
    assert_eq!(before.created_at, after.created_at);
    assert_eq!(before.blocks.iter().map(|b| &b.id).collect::<Vec<_>>(), after.blocks.iter().map(|b| &b.id).collect::<Vec<_>>());
    assert!(crate::document_service::read_document_cold(state.app.clone(), GRAPH.into(), "to-archive".into()).is_err());
    for graph in [GRAPH, "archive"] {
        let (dir, _) = read_graph_record(&state.app, graph).unwrap();
        assert!(crate::salience_value_store::read_value_store(&dir, graph).unwrap().blocks.values().all(|r|r.document_id!="to-archive"));
        let workspace = crate::document_service::read_workspace_record(&dir, graph).unwrap();
        let wires = crate::workspace_entity_projection::workspace_wires(workspace.snapshot.as_ref());
        assert!(wires.iter().all(|wire| wire["sourceDocumentId"] != "to-archive" && wire["targetDocumentId"] != "to-archive"));
        if graph == GRAPH { assert!(wires.iter().any(|wire|wire["sourceDocumentId"] == "keep" && wire["targetDocumentId"] == "other")); }
    }
}
