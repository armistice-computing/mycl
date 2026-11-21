//! The Dreamer: the one model call in the dream cycle ("Dreaming Port Plan —
//! cloud-1 cycle onto offline Garden (2026-10-02)"). A system prompt and a
//! user prompt go in; a JSON object comes out; at most a few calls a night.
//!
//! Two kinds, chosen by the person in settings:
//! - `endpoint`: an OpenAI-compatible chat endpoint (Ollama, LM Studio,
//!   llama-server, or a remote API with a stored provider key).
//! - `app`: an AI app already installed on this machine, run once headless
//!   (Kimi Code, Claude Code, Codex, or a custom command), on the person's own
//!   login, so dreaming spends no API credits.
//!
//! A headless agent app is more than a model, so every app run is isolated
//! (Epsilon's boundary, 2026-10-02): the app's own tools, MCP servers, hooks,
//! plugins and user settings are switched off by flags or a Garden-owned
//! config home; the environment is cleared down to what login needs; the
//! working directory is a fresh empty folder; when the run ends, for any
//! reason, every process left in its group is killed; output is bounded and
//! never waited on past a short drain; and the caller's consent gate is asked
//! before any preparation and again immediately before the process starts.
//!
//! What this is not: an OS sandbox. "Isolated" means the app's own
//! configuration surfaces are switched off; the group kill is lifecycle
//! hygiene, and a descendant that deliberately leaves the group (setsid)
//! outlives it. Containing a hostile program would need an OS boundary; the
//! presets run apps the person already trusts with their login. The Kimi
//! preset shares that login's credential files by symlink, a trusted-app
//! contract, not credential isolation (Epsilon's review, 2026-10-02).
//! Presets whose tools cannot be removed (Codex) and custom commands run only
//! when the settings say the person accepted reduced isolation.
// Reached from the desktop commands today; the headless dream cycle
// (Phase 2) will call it too.
#![cfg_attr(not(feature = "desktop"), allow(dead_code))]

use crate::app_runtime::AppHandle;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    process,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

const SETTINGS_FILE: &str = "dreamer.json";
const SCHEMA_VERSION: u32 = 1;
pub(crate) const DEFAULT_TIMEOUT: Duration = Duration::from_secs(180);
/// The receipt error for a reply that ran but carried no JSON object, the
/// one failure worth asking again.
pub(crate) const NO_JSON_ERROR: &str = "reply had no JSON object";
const MAX_STDOUT_BYTES: usize = 256 * 1024;
const MAX_STDERR_BYTES: usize = 64 * 1024;
/// After the run ends and its group is killed, how long the output readers
/// may take to finish. A process that escaped the group can hold a pipe open
/// forever; we stop waiting and keep what was read.
const DRAIN_GRACE: Duration = Duration::from_secs(2);
/// How many `{` positions the JSON search tries before giving up.
const MAX_JSON_CANDIDATES: usize = 256;
/// cloud-1 articulator: temperature 0.8, JSON object out, reasoning off.
pub(crate) const DREAM_TEMPERATURE: f64 = 0.8;
pub(crate) const DREAM_MAX_TOKENS: u32 = 2048;

// ---------------------------------------------------------------- settings

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum AppPreset {
    Kimi,
    Claude,
    Codex,
    Custom,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub(crate) enum DreamerConfig {
    /// Dreaming has no dreamer until the person chooses one.
    #[serde(rename = "none")]
    None,
    #[serde(rename = "endpoint", rename_all = "camelCase")]
    Endpoint {
        /// e.g. http://localhost:11434/v1 (the path up to /chat/completions).
        base_url: String,
        model: String,
        /// A provider whose stored key is sent as a bearer token
        /// (local_provider_keys); only to that provider's own host.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        api_key_provider: Option<String>,
    },
    #[serde(rename = "app", rename_all = "camelCase")]
    App {
        preset: AppPreset,
        /// Override for where the app lives; detected when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        program: Option<String>,
        /// The app's model name or alias; the app's default when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        /// Custom preset only: the arguments. The prompt arrives on stdin.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
        /// Required for presets that cannot be fully isolated (Codex, custom).
        #[serde(default)]
        accept_reduced_isolation: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DreamerSettingsFile {
    schema_version: u32,
    dreamer: DreamerConfig,
}

fn settings_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(crate::profile_paths::profile_dir(app)?.join(SETTINGS_FILE))
}

pub(crate) fn read_dreamer_config(app: &AppHandle) -> Result<DreamerConfig, String> {
    let path = settings_path(app)?;
    if !path.exists() {
        return Ok(DreamerConfig::None);
    }
    let file: DreamerSettingsFile =
        crate::storage::read_json(&path).map_err(|error| error.to_string())?;
    Ok(file.dreamer)
}

pub(crate) fn write_dreamer_config(app: &AppHandle, dreamer: DreamerConfig) -> Result<(), String> {
    validate_config(&dreamer)?;
    let file = DreamerSettingsFile { schema_version: SCHEMA_VERSION, dreamer };
    crate::storage::write_json(&settings_path(app)?, &file).map_err(|error| error.to_string())
}

/// The host a stored provider key may be sent to. A key never goes anywhere
/// else, whatever URL the settings name.
fn provider_key_host(provider: &str) -> Option<&'static str> {
    match provider {
        "openrouter" => Some("openrouter.ai"),
        "openai" => Some("api.openai.com"),
        "anthropic" => Some("api.anthropic.com"),
        _ => None,
    }
}

pub(crate) fn validate_config(config: &DreamerConfig) -> Result<(), String> {
    match config {
        DreamerConfig::None => Ok(()),
        DreamerConfig::Endpoint { base_url, model, api_key_provider } => {
            let url = reqwest::Url::parse(base_url.trim())
                .map_err(|error| format!("endpoint URL: {error}"))?;
            if !matches!(url.scheme(), "http" | "https") {
                return Err("endpoint URL must be http or https".into());
            }
            if model.trim().is_empty() {
                return Err("endpoint model must not be empty".into());
            }
            if let Some(provider) = api_key_provider {
                let host = provider_key_host(provider)
                    .ok_or_else(|| format!("no stored key belongs to provider {provider}"))?;
                if url.scheme() != "https" || url.host_str() != Some(host) {
                    return Err(format!("the {provider} key is only sent to https://{host}"));
                }
            }
            Ok(())
        }
        DreamerConfig::App { preset, program, accept_reduced_isolation, .. } => {
            if *preset == AppPreset::Custom && program.as_deref().is_none_or(|p| p.trim().is_empty()) {
                return Err("a custom dreamer needs the program to run".into());
            }
            // Refused at save as well as at the call, so a dreamer bound to
            // fail at night is never stored.
            if preset_isolation(*preset) != Isolation::Isolated && !accept_reduced_isolation {
                return Err("this dreamer can't be fully isolated; accept that in Dreaming settings to use it".into());
            }
            Ok(())
        }
    }
}

// ---------------------------------------------------------------- isolation

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Isolation {
    /// Tools, MCP, hooks, plugins and user settings off; verified by test.
    Isolated,
    /// Runs in a read-only, no-network sandbox, but its tools can't be removed.
    Sandboxed,
    /// We can't know what an arbitrary command does.
    UserResponsibility,
    /// No process: an HTTP call.
    NotApplicable,
}

pub(crate) fn preset_isolation(preset: AppPreset) -> Isolation {
    match preset {
        AppPreset::Kimi | AppPreset::Claude => Isolation::Isolated,
        AppPreset::Codex => Isolation::Sandboxed,
        AppPreset::Custom => Isolation::UserResponsibility,
    }
}

fn preset_program_name(preset: AppPreset) -> Option<&'static str> {
    match preset {
        AppPreset::Kimi => Some("kimi"),
        AppPreset::Claude => Some("claude"),
        AppPreset::Codex => Some("codex"),
        AppPreset::Custom => None,
    }
}

/// Where AI apps install themselves. A Finder-launched app gets a minimal
/// PATH, so the user's shell PATH can't be relied on.
fn search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    if let Some(home) = home_dir() {
        for rel in [".local/bin", ".kimi-code/bin", "Library/pnpm/bin", ".npm-global/bin", ".bun/bin", ".cargo/bin"] {
            dirs.push(home.join(rel));
        }
    }
    for dir in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"] {
        dirs.push(PathBuf::from(dir));
    }
    dirs
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

pub(crate) fn find_program(name: &str) -> Option<PathBuf> {
    let path = Path::new(name);
    if path.components().count() > 1 {
        return path.is_file().then(|| path.to_path_buf());
    }
    search_dirs().into_iter().map(|dir| dir.join(name)).find(|candidate| candidate.is_file())
}

/// The child's whole environment. Login state lives in HOME (keychain and
/// the apps' own credential files), so HOME and USER stay; API keys in our
/// own environment never reach the child.
fn child_env(program: &Path, tmp: &Path, extra: &[(&str, String)]) -> Vec<(String, String)> {
    let mut path_dirs: Vec<String> = Vec::new();
    if let Some(dir) = program.parent() {
        path_dirs.push(dir.display().to_string());
    }
    for dir in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin", "/usr/sbin", "/sbin"] {
        if !path_dirs.iter().any(|d| d == dir) {
            path_dirs.push(dir.to_string());
        }
    }
    let mut env = vec![
        ("PATH".to_string(), path_dirs.join(":")),
        ("TMPDIR".to_string(), tmp.display().to_string()),
        ("LANG".to_string(), "en_US.UTF-8".to_string()),
        ("TERM".to_string(), "dumb".to_string()),
        ("NO_COLOR".to_string(), "1".to_string()),
    ];
    for key in ["HOME", "USER", "LOGNAME"] {
        if let Ok(value) = std::env::var(key) {
            env.push((key.to_string(), value));
        }
    }
    env.extend(extra.iter().map(|(k, v)| (k.to_string(), v.clone())));
    env
}

// ---------------------------------------------------------------- the call

#[derive(Debug, Clone)]
pub(crate) struct DreamRequest {
    pub(crate) system: String,
    pub(crate) user: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DreamReceipt {
    pub(crate) kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) preset: Option<AppPreset>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) program: Option<String>,
    /// The model the settings asked for; the app's default when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) configured_model: Option<String>,
    pub(crate) isolation: Isolation,
    pub(crate) accepted_reduced_isolation: bool,
    /// "local" (this machine), "remote" (an HTTP API elsewhere), or "app"
    /// (the app's own model service; the app decides where it goes).
    pub(crate) network: &'static str,
    pub(crate) consent: String,
    pub(crate) started_at: String,
    pub(crate) duration_ms: u128,
    /// "exited", "timed-out", "cancelled", or "not-started".
    pub(crate) ended: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) attempts: Option<u32>,
    pub(crate) output_bytes: usize,
    pub(crate) output_truncated: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub(crate) stderr_tail: String,
    pub(crate) ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
}

impl DreamReceipt {
    fn new(kind: &'static str, config: &DreamerConfig, isolation: Isolation, network: &'static str, started_at: String) -> Self {
        let (preset, configured_model, accepted) = match config {
            DreamerConfig::App { preset, model, accept_reduced_isolation, .. } => (Some(*preset), model.clone(), *accept_reduced_isolation),
            DreamerConfig::Endpoint { model, .. } => (None, Some(model.clone()), false),
            DreamerConfig::None => (None, None, false),
        };
        DreamReceipt {
            kind,
            preset,
            program: None,
            configured_model,
            isolation,
            accepted_reduced_isolation: accepted,
            network,
            consent: String::new(),
            started_at,
            duration_ms: 0,
            ended: "not-started",
            exit_code: None,
            attempts: None,
            output_bytes: 0,
            output_truncated: false,
            stderr_tail: String::new(),
            ok: false,
            error: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DreamReply {
    /// The reply's JSON object, when it had one.
    pub(crate) json: Option<Value>,
    /// The reply text as the dreamer gave it (bounded).
    pub(crate) text: String,
    pub(crate) receipt: DreamReceipt,
}

fn failed(mut receipt: DreamReceipt, started: Instant, error: String) -> DreamReply {
    receipt.duration_ms = started.elapsed().as_millis();
    receipt.error = Some(error);
    DreamReply { json: None, text: String::new(), receipt }
}

/// Asked before any preparation and again immediately before any process
/// starts or request is sent. Returns a short description of the consent
/// that holds ("user-invoked test", "graph opted in at …"), or an error that
/// stops the call.
pub(crate) type ConsentGate = Arc<dyn Fn() -> Result<String, String> + Send + Sync>;

pub(crate) async fn call_dreamer(
    app: &AppHandle,
    config: &DreamerConfig,
    request: &DreamRequest,
    consent: ConsentGate,
    cancel: Arc<AtomicBool>,
    timeout: Duration,
) -> DreamReply {
    let started_at = crate::clock::timestamp();
    let started = Instant::now();
    if let Err(error) = validate_config(config) {
        let receipt = DreamReceipt::new("none", config, Isolation::NotApplicable, "local", started_at);
        return failed(receipt, started, error);
    }
    match config {
        DreamerConfig::None => {
            let receipt = DreamReceipt::new("none", config, Isolation::NotApplicable, "local", started_at);
            failed(receipt, started, "no dreamer is configured".into())
        }
        DreamerConfig::Endpoint { base_url, model, api_key_provider } => {
            let mut receipt = DreamReceipt::new("endpoint", config, Isolation::NotApplicable, endpoint_network(base_url), started_at);
            if let Err(error) = consent() {
                return failed(receipt, started, error);
            }
            let key = match api_key_provider {
                Some(provider) => match crate::local_provider_keys::get_provider_keys(app.clone()) {
                    Ok(keys) => match keys.get(provider) {
                        Some(key) => Some(key.clone()),
                        None => return failed(receipt, started, format!("no {provider} key is stored")),
                    },
                    Err(error) => return failed(receipt, started, error),
                },
                None => None,
            };
            // The gate is asked again inside, immediately before each send.
            let outcome = call_endpoint(base_url, model, key.as_deref(), request, &consent, &cancel, started + timeout).await;
            receipt.consent = outcome.consent.clone();
            receipt.duration_ms = started.elapsed().as_millis();
            receipt.attempts = Some(outcome.attempts);
            receipt.ended = outcome.ended;
            match outcome.result {
                Ok((text, truncated)) => {
                    receipt.output_bytes = text.len();
                    receipt.output_truncated = truncated;
                    let json = extract_json_object(&text, Some("narrative"));
                    receipt.ok = json.is_some();
                    if json.is_none() {
                        receipt.error = Some(NO_JSON_ERROR.into());
                    }
                    DreamReply { json, text: truncate(&text, 16 * 1024), receipt }
                }
                Err(error) => failed(receipt, started, error),
            }
        }
        DreamerConfig::App { preset, .. } => {
            let receipt = DreamReceipt::new("app", config, preset_isolation(*preset), "app", started_at);
            let moved_config = config.clone();
            let request = request.clone();
            let moved_receipt = receipt.clone();
            let joined = crate::app_runtime::async_runtime::spawn_blocking(move || {
                run_app(&moved_config, &request, &consent, &cancel, timeout, moved_receipt, started)
            })
            .await;
            match joined {
                Ok(reply) => reply,
                Err(error) => failed(receipt, started, error.to_string()),
            }
        }
    }
}

fn endpoint_network(base_url: &str) -> &'static str {
    let host = reqwest::Url::parse(base_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase));
    match host.as_deref() {
        Some("localhost") | Some("127.0.0.1") | Some("::1") | Some("[::1]") => "local",
        _ => "remote",
    }
}

struct EndpointOutcome {
    result: Result<(String, bool), String>,
    attempts: u32,
    ended: &'static str,
    /// The consent note from the gate before the last send.
    consent: String,
}

/// One deadline across every attempt; cancel is checked between and during
/// attempts; consent is asked immediately before each send; redirects are
/// never followed (a redirect could carry the prompt to a host the settings
/// didn't name); the reply body is read to a bound.
async fn call_endpoint(
    base_url: &str,
    model: &str,
    key: Option<&str>,
    request: &DreamRequest,
    consent: &ConsentGate,
    cancel: &AtomicBool,
    deadline: Instant,
) -> EndpointOutcome {
    let url = format!("{}/chat/completions", base_url.trim().trim_end_matches('/'));
    let client = match reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build() {
        Ok(client) => client,
        Err(error) => return EndpointOutcome { result: Err(format!("http client: {error}")), attempts: 0, ended: "not-started", consent: String::new() },
    };
    let mut consent_note = String::new();
    let mut body = json!({
        "model": model,
        "messages": [
            {"role": "system", "content": request.system},
            {"role": "user", "content": request.user},
        ],
        "temperature": DREAM_TEMPERATURE,
        "max_tokens": DREAM_MAX_TOKENS,
        "response_format": {"type": "json_object"},
        // cloud-1: reasoning off keeps the voice and the JSON intact.
        "reasoning_effort": "none",
    });
    let mut attempts = 0;
    // Servers that don't know an optional field reject it; retry once
    // without it, then once more without the JSON mode.
    for attempt in 0..3 {
        if attempt == 1 {
            body.as_object_mut().map(|b| b.remove("reasoning_effort"));
        }
        if attempt == 2 {
            body.as_object_mut().map(|b| b.remove("response_format"));
        }
        if cancel.load(Ordering::SeqCst) {
            return EndpointOutcome { result: Err("cancelled".into()), attempts, ended: "cancelled", consent: consent_note.clone() };
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return EndpointOutcome { result: Err("timed out".into()), attempts, ended: "timed-out", consent: consent_note.clone() };
        }
        match consent() {
            Ok(note) => consent_note = note,
            Err(error) => return EndpointOutcome { result: Err(error), attempts, ended: "not-started", consent: consent_note.clone() },
        }
        attempts += 1;
        let mut builder = client.post(&url).timeout(remaining).json(&body);
        if let Some(key) = key {
            builder = builder.bearer_auth(key);
        }
        let send = async {
            let response = builder.send().await.map_err(|error| format!("request: {error}"))?;
            let status = response.status();
            let (text, truncated) = read_body_bounded(response, MAX_STDOUT_BYTES).await?;
            Ok::<_, String>((status, text, truncated))
        };
        let watched = async {
            loop {
                tokio::time::sleep(Duration::from_millis(200)).await;
                if cancel.load(Ordering::SeqCst) {
                    break;
                }
            }
        };
        let (status, text, truncated) = tokio::select! {
            result = send => match result {
                Ok(parts) => parts,
                Err(error) => {
                    let ended = if Instant::now() >= deadline { "timed-out" } else { "exited" };
                    return EndpointOutcome { result: Err(error), attempts, ended, consent: consent_note.clone() };
                }
            },
            _ = watched => return EndpointOutcome { result: Err("cancelled".into()), attempts, ended: "cancelled", consent: consent_note.clone() },
        };
        if status.is_success() {
            if truncated {
                return EndpointOutcome { result: Err(format!("reply exceeded {MAX_STDOUT_BYTES} bytes")), attempts, ended: "exited", consent: consent_note.clone() };
            }
            let parsed = serde_json::from_str::<Value>(&text)
                .map_err(|error| format!("reply JSON: {error}"))
                .and_then(|value| {
                    value
                        .pointer("/choices/0/message/content")
                        .and_then(Value::as_str)
                        .map(|content| (content.to_string(), false))
                        .ok_or_else(|| "reply has no choices[0].message.content".to_string())
                });
            return EndpointOutcome { result: parsed, attempts, ended: "exited", consent: consent_note.clone() };
        }
        let retryable = matches!(status.as_u16(), 400 | 422);
        if !retryable || attempt == 2 {
            return EndpointOutcome {
                result: Err(format!("endpoint returned {status}: {}", truncate(&text, 300))),
                attempts,
                ended: "exited",
                consent: consent_note.clone(),
            };
        }
    }
    unreachable!("the loop returns on its last attempt")
}

async fn read_body_bounded(mut response: reqwest::Response, cap: usize) -> Result<(String, bool), String> {
    let mut kept: Vec<u8> = Vec::new();
    let mut truncated = false;
    while let Some(chunk) = response.chunk().await.map_err(|error| format!("read reply: {error}"))? {
        let room = cap.saturating_sub(kept.len());
        if chunk.len() > room {
            kept.extend_from_slice(&chunk[..room]);
            truncated = true;
            break;
        }
        kept.extend_from_slice(&chunk);
    }
    Ok((String::from_utf8_lossy(&kept).into_owned(), truncated))
}

// ---------------------------------------------------------------- app runs

struct AppInvocation {
    program: PathBuf,
    args: Vec<String>,
    stdin: Option<String>,
    extra_env: Vec<(&'static str, String)>,
    /// Codex writes its final message to a file instead of stdout.
    output_file: Option<PathBuf>,
    /// Kimi's stream-json: take the last assistant message.
    stream_json: bool,
    /// Kimi keeps sessions in its home; removed after the run.
    cleanup: Option<PathBuf>,
}

#[allow(clippy::too_many_arguments)]
fn run_app(
    config: &DreamerConfig,
    request: &DreamRequest,
    consent: &ConsentGate,
    cancel: &AtomicBool,
    timeout: Duration,
    mut receipt: DreamReceipt,
    started: Instant,
) -> DreamReply {
    let DreamerConfig::App { preset, program, model, args, accept_reduced_isolation } = config else {
        return failed(receipt, started, "not an app dreamer".into());
    };
    if cancel.load(Ordering::SeqCst) {
        receipt.ended = "cancelled";
        return failed(receipt, started, "cancelled".into());
    }
    // Nothing is prepared (no config with a key is written) without consent.
    if let Err(error) = consent() {
        return failed(receipt, started, error);
    }
    if receipt.isolation != Isolation::Isolated && !accept_reduced_isolation {
        return failed(
            receipt,
            started,
            "this dreamer can't be fully isolated; enable it in Dreaming settings to accept that".into(),
        );
    }
    let name = program.clone().or_else(|| preset_program_name(*preset).map(str::to_string));
    let Some(found) = name.as_deref().and_then(find_program) else {
        return failed(receipt, started, "the app was not found on this machine".into());
    };
    receipt.program = Some(found.display().to_string());
    let workdir = match tempdir("garden-dream") {
        Ok(dir) => dir,
        Err(error) => return failed(receipt, started, error),
    };
    let invocation = match build_invocation(*preset, &found, model.as_deref(), args, request, &workdir) {
        Ok(invocation) => invocation,
        Err(error) => {
            let _ = std::fs::remove_dir_all(&workdir);
            return failed(receipt, started, error);
        }
    };
    let env = child_env(&invocation.program, &workdir, &invocation.extra_env);
    // Asked again, last, immediately before the process starts.
    match consent() {
        Ok(note) => receipt.consent = note,
        Err(error) => {
            cleanup_run(&workdir, &invocation);
            return failed(receipt, started, error);
        }
    }
    // One deadline from the start of the call, not from the end of
    // preparation; a cancel or a spent deadline means the app never starts.
    let remaining = timeout.saturating_sub(started.elapsed());
    if cancel.load(Ordering::SeqCst) || remaining.is_zero() {
        cleanup_run(&workdir, &invocation);
        receipt.ended = if remaining.is_zero() { "timed-out" } else { "cancelled" };
        let error = if remaining.is_zero() { "timed out before the app started" } else { "cancelled" };
        return failed(receipt, started, error.into());
    }
    let output = run_bounded(&invocation.program, &invocation.args, invocation.stdin.as_deref(), &workdir, &env, cancel, remaining);
    receipt.duration_ms = started.elapsed().as_millis();
    let reply = match output {
        Err(error) => failed(receipt, started, error),
        Ok(out) => {
            receipt.ended = out.end.label();
            receipt.exit_code = out.end.exit_code();
            receipt.stderr_tail = tail(&String::from_utf8_lossy(&out.stderr), 500);
            let raw = match &invocation.output_file {
                Some(path) => read_file_bounded(path, MAX_STDOUT_BYTES),
                None => (String::from_utf8_lossy(&out.stdout).into_owned(), out.stdout_truncated),
            };
            receipt.output_bytes = raw.0.len();
            receipt.output_truncated = raw.1;
            let text = if invocation.stream_json { last_assistant_message(&raw.0) } else { raw.0 };
            let json = extract_json_object(&text, Some("narrative"));
            receipt.error = match out.end {
                RunEnd::Exited(Some(0)) if json.is_some() => None,
                RunEnd::Exited(Some(0)) => Some(NO_JSON_ERROR.into()),
                RunEnd::Exited(code) => Some(format!("the app exited with {code:?}")),
                RunEnd::TimedOut => Some(format!("timed out after {}s", timeout.as_secs())),
                RunEnd::Cancelled => Some("cancelled".into()),
            };
            receipt.ok = receipt.error.is_none();
            DreamReply { json, text: truncate(&text, 16 * 1024), receipt }
        }
    };
    cleanup_run(&workdir, &invocation);
    reply
}

fn cleanup_run(workdir: &Path, invocation: &AppInvocation) {
    let _ = std::fs::remove_dir_all(workdir);
    if let Some(path) = &invocation.cleanup {
        let _ = std::fs::remove_dir_all(path);
    }
}

fn read_file_bounded(path: &Path, cap: usize) -> (String, bool) {
    match std::fs::File::open(path) {
        Ok(file) => {
            let mut kept = Vec::new();
            let read = file.take(cap as u64 + 1).read_to_end(&mut kept);
            let truncated = read.is_ok() && kept.len() > cap;
            kept.truncate(cap);
            (String::from_utf8_lossy(&kept).into_owned(), truncated)
        }
        Err(_) => (String::new(), false),
    }
}

fn build_invocation(
    preset: AppPreset,
    program: &Path,
    model: Option<&str>,
    custom_args: &[String],
    request: &DreamRequest,
    workdir: &Path,
) -> Result<AppInvocation, String> {
    let mut args: Vec<String> = Vec::new();
    let mut invocation = AppInvocation {
        program: program.into(),
        args: Vec::new(),
        stdin: None,
        extra_env: Vec::new(),
        output_file: None,
        stream_json: false,
        cleanup: None,
    };
    match preset {
        AppPreset::Claude => {
            let system_file = workdir.join("system-prompt.txt");
            crate::storage_file_ops::write_bytes(&system_file, request.system.as_bytes())
                .map_err(|e| format!("write system prompt: {e}"))?;
            // --safe-mode: no CLAUDE.md, skills, plugins, hooks, MCP;
            // --setting-sources "": no user/project/local settings;
            // --tools "": no tools at all. Login is unaffected.
            args.extend(
                ["-p", "--safe-mode", "--tools", "", "--strict-mcp-config", "--setting-sources", "", "--no-session-persistence"]
                    .map(String::from),
            );
            args.push("--system-prompt-file".into());
            args.push(system_file.display().to_string());
            if let Some(model) = model {
                args.push("--model".into());
                args.push(model.into());
            }
            invocation.stdin = Some(request.user.clone());
        }
        AppPreset::Kimi => {
            let home = kimi_dream_home(&workdir.join("kimi-home"))?;
            let agent_file = workdir.join("dreamer.md");
            crate::storage_file_ops::write_bytes(&agent_file, kimi_agent_file(&request.system).as_bytes())
                .map_err(|e| format!("write agent file: {e}"))?;
            args.extend(["--agent-file".to_string(), agent_file.display().to_string()]);
            args.extend(["--output-format".to_string(), "stream-json".to_string()]);
            if let Some(model) = model {
                args.extend(["-m".to_string(), model.to_string()]);
            }
            // Kimi's prompt mode takes the prompt as an argument (visible to
            // this user's own `ps`; bounded by ARG_MAX).
            args.extend(["-p".to_string(), request.user.clone()]);
            invocation.extra_env = vec![("KIMI_CODE_HOME", home.display().to_string())];
            invocation.stream_json = true;
            // The home lives in the run's folder and goes with it.
        }
        AppPreset::Codex => {
            let out = workdir.join("last-message.txt");
            args.extend(
                ["exec", "--ephemeral", "--ignore-user-config", "--sandbox", "read-only", "--skip-git-repo-check"]
                    .map(String::from),
            );
            args.extend(["-C".to_string(), workdir.display().to_string()]);
            args.extend(["-o".to_string(), out.display().to_string()]);
            if let Some(model) = model {
                args.extend(["-m".to_string(), model.to_string()]);
            }
            args.push("-".into());
            invocation.stdin = Some(format!("{}\n\n{}", request.system, request.user));
            invocation.output_file = Some(out);
        }
        AppPreset::Custom => {
            args = custom_args.to_vec();
            invocation.stdin = Some(format!("{}\n\n{}", request.system, request.user));
        }
    }
    invocation.args = args;
    Ok(invocation)
}

fn kimi_agent_file(system: &str) -> String {
    format!(
        "---\nname: garden-dreamer\ndescription: Garden's dreamer. Writes from the fragments it is given. No tools.\ntools: []\n---\n\n{system}\n"
    )
}

/// A Kimi Code home for one run, inside that run's own folder (so two runs
/// never share or delete each other's config or sessions): the person's
/// login, symlinked, so a token refresh updates their real login rather than
/// forking it; their provider and model definitions; thinking at its lowest;
/// and nothing else: no hooks, permission rules, MCP servers or services from
/// their own config. Removed with the run's folder.
fn kimi_dream_home(home: &Path) -> Result<PathBuf, String> {
    let user_home = home_dir().ok_or("HOME is not set")?.join(".kimi-code");
    let user_config = std::fs::read_to_string(user_home.join("config.toml"))
        .map_err(|error| format!("read Kimi Code config: {error}"))?;
    let config = kimi_dream_config(&user_config)?;
    std::fs::create_dir_all(home).map_err(|error| format!("create Kimi dream home: {error}"))?;
    for name in ["credentials", "oauth", "device_id", "region"] {
        relink(&home.join(name), &user_home.join(name))?;
    }
    write_private(&home.join("config.toml"), &config)?;
    write_private(&home.join("mcp.json"), "{\"mcpServers\": {}}\n")?;
    Ok(home.to_path_buf())
}

/// Make `link` point at `target`: repaired when stale or dangling, removed
/// when the target no longer exists.
fn relink(link: &Path, target: &Path) -> Result<(), String> {
    let existing = std::fs::symlink_metadata(link).is_ok();
    let current = std::fs::read_link(link).ok();
    let target_exists = target.exists();
    if existing && (current.as_deref() != Some(target) || !target_exists) {
        std::fs::remove_file(link).map_err(|error| format!("replace {}: {error}", link.display()))?;
    }
    if target_exists && std::fs::symlink_metadata(link).is_err() {
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).map_err(|error| format!("link {}: {error}", link.display()))?;
    }
    Ok(())
}

/// Parsed, not filtered: keep `default_model`, the `providers` and `models`
/// tables (each model's default effort lowered), and a `thinking` table at
/// its lowest. Everything else (hooks, permissions, MCP, services, any other
/// top-level key) is dropped. Kimi Code's K3 models always think; "low" is
/// the least it allows.
pub(crate) fn kimi_dream_config(user_config: &str) -> Result<String, String> {
    let user: toml::Table = user_config.parse().map_err(|error| format!("Kimi Code config is not valid TOML: {error}"))?;
    let mut out = toml::Table::new();
    if let Some(value @ toml::Value::String(_)) = user.get("default_model") {
        out.insert("default_model".into(), value.clone());
    }
    for key in ["providers", "models"] {
        if let Some(toml::Value::Table(table)) = user.get(key) {
            let mut table = table.clone();
            if key == "models" {
                for (_, model) in table.iter_mut() {
                    if let Some(model) = model.as_table_mut() {
                        if model.contains_key("default_effort") {
                            model.insert("default_effort".into(), toml::Value::String("low".into()));
                        }
                    }
                }
            }
            out.insert(key.into(), toml::Value::Table(table));
        }
    }
    let mut thinking = toml::Table::new();
    thinking.insert("enabled".into(), toml::Value::Boolean(false));
    thinking.insert("effort".into(), toml::Value::String("low".into()));
    out.insert("thinking".into(), toml::Value::Table(thinking));
    toml::to_string(&out).map_err(|error| format!("write Kimi dream config: {error}"))
}

/// Created 0o600 from the start: the Kimi config carries the provider's key.
fn write_private(path: &Path, contents: &str) -> Result<(), String> {
    let temp = crate::storage_atomic::atomic_temp_path(path);
    let result = crate::storage_atomic::write_bytes_atomic(path, &temp, contents.as_bytes(), Some(0o600));
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

fn tempdir(prefix: &str) -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).map_err(|error| format!("create work folder: {error}"))?;
    Ok(dir)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunEnd {
    Exited(Option<i32>),
    TimedOut,
    Cancelled,
}

impl RunEnd {
    fn label(self) -> &'static str {
        match self {
            RunEnd::Exited(_) => "exited",
            RunEnd::TimedOut => "timed-out",
            RunEnd::Cancelled => "cancelled",
        }
    }

    fn exit_code(self) -> Option<i32> {
        match self {
            RunEnd::Exited(code) => code,
            _ => None,
        }
    }
}

#[derive(Debug)]
pub(crate) struct BoundedOutput {
    pub(crate) end: RunEnd,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stdout_truncated: bool,
    pub(crate) stderr: Vec<u8>,
}

type SharedBuffer = Arc<Mutex<(Vec<u8>, bool)>>;

/// Read a pipe into a shared, capped buffer; past the cap, keep draining so
/// the writer never blocks. Signals when the pipe closes.
fn spawn_reader(mut pipe: impl Read + Send + 'static, cap: usize) -> (SharedBuffer, mpsc::Receiver<()>) {
    let buffer: SharedBuffer = Arc::new(Mutex::new((Vec::new(), false)));
    let (done, finished) = mpsc::channel();
    let shared = buffer.clone();
    thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut guard = shared.lock().unwrap_or_else(|p| p.into_inner());
                    let room = cap.saturating_sub(guard.0.len());
                    if room >= n {
                        guard.0.extend_from_slice(&chunk[..n]);
                    } else {
                        guard.0.extend_from_slice(&chunk[..room]);
                        guard.1 = true;
                    }
                }
            }
        }
        let _ = done.send(());
    });
    (buffer, finished)
}

fn take_buffer(buffer: &SharedBuffer) -> (Vec<u8>, bool) {
    let mut guard = buffer.lock().unwrap_or_else(|p| p.into_inner());
    (std::mem::take(&mut guard.0), guard.1)
}

/// Run a program in its own process group with a cleared environment and a
/// given working directory. When it ends, for any reason, every process left
/// in its group is killed; the output readers then get DRAIN_GRACE to finish
/// and are never waited on longer (a process that escaped the group may hold
/// a pipe open). The stdin writer is never joined, for the same reason.
pub(crate) fn run_bounded(
    program: &Path,
    args: &[String],
    stdin: Option<&str>,
    cwd: &Path,
    env: &[(String, String)],
    cancel: &AtomicBool,
    timeout: Duration,
) -> Result<BoundedOutput, String> {
    let mut command = process::Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(if stdin.is_some() { process::Stdio::piped() } else { process::Stdio::null() })
        .stdout(process::Stdio::piped())
        .stderr(process::Stdio::piped());
    crate::process_utils::place_child_in_new_process_group(&mut command);
    let mut child = command.spawn().map_err(|error| format!("start {}: {error}", program.display()))?;
    let leader = child.id();
    if let Some(text) = stdin {
        let mut pipe = child.stdin.take().expect("piped stdin");
        let text = text.to_string();
        thread::spawn(move || {
            let _ = pipe.write_all(text.as_bytes());
        });
    }
    let (out_buffer, out_done) = spawn_reader(child.stdout.take().expect("piped stdout"), MAX_STDOUT_BYTES);
    let (err_buffer, err_done) = spawn_reader(child.stderr.take().expect("piped stderr"), MAX_STDERR_BYTES);
    let started = Instant::now();
    let end = loop {
        match child.try_wait() {
            Ok(Some(status)) => break RunEnd::Exited(status.code()),
            Ok(None) if cancel.load(Ordering::SeqCst) => {
                crate::process_utils::terminate_child(&mut child, crate::process_utils::DEFAULT_TERMINATE_GRACE);
                break RunEnd::Cancelled;
            }
            Ok(None) if started.elapsed() >= timeout => {
                crate::process_utils::terminate_child(&mut child, crate::process_utils::DEFAULT_TERMINATE_GRACE);
                break RunEnd::TimedOut;
            }
            Ok(None) => thread::sleep(Duration::from_millis(100)),
            Err(error) => {
                crate::process_utils::kill_process_group(leader);
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("poll the app: {error}"));
            }
        }
    };
    // Whatever is still in the group (background children, SIGTERM-ignorers)
    // dies now, whether the leader exited on its own or was stopped.
    crate::process_utils::kill_process_group(leader);
    let drain_until = Instant::now() + DRAIN_GRACE;
    for done in [&out_done, &err_done] {
        let _ = done.recv_timeout(drain_until.saturating_duration_since(Instant::now()));
    }
    let (stdout, stdout_truncated) = take_buffer(&out_buffer);
    let (stderr, _) = take_buffer(&err_buffer);
    Ok(BoundedOutput { end, stdout, stdout_truncated, stderr })
}

// ---------------------------------------------------------------- parsing

/// A JSON object in the text: models wrap replies in code fences or a
/// sentence of preamble. Returns the first object that has `prefer_key`, or
/// else the first object; tries at most MAX_JSON_CANDIDATES positions.
pub(crate) fn extract_json_object(text: &str, prefer_key: Option<&str>) -> Option<Value> {
    let bytes = text.as_bytes();
    let mut first: Option<Value> = None;
    let mut candidates = 0;
    let mut start = 0;
    while let Some(offset) = text[start..].find('{') {
        let open = start + offset;
        candidates += 1;
        if candidates > MAX_JSON_CANDIDATES {
            break;
        }
        let mut depth = 0usize;
        let mut in_string = false;
        let mut escaped = false;
        let mut close = None;
        for (i, &b) in bytes.iter().enumerate().skip(open) {
            if in_string {
                match b {
                    _ if escaped => escaped = false,
                    b'\\' => escaped = true,
                    b'"' => in_string = false,
                    _ => {}
                }
                continue;
            }
            match b {
                b'"' => in_string = true,
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(close) = close else { break };
        if let Ok(value @ Value::Object(_)) = serde_json::from_str::<Value>(&text[open..=close]) {
            if prefer_key.is_none_or(|key| value.get(key).is_some()) {
                return Some(value);
            }
            first.get_or_insert(value);
            start = close + 1;
        } else {
            start = open + 1;
        }
    }
    first
}

/// Kimi Code stream-json: one JSON object per line; replies are
/// `{"role":"assistant","content":"…"}`. Take the last.
pub(crate) fn last_assistant_message(stream: &str) -> String {
    stream
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| event.get("role").and_then(Value::as_str) == Some("assistant"))
        .filter_map(|event| event.get("content").and_then(Value::as_str).map(str::to_string))
        .last()
        .unwrap_or_default()
}

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

fn tail(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.trim().to_string();
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", text[start..].trim())
}

// ---------------------------------------------------------------- detection

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DetectedDreamer {
    pub(crate) kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) preset: Option<AppPreset>,
    pub(crate) label: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) program: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) base_url: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) models: Vec<String>,
    pub(crate) isolation: Isolation,
    pub(crate) found: bool,
    /// The house's default dreamer, offered first in setup: Kimi Code
    /// (Eschaton, 2026-10-02). A suggestion only; nothing runs unchosen.
    pub(crate) recommended: bool,
}

/// What this machine has: the known apps, and local OpenAI-compatible
/// servers that answer /models within a second.
pub(crate) async fn detect_dreamers() -> Vec<DetectedDreamer> {
    let mut found = Vec::new();
    for (preset, label) in [
        (AppPreset::Kimi, "Kimi Code"),
        (AppPreset::Claude, "Claude Code"),
        (AppPreset::Codex, "Codex"),
    ] {
        let program = preset_program_name(preset).and_then(find_program);
        found.push(DetectedDreamer {
            kind: "app",
            preset: Some(preset),
            label,
            found: program.is_some(),
            program: program.map(|p| p.display().to_string()),
            base_url: None,
            models: Vec::new(),
            isolation: preset_isolation(preset),
            recommended: preset == AppPreset::Kimi,
        });
    }
    let client = reqwest::Client::builder().timeout(Duration::from_secs(1)).build();
    for (label, base) in [
        ("Ollama", "http://localhost:11434/v1"),
        ("LM Studio", "http://localhost:1234/v1"),
        ("llama-server", "http://localhost:8080/v1"),
    ] {
        let models = match &client {
            Ok(client) => match client.get(format!("{base}/models")).send().await {
                Ok(response) if response.status().is_success() => Some(
                    read_body_bounded(response, 64 * 1024)
                        .await
                        .ok()
                        .and_then(|(text, _)| serde_json::from_str::<Value>(&text).ok())
                        .and_then(|v| v.get("data").and_then(Value::as_array).cloned())
                        .map(|data| data.iter().filter_map(|m| m.get("id").and_then(Value::as_str).map(str::to_string)).collect())
                        .unwrap_or_default(),
                ),
                _ => None,
            },
            Err(_) => None,
        };
        found.push(DetectedDreamer {
            kind: "endpoint",
            preset: None,
            label,
            program: None,
            base_url: Some(base.to_string()),
            found: models.is_some(),
            models: models.unwrap_or_default(),
            isolation: Isolation::NotApplicable,
            recommended: false,
        });
    }
    found
}

// ---------------------------------------------------------------- test call

pub(crate) fn test_request() -> DreamRequest {
    DreamRequest {
        system: "You are Garden's dreamer, answering a connection test. Answer only with the JSON object asked for.".into(),
        user: "Reply with only this JSON object, filling in the narrative with one sentence about a garden at night: {\"type\":\"scene\",\"narrative\":\"...\",\"confidence\":0.5}".into(),
    }
}

/// A test passes when the reply carries a JSON object with a string narrative.
pub(crate) fn test_passed(reply: &DreamReply) -> bool {
    reply.receipt.ok
        && reply
            .json
            .as_ref()
            .and_then(|json| json.get("narrative"))
            .is_some_and(Value::is_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_is_found_inside_fences_preamble_and_braces_in_strings() {
        let fenced = "```json\n{\"type\":\"scene\",\"narrative\":\"a {curly} night\",\"confidence\":0.5}\n```";
        assert_eq!(extract_json_object(fenced, None).unwrap()["narrative"], "a {curly} night");
        let preamble = "Here it is: {not json} then {\"ok\":true}";
        assert_eq!(extract_json_object(preamble, None).unwrap(), json!({"ok": true}));
        let escaped = r#"{"narrative":"she said \"}\" twice"}"#;
        assert_eq!(extract_json_object(escaped, None).unwrap()["narrative"], "she said \"}\" twice");
        assert!(extract_json_object("no object here", None).is_none());
        assert!(extract_json_object("[1,2]", None).is_none());
        // The preferred object wins over an earlier placeholder.
        let placeholder = r#"Use {} placeholders. {"type":"scene","narrative":"the real one"}"#;
        assert_eq!(extract_json_object(placeholder, Some("narrative")).unwrap()["narrative"], "the real one");
        assert_eq!(extract_json_object("only {} here", Some("narrative")).unwrap(), json!({}));
        // A long run of braces is bounded, not quadratic.
        let hostile = "{".repeat(MAX_STDOUT_BYTES);
        let started = Instant::now();
        assert!(extract_json_object(&hostile, Some("narrative")).is_none());
        assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
    }

    #[test]
    fn kimi_stream_takes_the_last_assistant_message() {
        // Shape recorded from kimi 2.1.1 --output-format stream-json, 2026-10-02.
        let stream = concat!(
            "{\"role\":\"meta\",\"type\":\"system.version\",\"version\":\"2.1.1\"}\n",
            "{\"role\":\"assistant\",\"content\":\"{\\\"type\\\":\\\"scene\\\",\\\"narrative\\\":\\\"a test\\\",\\\"confidence\\\":0.5}\"}\n",
            "{\"role\":\"meta\",\"type\":\"session.resume_hint\",\"session_id\":\"s\"}\n",
        );
        let text = last_assistant_message(stream);
        assert_eq!(extract_json_object(&text, Some("narrative")).unwrap()["narrative"], "a test");
    }

    #[test]
    fn kimi_dream_config_keeps_login_and_models_and_drops_the_rest() {
        // The shapes a line filter breaks on: dotted top-level keys, inline
        // tables and arrays at top level, arrays of arrays and multi-line
        // strings inside kept tables, spaced and quoted headers.
        let user = r#"default_model = "kimi-code/k3"
thinking.enabled = true
hooks = { on_start = "x" }
mcp_servers = ["a", "b"]

[ providers."managed:kimi-code" ]
type = "kimi"
base_url = "https://api.kimi.ai/coding/v1"
matrix = [
  [1, 2],
  [3, 4],
]
note = """
[not a header]
"""

[providers."managed:kimi-code".oauth]
storage = "file"

[models."kimi-code/k3"]
model = "k3"
default_effort = "high"

[[permission.rules]]
reason = "Garden graph"

[[hooks_list]]
command = "something"

[services.moonshot_search]
url = "x"
"#;
        let out = kimi_dream_config(user).unwrap();
        let parsed: toml::Table = out.parse().expect("valid TOML");
        let keys: Vec<&str> = parsed.keys().map(String::as_str).collect();
        assert_eq!(keys.len(), 4, "{keys:?}");
        for key in ["default_model", "providers", "models", "thinking"] {
            assert!(parsed.contains_key(key), "{key} kept");
        }
        let provider = &parsed["providers"]["managed:kimi-code"];
        assert_eq!(provider["base_url"].as_str(), Some("https://api.kimi.ai/coding/v1"));
        assert_eq!(provider["matrix"].as_array().unwrap().len(), 2);
        assert_eq!(provider["note"].as_str(), Some("[not a header]\n"));
        assert_eq!(provider["oauth"]["storage"].as_str(), Some("file"));
        assert_eq!(parsed["models"]["kimi-code/k3"]["default_effort"].as_str(), Some("low"));
        assert_eq!(parsed["thinking"]["enabled"].as_bool(), Some(false));
        assert_eq!(parsed["thinking"]["effort"].as_str(), Some("low"));
        assert!(kimi_dream_config("not = [valid").is_err());
    }

    #[test]
    fn config_round_trips_and_validates() {
        let app = DreamerConfig::App {
            preset: AppPreset::Kimi,
            program: None,
            model: None,
            args: vec![],
            accept_reduced_isolation: false,
        };
        let text = serde_json::to_value(&app).unwrap();
        assert_eq!(text, json!({"kind": "app", "preset": "kimi", "acceptReducedIsolation": false}));
        assert_eq!(serde_json::from_value::<DreamerConfig>(text).unwrap(), app);
        let endpoint_json = json!({"kind":"endpoint","baseUrl":"http://localhost:11434/v1","model":"llama3.2:3b"});
        let parsed: DreamerConfig = serde_json::from_value(endpoint_json).unwrap();
        assert!(validate_config(&parsed).is_ok());
        let endpoint = |url: &str, model: &str, provider: Option<&str>| DreamerConfig::Endpoint {
            base_url: url.into(),
            model: model.into(),
            api_key_provider: provider.map(str::to_string),
        };
        assert!(validate_config(&endpoint("file:///etc", "m", None)).is_err());
        assert!(validate_config(&endpoint("http://x", " ", None)).is_err());
        // A stored key goes only to its own provider's host, over https.
        assert!(validate_config(&endpoint("https://openrouter.ai/api/v1", "m", Some("openrouter"))).is_ok());
        assert!(validate_config(&endpoint("https://evil.example/v1", "m", Some("openrouter"))).is_err());
        assert!(validate_config(&endpoint("http://openrouter.ai/api/v1", "m", Some("openrouter"))).is_err());
        assert!(validate_config(&endpoint("https://api.openai.com/v1", "m", Some("unknown"))).is_err());
        let custom = DreamerConfig::App { preset: AppPreset::Custom, program: None, model: None, args: vec![], accept_reduced_isolation: true };
        assert!(validate_config(&custom).is_err());
        // Reduced isolation must be accepted before the dreamer is saved.
        let codex = |accept: bool| DreamerConfig::App { preset: AppPreset::Codex, program: None, model: None, args: vec![], accept_reduced_isolation: accept };
        assert!(validate_config(&codex(false)).is_err());
        assert!(validate_config(&codex(true)).is_ok());
        assert!(validate_config(&app).is_ok());
        assert_eq!(endpoint_network("http://localhost:11434/v1"), "local");
        assert_eq!(endpoint_network("http://127.0.0.1:1234/v1"), "local");
        assert_eq!(endpoint_network("https://openrouter.ai/api/v1"), "remote");
    }

    fn sh_env(dir: &Path) -> Vec<(String, String)> {
        child_env(Path::new("/bin/sh"), dir, &[])
    }

    fn alive(pid: i32) -> bool {
        process::Command::new("/bin/kill").args(["-0", &pid.to_string()]).stderr(process::Stdio::null()).status().unwrap().success()
    }

    #[cfg(unix)]
    #[test]
    fn runs_are_bounded_isolated_and_killable() {
        let cancel = AtomicBool::new(false);
        let dir = tempdir("dreamer-test").unwrap();
        let sh = Path::new("/bin/sh");
        let env = sh_env(&dir);
        // The environment is cleared: a secret in ours never reaches the child.
        std::env::set_var("GARDEN_DREAMER_TEST_SECRET", "leak");
        let out = run_bounded(sh, &["-c".into(), "printf '%s|%s' \"$GARDEN_DREAMER_TEST_SECRET\" \"$(pwd -P)\"; cat".into()], Some("stdin-ok"), &dir, &env, &cancel, Duration::from_secs(10)).unwrap();
        let text = String::from_utf8(out.stdout).unwrap();
        assert_eq!(text, format!("|{}stdin-ok", dir.canonicalize().unwrap().display()));
        assert_eq!(out.end, RunEnd::Exited(Some(0)));
        // Output beyond the bound is drained and dropped, not buffered.
        let big = run_bounded(sh, &["-c".into(), "head -c 400000 /dev/zero".into()], None, &dir, &env, &cancel, Duration::from_secs(10)).unwrap();
        assert_eq!(big.stdout.len(), MAX_STDOUT_BYTES);
        assert!(big.stdout_truncated);
        // A timeout kills the whole process group, children included.
        let pidfile = dir.join("grandchild.pid");
        let started = Instant::now();
        let slow = run_bounded(sh, &["-c".into(), format!("sleep 30 & echo $! > {}; wait", pidfile.display())], None, &dir, &env, &cancel, Duration::from_secs(1)).unwrap();
        assert_eq!(slow.end, RunEnd::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(10));
        let grandchild: i32 = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
        thread::sleep(Duration::from_millis(200));
        assert!(!alive(grandchild), "the grandchild must die with the group");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_leader_that_exits_early_leaves_no_sleeper_and_no_wait() {
        // The angel's repro: the leader exits at once, a background child in
        // its group holds stdout open. We must not wait for it, and it dies.
        let cancel = AtomicBool::new(false);
        let dir = tempdir("dreamer-sleeper").unwrap();
        let pidfile = dir.join("sleeper.pid");
        let started = Instant::now();
        let out = run_bounded(
            Path::new("/bin/sh"),
            &["-c".into(), format!("(trap '' TERM; sleep 30) & echo $! > {}; echo done; exit 0", pidfile.display())],
            None, &dir, &sh_env(&dir), &cancel, Duration::from_secs(60),
        )
        .unwrap();
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
        assert_eq!(out.end, RunEnd::Exited(Some(0)));
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "done");
        let sleeper: i32 = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
        thread::sleep(Duration::from_millis(200));
        assert!(!alive(sleeper), "a SIGTERM-ignoring sleeper in the group is SIGKILLed");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn an_escaped_process_holding_the_pipe_costs_only_the_drain_grace() {
        // A grandchild that leaves the group (its own session) can't be
        // killed by the group kill; we stop waiting after DRAIN_GRACE.
        let perl = Path::new("/usr/bin/perl");
        if !perl.exists() {
            return;
        }
        let cancel = AtomicBool::new(false);
        let dir = tempdir("dreamer-escape").unwrap();
        let pidfile = dir.join("escaped.pid");
        let script = format!(
            "if (fork()) {{ print \"leader\\n\"; exit 0 }} POSIX::setsid(); open(my $f, '>', '{}'); print $f $$; close $f; sleep 20;",
            pidfile.display()
        );
        let started = Instant::now();
        let out = run_bounded(perl, &["-MPOSIX".into(), "-e".into(), script], None, &dir, &sh_env(&dir), &cancel, Duration::from_secs(60)).unwrap();
        let elapsed = started.elapsed();
        assert!(elapsed < DRAIN_GRACE + Duration::from_secs(3), "{elapsed:?}");
        assert_eq!(out.end, RunEnd::Exited(Some(0)));
        assert!(String::from_utf8_lossy(&out.stdout).contains("leader"));
        thread::sleep(Duration::from_millis(200));
        if let Ok(pid) = std::fs::read_to_string(&pidfile) {
            let _ = process::Command::new("/bin/kill").args(["-9", pid.trim()]).status();
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn relink_repairs_stale_and_dangling_links() {
        let dir = tempdir("dreamer-relink").unwrap();
        let target = dir.join("target");
        let other = dir.join("other");
        crate::storage_file_ops::write_bytes(&target, b"t").unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&other, &link).unwrap(); // dangling, wrong target
        relink(&link, &target).unwrap();
        assert_eq!(std::fs::read_link(&link).unwrap(), target);
        std::fs::remove_file(&target).unwrap();
        relink(&link, &target).unwrap();
        assert!(std::fs::symlink_metadata(&link).is_err(), "a link to nothing is removed");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A tiny HTTP server: answers every connection with `response` and
    /// counts the requests it saw.
    fn http_stub(response: String) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = seen.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                counter.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 65536];
                let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(response.as_bytes());
            }
        });
        (format!("http://{addr}"), seen)
    }

    fn always_consent() -> ConsentGate {
        Arc::new(|| Ok("test".to_string()))
    }

    #[test]
    fn a_redirect_is_never_followed() {
        let (target, target_seen) = http_stub("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}".into());
        let (source, source_seen) = http_stub(format!(
            "HTTP/1.1 307 Temporary Redirect\r\nLocation: {target}/v1/chat/completions\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        ));
        let cancel = AtomicBool::new(false);
        let outcome = crate::app_runtime::async_runtime::block_on(call_endpoint(
            &format!("{source}/v1"), "m", None, &test_request(), &always_consent(), &cancel,
            Instant::now() + Duration::from_secs(10),
        ));
        assert!(outcome.result.unwrap_err().contains("307"));
        assert_eq!(source_seen.load(Ordering::SeqCst), 1);
        thread::sleep(Duration::from_millis(200));
        assert_eq!(target_seen.load(Ordering::SeqCst), 0, "the redirect target must get no request");
    }

    #[test]
    fn consent_is_asked_before_every_send() {
        let (base, seen) = http_stub("HTTP/1.1 400 Bad Request\r\nContent-Length: 3\r\nConnection: close\r\n\r\nbad".into());
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = calls.clone();
        // Consent holds for the first send, then is withdrawn.
        let gate: ConsentGate = Arc::new(move || {
            if counted.fetch_add(1, Ordering::SeqCst) == 0 { Ok("first".into()) } else { Err("opted out".into()) }
        });
        let cancel = AtomicBool::new(false);
        let outcome = crate::app_runtime::async_runtime::block_on(call_endpoint(
            &format!("{base}/v1"), "m", None, &test_request(), &gate, &cancel,
            Instant::now() + Duration::from_secs(10),
        ));
        assert_eq!(outcome.result.unwrap_err(), "opted out");
        assert_eq!(outcome.attempts, 1);
        assert_eq!(seen.load(Ordering::SeqCst), 1, "the retry after opt-out must not be sent");
    }

    #[cfg(unix)]
    #[test]
    fn a_cancelled_call_never_starts_the_app() {
        let dir = tempdir("dreamer-precancel").unwrap();
        let sentinel = dir.join("ran");
        let config = DreamerConfig::App {
            preset: AppPreset::Custom,
            program: Some("/bin/sh".into()),
            model: None,
            args: vec!["-c".into(), format!("touch {}", sentinel.display())],
            accept_reduced_isolation: true,
        };
        let app = crate::tauri_runtime::build_mock_app_for_tests(false);
        let reply = crate::app_runtime::async_runtime::block_on(call_dreamer(
            &app, &config, &test_request(), always_consent(), Arc::new(AtomicBool::new(true)), Duration::from_secs(10),
        ));
        assert_eq!(reply.receipt.ended, "cancelled");
        assert!(!reply.receipt.ok);
        thread::sleep(Duration::from_millis(200));
        assert!(!sentinel.exists(), "a pre-cancelled call must not run the app");
        // And with no time left, it doesn't start either.
        let reply = crate::app_runtime::async_runtime::block_on(call_dreamer(
            &app, &config, &test_request(), always_consent(), Arc::new(AtomicBool::new(false)), Duration::ZERO,
        ));
        assert_eq!(reply.receipt.ended, "timed-out");
        assert!(!sentinel.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn reduced_isolation_presets_are_marked() {
        assert_eq!(preset_isolation(AppPreset::Kimi), Isolation::Isolated);
        assert_eq!(preset_isolation(AppPreset::Claude), Isolation::Isolated);
        assert_eq!(preset_isolation(AppPreset::Codex), Isolation::Sandboxed);
        assert_eq!(preset_isolation(AppPreset::Custom), Isolation::UserResponsibility);
    }

    /// Opt-in: calls the real AI apps and servers on this machine.
    /// GARDEN_DREAMER_LIVE=kimi,claude,codex,ollama (any subset).
    #[test]
    #[ignore = "set GARDEN_DREAMER_LIVE to call real dreamers"]
    fn live_dreamers_answer_the_test_prompt() {
        let wanted = std::env::var("GARDEN_DREAMER_LIVE").unwrap_or_default();
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile = tempdir("dreamer-live-profile").unwrap();
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);
        let app = crate::tauri_runtime::build_mock_app_for_tests(false);
        let app_dreamer = |preset, model: Option<&str>| DreamerConfig::App {
            preset,
            program: None,
            model: model.map(str::to_string),
            args: vec![],
            accept_reduced_isolation: preset == AppPreset::Codex,
        };
        let mut configs = Vec::new();
        for name in wanted.split(',').map(str::trim) {
            match name {
                "kimi" => configs.push(app_dreamer(AppPreset::Kimi, None)),
                "claude" => configs.push(app_dreamer(AppPreset::Claude, Some("haiku"))),
                "codex" => configs.push(app_dreamer(AppPreset::Codex, None)),
                "ollama" => configs.push(DreamerConfig::Endpoint {
                    base_url: "http://localhost:11434/v1".into(),
                    model: std::env::var("GARDEN_DREAMER_OLLAMA_MODEL").unwrap_or("llama3.2:3b".into()),
                    api_key_provider: None,
                }),
                _ => {}
            }
        }
        let mut failures = Vec::new();
        for config in configs {
            let reply = crate::app_runtime::async_runtime::block_on(call_dreamer(
                &app,
                &config,
                &test_request(),
                Arc::new(|| Ok("live test".to_string())),
                Arc::new(AtomicBool::new(false)),
                DEFAULT_TIMEOUT,
            ));
            eprintln!("{}", serde_json::to_string_pretty(&reply).unwrap());
            if !test_passed(&reply) {
                failures.push(format!("{config:?}"));
            }
        }
        if wanted.contains("kimi") {
            assert!(!profile.join("dreamer/kimi-home/sessions").exists(), "Kimi sessions are removed after the run");
        }
        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(profile);
        assert!(failures.is_empty(), "failed: {failures:?}");
    }
}
