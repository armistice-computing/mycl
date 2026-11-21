//! Desktop-only owner controls. No request parameter can name a source path.
use crate::{app_runtime::AppHandle, vault_runtime::VaultRuntime, vault_selection};
use serde_json::{json, Value};
use std::sync::Arc;
use tauri::{Manager, WebviewWindow};

fn require_main(window: &WebviewWindow) -> Result<(), String> {
    if window.label() != "main" {
        return Err("vault selection is available only in the main app window".into());
    }
    Ok(())
}

#[tauri::command]
pub(crate) async fn select_vault_source(
    app: AppHandle,
    window: WebviewWindow,
    graph_id: String,
    kind: String,
) -> Result<Value, String> {
    require_main(&window)?;
    let kind = vault_selection::kind(&kind)?;
    app.state::<Arc<VaultRuntime>>().store()?;
    let binding = vault_selection::owner_binding(&app, &graph_id)?;
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (kind, binding);
        return Err("native vault selection is currently available on macOS and Linux only".into());
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let picker = app.state::<Arc<crate::vault_ui::Runtime>>().picker.clone();
        let (_request_guard, permit) = picker.enter()?;
        let (send, receive) = tokio::sync::oneshot::channel();
        let title = match kind {
            crate::vault_capture::SourceKind::Folder => {
                "Choose a folder to import into Mycl as a new graph"
            }
            crate::vault_capture::SourceKind::Zip => {
                "Choose a ZIP to import into Mycl as a new graph"
            }
        };
        let picker_app = app.clone();
        let picker_window = window.clone();
        window
            .run_on_main_thread(move || {
                use tauri_plugin_dialog::DialogExt;
                if send.is_closed() {
                    return;
                }
                let dialog = picker_app
                    .dialog()
                    .file()
                    .set_parent(&picker_window)
                    .set_title(title)
                    .set_can_create_directories(false);
                let chosen = move |selected: Option<tauri_plugin_dialog::FilePath>| {
                    let _permit = permit;
                    let result = selected
                        .map(|p| {
                            p.into_path()
                                .map_err(|_| "Source must be a local file or folder".to_owned())
                        })
                        .transpose();
                    let _ = send.send(result);
                };
                match kind {
                    crate::vault_capture::SourceKind::Folder => dialog.pick_folder(chosen),
                    crate::vault_capture::SourceKind::Zip => {
                        dialog.add_filter("Vault ZIP", &["zip"]).pick_file(chosen)
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        let selected = tokio::time::timeout(std::time::Duration::from_secs(600), receive)
            .await
            .map_err(|_| "The chooser timed out. Close any open chooser and try again.")?
            .map_err(|_| "source chooser closed without a result")??;
        let Some(path) = selected else {
            return Ok(json!({"state":"cancelled","graphWrites":false}));
        };
        let profile = crate::profile_paths::profile_dir(&app)?;
        let source = crate::app_runtime::async_runtime::spawn_blocking(move || {
            vault_selection::open_selected_source(&path, &profile, kind)
        })
        .await
        .map_err(|e| e.to_string())??;
        let result =
            vault_selection::start_capture(app.clone(), binding.clone(), source, kind).await?;
        if let Some(scan) = result.get("scanId").and_then(Value::as_str) {
            app.state::<Arc<crate::vault_ui::Runtime>>()
                .record_chooser_selection(binding, scan.to_owned())?;
        }
        Ok(result)
    }
}

#[tauri::command]
pub(crate) async fn get_vault_capture_status(
    app: AppHandle,
    window: WebviewWindow,
    graph_id: String,
    scan_id: String,
    offset: Option<usize>,
    limit: Option<usize>,
) -> Result<Value, String> {
    require_main(&window)?;
    crate::app_runtime::async_runtime::spawn_blocking(move || {
        vault_selection::status(
            &app,
            &graph_id,
            &scan_id,
            offset.unwrap_or(0),
            limit.unwrap_or(50),
        )
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub(crate) async fn cancel_vault_capture(
    app: AppHandle,
    window: WebviewWindow,
    graph_id: String,
    scan_id: String,
) -> Result<Value, String> {
    require_main(&window)?;
    crate::app_runtime::async_runtime::spawn_blocking(move || {
        vault_selection::cancel(&app, &graph_id, &scan_id)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub(crate) async fn review_vault_import(
    app: AppHandle,
    window: WebviewWindow,
    graph_id: String,
    scan_id: String,
    title: String,
) -> Result<Value, crate::vault_ui::Error> {
    crate::vault_ui::require_window(window.label())?;
    crate::vault_ui::review(app, graph_id, scan_id, title).await
}
#[tauri::command]
pub(crate) async fn commit_vault_import(
    app: AppHandle,
    window: WebviewWindow,
    review_id: String,
    digest: String,
) -> Result<Value, crate::vault_ui::Error> {
    crate::vault_ui::require_window(window.label())?;
    crate::vault_ui::commit(app, review_id, digest).await
}
#[tauri::command]
pub(crate) async fn get_vault_import_status(
    app: AppHandle,
    window: WebviewWindow,
    job_id: String,
) -> Result<Value, crate::vault_ui::Error> {
    crate::vault_ui::require_window(window.label())?;
    crate::vault_ui::status(app, job_id).await
}
#[tauri::command]
pub(crate) async fn cancel_vault_import(
    app: AppHandle,
    window: WebviewWindow,
    job_id: String,
) -> Result<Value, crate::vault_ui::Error> {
    crate::vault_ui::require_window(window.label())?;
    crate::vault_ui::cancel(app, job_id).await
}
#[tauri::command]
pub(crate) async fn release_vault_review(
    app: AppHandle,
    window: WebviewWindow,
    review_id: String,
) -> Result<Value, crate::vault_ui::Error> {
    crate::vault_ui::require_window(window.label())?;
    crate::vault_ui::release(&app, &review_id)
}
#[tauri::command]
pub(crate) async fn list_vault_import_jobs(
    app: AppHandle,
    window: WebviewWindow,
    limit: Option<usize>,
) -> Result<Value, crate::vault_ui::Error> {
    crate::vault_ui::require_window(window.label())?;
    crate::vault_ui::recent(app, limit.unwrap_or(20)).await
}
