//! The native menu bar, built from a spec the frontend sends.
//!
//! The frontend owns the commands (its CommandRegistry feeds the palette, the
//! shortcuts dialog and this menu), so the menu's items, shortcuts and
//! enabled/checked states come from there, and a click on one of them comes
//! back as an `app-menu` event naming the command. The shell keeps only what
//! macOS itself must provide: the app menu's About/Services/Hide/Quit, the
//! clipboard and undo items (in a WKWebView those run through the Edit menu),
//! full screen and the Window menu.
//!
//! Until the frontend sends its first spec the app shows `default_app_menu`:
//! Tauri's default menu minus Edit > Select All (see there).

use serde::Serialize;
use tauri::menu::{CheckMenuItemBuilder, Menu, MenuItemBuilder, Submenu, SubmenuBuilder};
use tauri::{AppHandle, Emitter, Runtime};

use crate::app_menu_spec::{AppMenuItemSpec, AppMenuSpec, APP_MENU_EVENT, COMMAND_ID_PREFIX};

#[derive(Clone, Serialize)]
struct AppMenuEventPayload {
    id: String,
}

/// Tauri's default macOS menu, minus Edit > Select All.
///
/// WHY. On macOS Tauri installs a default app menu whenever the app does not
/// set one, and its Edit submenu binds Select All to Cmd+A. macOS gives menu
/// key equivalents first refusal, so the menu consumed every Cmd+A and ran
/// WebKit's NATIVE select-all on the whole web view. The editor never saw the
/// keypress, so ProseMirror's own select-all never ran. Reported as "cmd+a
/// doesn't select all"; the same keypress works in every browser, because a
/// browser tab has no app menu between the key and the page.
///
/// The frontend's menu replaces this once it loads; its Select All runs the
/// editor's own select-all.
pub(crate) fn default_app_menu<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<Menu<R>> {
    let menu = Menu::default(app)?;
    for item in menu.items()? {
        let Some(submenu) = item.as_submenu() else { continue };
        for child in submenu.items()? {
            let Some(predefined) = child.as_predefined_menuitem() else { continue };
            if predefined.text()?.eq_ignore_ascii_case("select all") {
                submenu.remove(predefined)?;
            }
        }
    }
    Ok(menu)
}

fn push_item<'a, R: Runtime>(
    app: &'a AppHandle<R>,
    builder: SubmenuBuilder<'a, R, AppHandle<R>>,
    item: &AppMenuItemSpec,
) -> Result<SubmenuBuilder<'a, R, AppHandle<R>>, String> {
    Ok(match item {
        AppMenuItemSpec::Separator => builder.separator(),
        AppMenuItemSpec::Native { item } => match item.as_str() {
            "undo" => builder.undo(),
            "redo" => builder.redo(),
            "cut" => builder.cut(),
            "copy" => builder.copy(),
            "paste" => builder.paste(),
            "fullscreen" => builder.fullscreen(),
            "minimize" => builder.minimize(),
            "maximize" => builder.maximize(),
            "closeWindow" => builder.close_window(),
            other => return Err(format!("unknown native menu item: {other}")),
        },
        AppMenuItemSpec::Submenu { label, items } => {
            let nested = build_submenu(app, label, items, false)?;
            builder.item(&nested)
        }
        AppMenuItemSpec::Command { id, label, accelerator, enabled, checked } => {
            let menu_id = format!("{COMMAND_ID_PREFIX}{id}");
            match checked {
                Some(checked) => {
                    let mut check = CheckMenuItemBuilder::with_id(menu_id, label)
                        .enabled(*enabled)
                        .checked(*checked);
                    if let Some(accelerator) = accelerator {
                        check = check.accelerator(accelerator);
                    }
                    builder.item(&check.build(app).map_err(|e| e.to_string())?)
                }
                None => {
                    let mut plain = MenuItemBuilder::with_id(menu_id, label).enabled(*enabled);
                    if let Some(accelerator) = accelerator {
                        plain = plain.accelerator(accelerator);
                    }
                    builder.item(&plain.build(app).map_err(|e| e.to_string())?)
                }
            }
        }
    })
}

fn build_submenu<R: Runtime>(
    app: &AppHandle<R>,
    title: &str,
    items: &[AppMenuItemSpec],
    window_items: bool,
) -> Result<Submenu<R>, String> {
    let mut builder = SubmenuBuilder::new(app, title);
    for item in items {
        builder = push_item(app, builder, item)?;
    }
    if window_items {
        if !items.is_empty() {
            builder = builder.separator();
        }
        builder = builder.minimize().maximize().separator().close_window();
    }
    builder.build().map_err(|e| e.to_string())
}

/// Build the whole menu bar from `spec`.
pub(crate) fn build_app_menu<R: Runtime>(
    app: &AppHandle<R>,
    spec: &AppMenuSpec,
) -> Result<Menu<R>, String> {
    let app_name = app.package_info().name.clone();
    let mut app_builder = SubmenuBuilder::new(app, &app_name).about(None).separator();
    for item in &spec.app_items {
        app_builder = push_item(app, app_builder, item)?;
    }
    if !spec.app_items.is_empty() {
        app_builder = app_builder.separator();
    }
    let app_submenu = app_builder
        .services()
        .separator()
        .hide()
        .hide_others()
        .show_all()
        .separator()
        .quit()
        .build()
        .map_err(|e| e.to_string())?;

    let menu = Menu::new(app).map_err(|e| e.to_string())?;
    menu.append(&app_submenu).map_err(|e| e.to_string())?;
    let mut has_window_menu = false;
    for submenu in &spec.menus {
        let is_window = submenu.title == "Window";
        has_window_menu |= is_window;
        let built = build_submenu(app, &submenu.title, &submenu.items, is_window)?;
        menu.append(&built).map_err(|e| e.to_string())?;
    }
    if !has_window_menu {
        let window = build_submenu(app, "Window", &[], true)?;
        menu.append(&window).map_err(|e| e.to_string())?;
    }
    Ok(menu)
}

/// Replace the menu bar with the frontend's commands. Synchronous on purpose:
/// Tauri runs synchronous commands on the main thread, where AppKit menus
/// must be built.
#[tauri::command]
pub(crate) fn set_app_menu(app: AppHandle, spec: AppMenuSpec) -> Result<(), String> {
    let result = build_app_menu(&app, &spec)
        .and_then(|menu| app.set_menu(menu).map(|_| ()).map_err(|e| e.to_string()));
    match &result {
        Ok(()) => log::info!("app menu set: {} menus", spec.menus.len()),
        Err(error) => log::warn!("app menu refused: {error}"),
    }
    result
}

/// Forward a click on a frontend command to the webview as `app-menu`.
pub(crate) fn handle_menu_event<R: Runtime>(app: &AppHandle<R>, event: tauri::menu::MenuEvent) {
    let Some(id) = event.id().as_ref().strip_prefix(COMMAND_ID_PREFIX) else { return };
    if let Err(error) = app.emit(APP_MENU_EVENT, AppMenuEventPayload { id: id.to_string() }) {
        log::warn!("app menu event {id} was not delivered: {error}");
    }
}
