//! `mycl --mcp-stdio`: the stdio MCP bridge inside the app's own binary.
//!
//! A port of scripts/garden-mcp-stdio.mjs, so a stranger's agent config names
//! the app and nothing else: no Node in the bundle, no script in a checkout
//! (release plan, 2026-10-23). Lines of JSON-RPC arrive on stdin, each is
//! forwarded to the running app's loopback MCP endpoint with the named agent
//! credential from `GARDEN_MCP_TOKEN`, and the answers go out on stdout as
//! they finish, not in arrival order (JSON-RPC matches by id; one stuck write
//! must not hold the reads behind it, 2026-10-16).
//!
//! It starts no window, takes no profile lock, opens no store and hosts no
//! model: the running app is the graph's owner, and this process is only a
//! pipe to it. Diagnostics go to stderr; stdout carries the protocol alone.
//!
//! Environment, as the script reads it: `GARDEN_MCP_TOKEN` (required),
//! `GARDEN_MCP_URL` (skips the manifest), `GARDEN_LOOPBACK_MANIFEST`,
//! `GARDEN_PROFILE_DIR`, `GARDEN_MCP_TIMEOUT_MS` (default 290 s),
//! `GARDEN_MCP_DROP_STRUCTURED=1` (for clients that render structured content
//! twice, such as Kimi Code).

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The bundle identifier, which names the profile directory. The public cut
/// changes it (Epsilon's packaging run); a test holds it to tauri.conf.json.
pub(crate) const DEFAULT_APP_ID: &str = "dev.sophia.garden";
const FLAG: &str = "--mcp-stdio";
const DEFAULT_TIMEOUT_MS: u64 = 290_000;
const RESPONSE_VIEW_META: &str = "garden/responseView";
const AGENT_VIEW: &str = "agent-v1";
const MAX_IN_FLIGHT: usize = 64;

/// Runs the bridge and returns true when the process was started for it;
/// the caller then exits without building the app.
pub fn run_if_requested() -> bool {
    if !std::env::args().skip(1).any(|arg| arg == FLAG) {
        return false;
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("mycl --mcp-stdio: cannot start the runtime: {error}");
            std::process::exit(1);
        }
    };
    let code = runtime.block_on(async {
        let ctx = Context::from_process();
        let stdin = tokio::io::stdin();
        let reader = tokio::io::BufReader::new(stdin);
        match serve(reader, ctx).await {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("mycl --mcp-stdio: {error}");
                1
            }
        }
    });
    std::process::exit(code);
}

// ---------------------------------------------------------------- environment

/// What the bridge reads from the environment, captured once so tests can
/// supply their own.
#[derive(Clone, Debug, Default)]
pub(crate) struct Env {
    pub token: Option<String>,
    pub mcp_url: Option<String>,
    pub manifest: Option<String>,
    pub profile_dir: Option<String>,
    pub timeout_ms: Option<String>,
    pub drop_structured: bool,
    pub home: Option<String>,
    pub appdata: Option<String>,
    pub xdg_data_home: Option<String>,
}

impl Env {
    fn from_process() -> Self {
        let var = |name: &str| std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        Env {
            token: var("GARDEN_MCP_TOKEN"),
            mcp_url: var("GARDEN_MCP_URL"),
            manifest: var("GARDEN_LOOPBACK_MANIFEST"),
            profile_dir: var("GARDEN_PROFILE_DIR"),
            timeout_ms: var("GARDEN_MCP_TIMEOUT_MS"),
            drop_structured: var("GARDEN_MCP_DROP_STRUCTURED").as_deref() == Some("1"),
            home: home_dir(),
            appdata: var("APPDATA"),
            xdg_data_home: var("XDG_DATA_HOME"),
        }
    }

    fn timeout(&self) -> Duration {
        let ms = self.timeout_ms.as_deref().and_then(|v| v.parse::<u64>().ok()).filter(|ms| *ms > 0);
        Duration::from_millis(ms.unwrap_or(DEFAULT_TIMEOUT_MS))
    }
}

/// The home directory as Node's os.homedir() finds it: USERPROFILE first on
/// Windows, HOME elsewhere, and the account database when neither is set
/// (some sandboxed agent hosts clear HOME).
fn home_dir() -> Option<String> {
    let var = |name: &str| std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let from_env = if cfg!(windows) { var("USERPROFILE").or_else(|| var("HOME")) } else { var("HOME") };
    #[allow(deprecated)]
    from_env.or_else(|| std::env::home_dir().map(|p| p.display().to_string()))
}

impl Env {
}

/// Where the running app's loopback manifest is, by platform convention,
/// unless the environment names it.
pub(crate) fn default_manifest_path(platform: &str, env: &Env) -> PathBuf {
    if let Some(manifest) = &env.manifest {
        return PathBuf::from(manifest);
    }
    if let Some(profile) = &env.profile_dir {
        return Path::new(profile).join("loopback.json");
    }
    let home = PathBuf::from(env.home.clone().unwrap_or_default());
    let data = match platform {
        "macos" => home.join("Library").join("Application Support"),
        "windows" => env.appdata.clone().map(PathBuf::from).unwrap_or_else(|| home.join("AppData").join("Roaming")),
        _ => env.xdg_data_home.clone().map(PathBuf::from).unwrap_or_else(|| home.join(".local").join("share")),
    };
    data.join(DEFAULT_APP_ID).join("profiles").join("default").join("loopback.json")
}

fn platform() -> &'static str {
    std::env::consts::OS
}

/// The MCP URL: from the environment, else `mcpUrl` out of the manifest and
/// nothing else from it (the manifest also holds the owner token, which this
/// process never presents).
pub(crate) fn resolve_mcp_url(env: &Env, read: &dyn Fn(&Path) -> Result<String, String>) -> Result<String, String> {
    if let Some(url) = env.mcp_url.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
        return Ok(url.to_string());
    }
    let path = default_manifest_path(platform(), env);
    let text = read(&path).map_err(|error| {
        format!("Mycl is not running or its manifest is unavailable at {}: {error}", path.display())
    })?;
    let manifest: Value = serde_json::from_str(&text).map_err(|error| {
        format!("Mycl is not running or its manifest is unavailable at {}: {error}", path.display())
    })?;
    match manifest.get("mcpUrl").and_then(Value::as_str).map(str::trim) {
        Some(url) if !url.is_empty() => Ok(url.to_string()),
        _ => Err(format!("Mycl manifest at {} does not contain mcpUrl", path.display())),
    }
}

// ---------------------------------------------------------------- transport

/// One HTTP answer, enough to decode either a JSON body or an event stream.
#[derive(Clone, Debug)]
pub(crate) struct FetchResponse {
    pub status: u16,
    pub content_type: Option<String>,
    pub body: String,
}

/// The POST to Garden, abstracted so tests never open a socket.
pub(crate) trait Fetch: Send + Sync {
    fn post(
        &self,
        url: &str,
        token: &str,
        body: String,
        timeout: Duration,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<FetchResponse, String>> + Send + '_>>;
}

struct ReqwestFetch(reqwest::Client);

/// A timeout, a refused connection and a proxy failure must read
/// differently; reqwest's Display hides the cause behind one sentence.
fn describe_error(error: reqwest::Error) -> String {
    let kind = if error.is_timeout() {
        "timed out"
    } else if error.is_connect() {
        "could not connect"
    } else {
        "request failed"
    };
    let mut text = format!("{kind}: {error}");
    let mut source = std::error::Error::source(&error);
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

impl Fetch for ReqwestFetch {
    fn post(
        &self,
        url: &str,
        token: &str,
        body: String,
        timeout: Duration,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<FetchResponse, String>> + Send + '_>> {
        let request = self
            .0
            .post(url)
            .timeout(timeout)
            .header("accept", "application/json, text/event-stream")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(body);
        Box::pin(async move {
            let response = request.send().await.map_err(describe_error)?;
            let status = response.status().as_u16();
            let content_type = response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let body = response.text().await.map_err(describe_error)?;
            Ok(FetchResponse { status, content_type, body })
        })
    }
}

/// Everything a request needs, shared by every line.
pub(crate) struct Context {
    pub env: Env,
    pub fetch: Box<dyn Fetch>,
    pub read: Box<dyn Fn(&Path) -> Result<String, String> + Send + Sync>,
}

impl Context {
    fn from_process() -> Self {
        Context {
            env: Env::from_process(),
            // The endpoint is loopback: a proxy from the shell's environment
            // (HTTP_PROXY, ALL_PROXY) must never see the credential or the
            // call (angel review, 2026-10-23).
            fetch: Box::new(ReqwestFetch(
                reqwest::Client::builder().no_proxy().build().expect("an HTTP client with no proxy"),
            )),
            read: Box::new(|path| std::fs::read_to_string(path).map_err(|error| error.to_string())),
        }
    }
}

// ---------------------------------------------------------------- the protocol

/// Presentation is Garden's, not the transport's: a tool call that names no
/// view gets the agent view; a call that names one is left alone, as is
/// anything malformed, which Garden answers for itself.
pub(crate) fn with_agent_view(request: &Value) -> Value {
    if request.get("method").and_then(Value::as_str) != Some("tools/call") {
        return request.clone();
    }
    let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
    if params.is_null() {
        return request.clone();
    }
    let Some(params_map) = params.as_object() else {
        return request.clone();
    };
    let meta = params_map.get("_meta");
    if meta.is_some_and(Value::is_null) {
        return request.clone();
    }
    let meta_map = match meta {
        None => serde_json::Map::new(),
        Some(Value::Object(map)) => map.clone(),
        Some(_) => return request.clone(),
    };
    if meta_map.contains_key(RESPONSE_VIEW_META) {
        return request.clone();
    }
    let mut meta_map = meta_map;
    meta_map.insert(RESPONSE_VIEW_META.into(), json!(AGENT_VIEW));
    let mut params_map = params_map.clone();
    params_map.insert("_meta".into(), Value::Object(meta_map));
    let mut out = request.clone();
    out["params"] = Value::Object(params_map);
    out
}

/// Some clients (Kimi Code) render `content` and append `structuredContent`
/// verbatim, doubling every structured response. With the opt-in, the
/// structured copy goes, and `outputSchema` goes from the tool list, since a
/// tool that declares one promises structured content. Never the only copy.
pub(crate) fn without_structured_content(request: &Value, response: Value, env: &Env) -> Value {
    if !env.drop_structured {
        return response;
    }
    let Some(result) = response.get("result").and_then(Value::as_object) else {
        return response;
    };
    match request.get("method").and_then(Value::as_str) {
        Some("tools/call") if result.contains_key("structuredContent") => {
            let has_text = result
                .get("content")
                .and_then(Value::as_array)
                .is_some_and(|parts| parts.iter().any(|p| p.get("type").and_then(Value::as_str) == Some("text")));
            if !has_text {
                return response;
            }
            let mut rest = result.clone();
            rest.remove("structuredContent");
            let mut out = response.clone();
            out["result"] = Value::Object(rest);
            out
        }
        Some("tools/list") => {
            let Some(tools) = result.get("tools").and_then(Value::as_array) else {
                return response;
            };
            let stripped: Vec<Value> = tools
                .iter()
                .map(|tool| match tool.as_object() {
                    Some(map) => {
                        let mut map = map.clone();
                        map.remove("outputSchema");
                        Value::Object(map)
                    }
                    None => tool.clone(),
                })
                .collect();
            let mut rest = result.clone();
            rest.insert("tools".into(), Value::Array(stripped));
            let mut out = response.clone();
            out["result"] = Value::Object(rest);
            out
        }
        _ => response,
    }
}

fn rpc_error(id: Value, message: String) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32000, "message": message } })
}

/// Forward one request and decode the answer, from JSON or from a finite
/// event stream (the message whose id matches).
pub(crate) async fn forward_rpc(request: &Value, ctx: &Context) -> Result<Value, String> {
    let token = ctx
        .env
        .token
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or("GARDEN_MCP_TOKEN must contain a named Mycl agent credential")?;
    let url = resolve_mcp_url(&ctx.env, ctx.read.as_ref())?;
    let body = serde_json::to_string(&with_agent_view(request)).map_err(|e| e.to_string())?;
    let response = ctx.fetch.post(&url, token, body, ctx.env.timeout()).await?;
    if !(200..300).contains(&response.status) {
        let detail = if response.body.is_empty() { String::new() } else { format!(": {}", response.body) };
        return Err(format!("Mycl MCP returned HTTP {}{detail}", response.status));
    }
    let is_stream = response.content_type.as_deref().is_some_and(|ct| ct.contains("text/event-stream"));
    if is_stream {
        let wanted = request.get("id").cloned().unwrap_or(Value::Null);
        for line in response.body.lines() {
            let Some(data) = line.strip_prefix("data:") else { continue };
            let data = data.trim();
            if data.is_empty() || data == "[DONE]" {
                continue;
            }
            let message: Value = serde_json::from_str(data).map_err(|e| e.to_string())?;
            if message.get("id") == Some(&wanted) {
                return Ok(message);
            }
        }
        return Err("Mycl MCP event stream ended without a matching response".into());
    }
    serde_json::from_str(&response.body).map_err(|e| e.to_string())
}

/// One request: notifications (no id) are consumed here, since Garden's
/// endpoint is stateless and they carry no work; anything else is forwarded,
/// and a failure becomes a JSON-RPC error with the request's id.
async fn handle_request(request: &Value, ctx: &Context) -> Option<Value> {
    let Some(map) = request.as_object() else {
        return Some(rpc_error(Value::Null, "Invalid JSON-RPC request".into()));
    };
    let id = map.get("id").cloned().unwrap_or(Value::Null);
    if id.is_null() {
        return None;
    }
    Some(match forward_rpc(request, ctx).await {
        Ok(response) => without_structured_content(request, response, &ctx.env),
        Err(error) => rpc_error(id, error),
    })
}

/// One stdin line: a request or a batch. Batch items are forwarded one at a
/// time (the loopback endpoint takes single messages) and answered together,
/// in order, without the notifications.
pub(crate) async fn handle_line(line: &str, ctx: &Context) -> Option<Value> {
    if line.trim().is_empty() {
        return None;
    }
    let request: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(error) => return Some(rpc_error(Value::Null, format!("Invalid JSON from MCP client: {error}"))),
    };
    let Some(items) = request.as_array() else {
        return handle_request(&request, ctx).await;
    };
    if items.is_empty() {
        return Some(rpc_error(Value::Null, "Invalid empty JSON-RPC batch".into()));
    }
    let answers = futures_util::future::join_all(items.iter().map(|item| handle_request(item, ctx))).await;
    let answers: Vec<Value> = answers.into_iter().flatten().collect();
    if answers.is_empty() {
        None
    } else {
        Some(Value::Array(answers))
    }
}

/// Read lines until stdin closes; answer each as it finishes. Writes are
/// serialized through one lock so two answers never interleave bytes.
pub(crate) async fn serve<R>(reader: R, ctx: Context) -> Result<(), String>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    use tokio::io::AsyncBufReadExt;
    use tokio::io::AsyncWriteExt;
    let ctx = std::sync::Arc::new(ctx);
    let out = std::sync::Arc::new(tokio::sync::Mutex::new(tokio::io::stdout()));
    // Bounded: a burst of calls must not exhaust the file-descriptor limit
    // (macOS launchd's default is 256).
    let permits = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_IN_FLIGHT));
    let mut reader = reader;
    let mut in_flight = tokio::task::JoinSet::new();
    let mut buf = Vec::new();
    let read_error = loop {
        buf.clear();
        let read = match reader.read_until(b'\n', &mut buf).await {
            Ok(read) => read,
            Err(error) => break Some(error.to_string()),
        };
        if read == 0 {
            break None;
        }
        // Lossy, as Node's readline is: one bad byte must not end the stream.
        let line = String::from_utf8_lossy(&buf).trim_end_matches(['\r', '\n']).to_string();
        let (ctx, out, permits) = (ctx.clone(), out.clone(), permits.clone());
        in_flight.spawn(async move {
            let _permit = permits.acquire_owned().await;
            if let Some(response) = handle_line(&line, &ctx).await {
                let mut text = serde_json::to_string(&response).unwrap_or_default();
                text.push('\n');
                let mut out = out.lock().await;
                if let Err(error) = out.write_all(text.as_bytes()).await {
                    eprintln!("mycl --mcp-stdio: {error}");
                }
                let _ = out.flush().await;
            }
        });
    };
    // Calls in flight are answered before the process ends, whatever ended
    // the reading.
    while let Some(joined) = in_flight.join_next().await {
        if let Err(error) = joined {
            eprintln!("mycl --mcp-stdio: {error}");
        }
    }
    match read_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A fetch that records what it was asked and answers from a closure.
    struct MockFetch {
        seen: Arc<Mutex<Vec<(String, String, String)>>>,
        answer: Box<dyn Fn(&Value) -> FetchResponse + Send + Sync>,
    }

    impl Fetch for MockFetch {
        fn post(
            &self,
            url: &str,
            token: &str,
            body: String,
            _timeout: Duration,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<FetchResponse, String>> + Send + '_>> {
            self.seen.lock().unwrap().push((url.to_string(), token.to_string(), body.clone()));
            let request: Value = serde_json::from_str(&body).unwrap();
            let response = (self.answer)(&request);
            Box::pin(async move { Ok(response) })
        }
    }

    fn json_response(body: Value) -> FetchResponse {
        FetchResponse { status: 200, content_type: Some("application/json".into()), body: body.to_string() }
    }

    fn ctx(env: Env, answer: impl Fn(&Value) -> FetchResponse + Send + Sync + 'static) -> (Context, Arc<Mutex<Vec<(String, String, String)>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let ctx = Context {
            env,
            fetch: Box::new(MockFetch { seen: seen.clone(), answer: Box::new(answer) }),
            read: Box::new(|_| Ok(json!({ "mcpUrl": "http://127.0.0.1:4123/mcp", "token": "owner-secret" }).to_string())),
        };
        (ctx, seen)
    }

    fn named_env() -> Env {
        Env { token: Some("named-agent-token".into()), manifest: Some("/tmp/loopback.json".into()), ..Default::default() }
    }

    #[test]
    fn the_app_id_matches_the_bundle_identifier() {
        let conf: Value = serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        assert_eq!(conf["identifier"], DEFAULT_APP_ID);
    }

    #[test]
    fn agent_presentation_is_transport_metadata_never_a_tool_argument() {
        let request = json!({ "id": 1, "method": "tools/call", "params": { "name": "recall", "arguments": { "limit": 8 }, "_meta": { "trace": "keep" } } });
        let result = with_agent_view(&request);
        assert_eq!(result["params"]["_meta"], json!({ "trace": "keep", "garden/responseView": "agent-v1" }));
        assert_eq!(result["params"]["arguments"], json!({ "limit": 8 }));
        let machine = json!({ "id": 1, "method": "tools/call", "params": { "_meta": { "garden/responseView": "machine" } } });
        assert_eq!(with_agent_view(&machine), machine);
    }

    #[test]
    fn the_bridge_does_not_repair_malformed_requests_or_rewrite_other_methods() {
        // No params at all is read as {} and given the view, as the script does.
        let bare = with_agent_view(&json!({ "id": 1, "method": "tools/call" }));
        assert_eq!(bare["params"]["_meta"]["garden/responseView"], "agent-v1");
        for request in [
            json!({ "method": "tools/call", "params": null }),
            json!({ "method": "tools/call", "params": { "_meta": null } }),
            json!({ "method": "tools/call", "params": [] }),
            json!({ "method": "tools/list", "params": {} }),
            json!({ "method": "initialize", "params": {} }),
        ] {
            assert_eq!(with_agent_view(&request), request);
        }
    }

    #[test]
    fn manifest_discovery_honors_the_environment_and_the_platform() {
        let env = Env { profile_dir: Some("/tmp/garden-profile".into()), ..Default::default() };
        assert_eq!(default_manifest_path("macos", &env), PathBuf::from("/tmp/garden-profile/loopback.json"));
        let env = Env { manifest: Some("/x/loopback.json".into()), profile_dir: Some("/ignored".into()), ..Default::default() };
        assert_eq!(default_manifest_path("linux", &env), PathBuf::from("/x/loopback.json"));
        let env = Env { home: Some("/Users/someone".into()), ..Default::default() };
        assert_eq!(
            default_manifest_path("macos", &env),
            PathBuf::from("/Users/someone/Library/Application Support/dev.sophia.garden/profiles/default/loopback.json")
        );
        let env = Env { home: Some("/home/someone".into()), xdg_data_home: Some("/data".into()), ..Default::default() };
        assert_eq!(default_manifest_path("linux", &env), PathBuf::from("/data/dev.sophia.garden/profiles/default/loopback.json"));
    }

    #[test]
    fn url_discovery_reads_only_mcp_url_from_the_manifest() {
        let env = Env { manifest: Some("/tmp/loopback.json".into()), ..Default::default() };
        let url = resolve_mcp_url(&env, &|_| Ok(json!({ "mcpUrl": "http://127.0.0.1:4123/mcp", "token": "owner-secret" }).to_string())).unwrap();
        assert_eq!(url, "http://127.0.0.1:4123/mcp");
        let missing = resolve_mcp_url(&env, &|_| Err("no such file".into())).unwrap_err();
        assert!(missing.contains("Mycl is not running"), "{missing}");
        let env = Env { mcp_url: Some(" http://127.0.0.1:1/mcp ".into()), ..Default::default() };
        assert_eq!(resolve_mcp_url(&env, &|_| panic!("not read")).unwrap(), "http://127.0.0.1:1/mcp");
    }

    #[tokio::test]
    async fn forwarding_presents_the_named_token_never_the_manifest_token() {
        let (ctx, seen) = ctx(named_env(), |request| json_response(json!({ "jsonrpc": "2.0", "id": request["id"], "result": { "tools": [] } })));
        let result = forward_rpc(&json!({ "jsonrpc": "2.0", "id": 7, "method": "tools/list", "params": {} }), &ctx).await.unwrap();
        assert_eq!(result["result"], json!({ "tools": [] }));
        let seen = seen.lock().unwrap();
        let (url, token, body) = &seen[0];
        assert_eq!(url, "http://127.0.0.1:4123/mcp");
        assert_eq!(token, "named-agent-token");
        assert!(!format!("{seen:?}").contains("owner-secret"));
        assert!(!body.contains("owner-secret"));
    }

    #[tokio::test]
    async fn forwarding_opts_in_to_the_agent_view_without_altering_errors() {
        let error = json!({ "jsonrpc": "2.0", "id": 3, "error": { "code": -32602, "message": "bad parameter" } });
        let expected = error.clone();
        let (ctx, seen) = ctx(named_env(), move |_| json_response(expected.clone()));
        let result = forward_rpc(&json!({ "id": 3, "method": "tools/call", "params": { "name": "recall", "arguments": {} } }), &ctx).await.unwrap();
        assert_eq!(result, error);
        let body: Value = serde_json::from_str(&seen.lock().unwrap()[0].2).unwrap();
        assert_eq!(body["params"]["_meta"]["garden/responseView"], "agent-v1");
    }

    #[tokio::test]
    async fn batches_answer_in_order_and_consume_notifications() {
        let (ctx, seen) = ctx(named_env(), |request| json_response(json!({ "jsonrpc": "2.0", "id": request["id"], "result": {} })));
        let line = json!([
            { "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} },
            { "jsonrpc": "2.0", "method": "notifications/initialized" },
            { "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} },
        ]);
        let result = handle_line(&line.to_string(), &ctx).await.unwrap();
        let ids: Vec<i64> = result.as_array().unwrap().iter().map(|r| r["id"].as_i64().unwrap()).collect();
        assert_eq!(ids, vec![1, 2]);
        assert_eq!(seen.lock().unwrap().len(), 2);
        assert!(handle_line("[]", &ctx).await.unwrap()["error"]["message"].as_str().unwrap().contains("empty"));
    }

    #[tokio::test]
    async fn finite_event_streams_are_decoded_by_request_id() {
        let (ctx, _) = ctx(named_env(), |_| FetchResponse {
            status: 200,
            content_type: Some("text/event-stream".into()),
            body: "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":9,\"result\":{}}\n\ndata: {\"jsonrpc\":\"2.0\",\"id\":4,\"result\":{\"tools\":[]}}\n\n".into(),
        });
        let result = forward_rpc(&json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/list", "params": {} }), &ctx).await.unwrap();
        assert_eq!(result["result"], json!({ "tools": [] }));
        let missing = forward_rpc(&json!({ "jsonrpc": "2.0", "id": 5, "method": "tools/list" }), &ctx).await.unwrap_err();
        assert!(missing.contains("without a matching response"));
    }

    #[tokio::test]
    async fn notifications_are_consumed_without_calling_garden() {
        let (ctx, seen) = ctx(named_env(), |_| panic!("Garden must not be called"));
        assert!(handle_line(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }).to_string(), &ctx).await.is_none());
        assert!(handle_line("   ", &ctx).await.is_none());
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_missing_named_token_is_a_protocol_error_not_an_exit() {
        let (ctx, _) = ctx(Env::default(), |_| panic!("Garden must not be called"));
        let result = handle_line(&json!({ "jsonrpc": "2.0", "id": 9, "method": "initialize", "params": {} }).to_string(), &ctx).await.unwrap();
        assert_eq!(result["id"], 9);
        assert!(result["error"]["message"].as_str().unwrap().contains("GARDEN_MCP_TOKEN"));
        let bad = handle_line("{not json", &ctx).await.unwrap();
        assert!(bad["error"]["message"].as_str().unwrap().contains("Invalid JSON"));
        assert_eq!(bad["id"], Value::Null);
    }

    #[tokio::test]
    async fn an_http_failure_becomes_an_error_with_the_body() {
        let (ctx, _) = ctx(named_env(), |_| FetchResponse { status: 401, content_type: None, body: "unauthorized".into() });
        let result = handle_line(&json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }).to_string(), &ctx).await.unwrap();
        assert_eq!(result["error"]["message"], "Mycl MCP returned HTTP 401: unauthorized");
    }

    #[test]
    fn structured_content_is_dropped_only_for_clients_that_opt_in() {
        let call = json!({ "id": 1, "method": "tools/call", "params": { "name": "value" } });
        let response = json!({ "jsonrpc": "2.0", "id": 1, "result": { "content": [{ "type": "text", "text": "{\"a\":1}" }], "structuredContent": { "a": 1 } } });
        assert_eq!(without_structured_content(&call, response.clone(), &Env::default()), response);
        let env = Env { drop_structured: true, ..Default::default() };
        let dropped = without_structured_content(&call, response.clone(), &env);
        assert_eq!(dropped["result"], json!({ "content": [{ "type": "text", "text": "{\"a\":1}" }] }));
        let only_structured = json!({ "id": 2, "result": { "content": [], "structuredContent": { "a": 1 } } });
        assert_eq!(without_structured_content(&call, only_structured.clone(), &env), only_structured);
        let error = json!({ "id": 3, "error": { "code": -32000, "message": "x" } });
        assert_eq!(without_structured_content(&call, error.clone(), &env), error);
        let list = json!({ "id": 4, "method": "tools/list" });
        let tools = json!({ "id": 4, "result": { "tools": [{ "name": "value", "inputSchema": {}, "outputSchema": { "type": "object" } }, { "name": "recall", "inputSchema": {} }] } });
        let stripped = without_structured_content(&list, tools.clone(), &env);
        assert_eq!(stripped["result"]["tools"], json!([{ "name": "value", "inputSchema": {} }, { "name": "recall", "inputSchema": {} }]));
        assert_eq!(without_structured_content(&list, tools.clone(), &Env::default()), tools);
    }

    #[tokio::test]
    async fn a_stuck_call_does_not_hold_the_calls_behind_it() {
        // The first call waits on a gate the second call opens; the second
        // answer must be written first.
        let gate = Arc::new(tokio::sync::Notify::new());
        struct Slow(Arc<tokio::sync::Notify>);
        impl Fetch for Slow {
            fn post(&self, _url: &str, _token: &str, body: String, _timeout: Duration) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<FetchResponse, String>> + Send + '_>> {
                let request: Value = serde_json::from_str(&body).unwrap();
                let gate = self.0.clone();
                Box::pin(async move {
                    if request["id"] == 1 { gate.notified().await } else { gate.notify_one() }
                    Ok(json_response(json!({ "jsonrpc": "2.0", "id": request["id"], "result": {} })))
                })
            }
        }
        let ctx = Context {
            env: named_env(),
            fetch: Box::new(Slow(gate)),
            read: Box::new(|_| Ok(json!({ "mcpUrl": "http://127.0.0.1:4123/mcp" }).to_string())),
        };
        let ctx = Arc::new(ctx);
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut set = tokio::task::JoinSet::new();
        for id in [1, 2] {
            let (ctx, order) = (ctx.clone(), order.clone());
            set.spawn(async move {
                let line = json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call" }).to_string();
                let response = handle_line(&line, &ctx).await.unwrap();
                order.lock().unwrap().push(response["id"].as_i64().unwrap());
            });
        }
        while set.join_next().await.is_some() {}
        assert_eq!(*order.lock().unwrap(), vec![2, 1]);
    }
}
