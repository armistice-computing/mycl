use crate::terminal::{TerminalEvent, TerminalFrameSink, TerminalManager};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Serialize;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tauri::{ipc::Channel, State, WebviewWindow};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub(crate) enum TerminalFrame {
    Data { session: String, bytes: String },
    Exit { session: String, code: i32 },
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TerminalOpenResponse {
    session: String,
    generation: u64,
    replay: String,
}

/// True while the "Turn on the terminal?" dialog is on screen.
static ASKING: AtomicBool = AtomicBool::new(false);

fn require_main(window: &WebviewWindow) -> Result<(), String> {
    if window.label() == "main" {
        Ok(())
    } else {
        Err("terminal commands are available only in the main window".to_string())
    }
}

#[tauri::command]
pub(crate) fn terminal_settings_get(
    window: WebviewWindow,
    manager: State<'_, TerminalManager>,
) -> Result<bool, String> {
    require_main(&window)?;
    Ok(manager.enabled())
}

/// Turning the terminal off needs no question. Turning it on is asked of
/// the person in a native dialog, which nothing running in the page can
/// answer: a shell is the one thing here a page script must not be able to
/// give itself.
#[tauri::command]
pub(crate) async fn terminal_settings_set(
    window: WebviewWindow,
    manager: State<'_, TerminalManager>,
    enabled: bool,
) -> Result<(), String> {
    require_main(&window)?;
    let manager = manager.inner().clone();
    if !enabled || manager.enabled() {
        return manager.set_enabled(enabled);
    }
    if ASKING.swap(true, Ordering::SeqCst) {
        return Err("The question is already on screen.".to_string());
    }
    tauri::async_runtime::spawn_blocking(move || {
        // Cleared however this ends, a panic in the dialog included: a flag
        // left set would keep the terminal from ever being turned on.
        struct Asked;
        impl Drop for Asked {
            fn drop(&mut self) {
                ASKING.store(false, Ordering::SeqCst);
            }
        }
        let _asked = Asked;
        let allowed = window
            .dialog()
            .message(
                "A terminal runs commands on this computer as you, with access to your files. \
                 Nothing starts until you open it, and you can turn it off again in Settings.",
            )
            .title("Turn on the terminal?")
            .kind(MessageDialogKind::Warning)
            .buttons(MessageDialogButtons::OkCancelCustom(
                "Turn On".to_string(),
                "Cancel".to_string(),
            ))
            .parent(&window)
            .blocking_show();
        if !allowed {
            return Err("The terminal was not turned on.".to_string());
        }
        manager.set_enabled(true)
    })
    .await
    .map_err(|error| format!("terminal settings: {error}"))?
}

#[tauri::command]
pub(crate) async fn terminal_open(
    window: WebviewWindow,
    manager: State<'_, TerminalManager>,
    cols: u16,
    rows: u16,
    output: Channel<TerminalFrame>,
) -> Result<TerminalOpenResponse, String> {
    require_main(&window)?;
    let manager = manager.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let sink: TerminalFrameSink = Arc::new(move |session, event, bytes| {
            let frame = match event {
                TerminalEvent::Data => TerminalFrame::Data {
                    session: session.to_string(),
                    bytes: STANDARD.encode(bytes),
                },
                TerminalEvent::Exit(code) => TerminalFrame::Exit {
                    session: session.to_string(),
                    code,
                },
            };
            let _ = output.send(frame);
        });
        let opened = manager.open(cols, rows, sink)?;
        Ok(TerminalOpenResponse {
            session: opened.session,
            generation: opened.generation,
            replay: STANDARD.encode(opened.replay),
        })
    })
    .await
    .map_err(|error| format!("open terminal: {error}"))?
}

#[tauri::command]
pub(crate) async fn terminal_write(
    window: WebviewWindow,
    manager: State<'_, TerminalManager>,
    session: String,
    data: String,
) -> Result<(), String> {
    require_main(&window)?;
    let manager = manager.inner().clone();
    tauri::async_runtime::spawn_blocking(move || manager.write(&session, &data))
        .await
        .map_err(|error| format!("write terminal: {error}"))?
}

#[tauri::command]
pub(crate) async fn terminal_resize(
    window: WebviewWindow,
    manager: State<'_, TerminalManager>,
    session: String,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    require_main(&window)?;
    let manager = manager.inner().clone();
    tauri::async_runtime::spawn_blocking(move || manager.resize(&session, cols, rows))
        .await
        .map_err(|error| format!("resize terminal: {error}"))?
}

#[tauri::command]
pub(crate) async fn terminal_close(
    window: WebviewWindow,
    manager: State<'_, TerminalManager>,
    session: String,
) -> Result<(), String> {
    require_main(&window)?;
    let manager = manager.inner().clone();
    tauri::async_runtime::spawn_blocking(move || manager.close(&session))
        .await
        .map_err(|error| format!("close terminal: {error}"))?
}
