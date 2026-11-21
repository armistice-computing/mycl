use crate::{
    profile_paths::profile_dir,
    window_geometry::{DisplayBounds, WindowGeometry, WINDOW_MIN_HEIGHT, WINDOW_MIN_WIDTH},
    storage::{create_dir_all, read_json, write_json},
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tauri::{App, AppHandle, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

const MAIN_WINDOW_LABEL: &str = "main";
const WINDOW_TITLE: &str = "Mycl";
const WINDOW_WIDTH: f64 = 1180.0;
const WINDOW_HEIGHT: f64 = 760.0;

/// Where the window controls sit in the `Minimal` title bar, in points from
/// the window's top-left corner: x is the close button's left edge, y the
/// offset tao uses to size the title-bar strip. Measured against the
/// frontend's 40px top bar so the controls share its centerline, as a Mac
/// app's unified toolbar does.
#[cfg(target_os = "macos")]
const TRAFFIC_LIGHTS: (f64, f64) = (18.0, 18.0);

/// How the native window's title bar is presented.
///
/// - `Full`: standard native title bar.
/// - `Minimal`: macOS "overlay" title bar - the window controls
///   (close/minimize/maximize) stay and float over the content, which extends
///   to the top edge, but the title bar chrome and title are hidden. The strip
///   stays draggable. On non-macOS this falls back to `Full`.
/// - `None`: fully frameless - no title bar and no window controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum TitleBarMode {
    Full,
    Minimal,
    None,
}

impl Default for TitleBarMode {
    /// On macOS the top bar sits in the title bar by default (her call,
    /// 2026-09-30: it makes Garden look like a Mac app). View > Toolbar in
    /// Title Bar turns it off, and that choice persists. Elsewhere `Minimal`
    /// falls back to `Full` anyway.
    fn default() -> Self {
        if cfg!(target_os = "macos") {
            Self::Minimal
        } else {
            Self::Full
        }
    }
}

impl TitleBarMode {
    /// Whether the OS draws a frame (and, on macOS, the traffic-light controls).
    /// `Minimal` keeps decorations on - the controls live in the overlay bar.
    fn decorations(self) -> bool {
        !matches!(self, TitleBarMode::None)
    }

    /// Whether this mode uses the macOS transparent/overlay title-bar style.
    #[cfg(target_os = "macos")]
    fn overlay_title_bar(self) -> bool {
        matches!(self, TitleBarMode::Minimal)
    }
}

/// Persisted appearance/window preferences for the native shell. Kept as a
/// struct so future appearance prefs (theme, remembered geometry, etc.) slot in.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WindowSettings {
    #[serde(default)]
    pub(crate) title_bar: TitleBarMode,
    #[serde(default)]
    pub(crate) disable_compositing: bool,
    /// Where the window was when it was last closed or the app last quit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) geometry: Option<WindowGeometry>,
}

/// Legacy on-disk shape from the original boolean toggle, migrated forward.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyWindowSettings {
    decorations: bool,
}

fn window_settings_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(profile_dir(app)?.join("window-settings.json"))
}

pub(crate) fn read_window_settings(app: &AppHandle) -> Result<WindowSettings, String> {
    let path = window_settings_path(app)?;
    if !path.is_file() {
        return Ok(WindowSettings::default());
    }
    if let Ok(settings) = read_json::<WindowSettings>(&path) {
        return Ok(settings);
    }
    // Migrate the original `{ "decorations": bool }` file: off -> None, on -> Full.
    if let Ok(legacy) = read_json::<LegacyWindowSettings>(&path) {
        return Ok(WindowSettings {
            title_bar: if legacy.decorations {
                TitleBarMode::Full
            } else {
                TitleBarMode::None
            },
            disable_compositing: false,
            geometry: None,
        });
    }
    Ok(WindowSettings::default())
}

pub(crate) fn configure_compositing(settings: &WindowSettings) {
    #[cfg(target_os = "linux")]
    if settings.disable_compositing {
        std::env::set_var("WEBKIT_DISABLE_COMPOSITING_MODE", "1");
    }
    #[cfg(not(target_os = "linux"))]
    let _ = settings;
}

fn write_window_settings(app: &AppHandle, settings: &WindowSettings) -> Result<(), String> {
    let dir = profile_dir(app)?;
    create_dir_all(&dir)?;
    write_json(&window_settings_path(app)?, settings).map_err(Into::into)
}

/// Create the main window in code (rather than statically in tauri.conf.json)
/// so the macOS title-bar style can vary by the persisted preference. The
/// creation-time style avoids a first-frame flash; runtime switches go through
/// [`apply_title_bar_mode`].
pub(crate) fn build_main_window(app: &App, settings: &WindowSettings) -> Result<(), String> {
    let mode = settings.title_bar;
    let geometry = settings
        .geometry
        .and_then(|geometry| geometry.restorable_on(&connected_displays(app.handle())));
    #[cfg(target_os = "macos")]
    {
        enable_continuous_spellcheck();
        keep_node_awake_while_hidden();
    }
    let builder = WebviewWindowBuilder::new(app, MAIN_WINDOW_LABEL, WebviewUrl::default())
        .title(WINDOW_TITLE)
        .min_inner_size(WINDOW_MIN_WIDTH, WINDOW_MIN_HEIGHT)
        .resizable(true)
        // Tauri's own drop handler takes a Finder drop before the page sees
        // it. The page handles drops itself (import, attach, block drag).
        .disable_drag_drop_handler()
        // A file dropped on a part of the page that does not take drops is
        // WebKit's to handle, and WebKit opens it in place of the app.
        .on_navigation(|url| url.scheme() != "file")
        .decorations(mode.decorations());
    let builder = match geometry {
        Some(geometry) => builder
            .inner_size(geometry.width, geometry.height)
            .position(geometry.x, geometry.y),
        None => builder.inner_size(WINDOW_WIDTH, WINDOW_HEIGHT),
    };

    #[cfg(target_os = "macos")]
    let builder = {
        use tauri::TitleBarStyle;

        if mode.overlay_title_bar() {
            builder
                .title_bar_style(TitleBarStyle::Overlay)
                .hidden_title(true)
                .traffic_light_position(tauri::LogicalPosition::new(
                    TRAFFIC_LIGHTS.0,
                    TRAFFIC_LIGHTS.1,
                ))
        } else {
            builder.title_bar_style(TitleBarStyle::Visible)
        }
    };

    let window = builder
        .build()
        .map_err(|error| format!("build main window: {error}"))?;

    #[cfg(target_os = "macos")]
    lift_webkit_60fps_cap(&window);

    // Closing the window must not end the app: the app is also the local
    // node, and other programs are connected to it. On macOS the red button
    // and Cmd-W hide the window; the Dock icon brings it back; Quit quits.
    let handle = app.handle().clone();
    window.on_window_event(move |event| match event {
        tauri::WindowEvent::Moved(_) | tauri::WindowEvent::Resized(_) => {
            note_window_geometry(&handle);
        }
        tauri::WindowEvent::CloseRequested { api, .. } => {
            note_window_geometry(&handle);
            save_window_geometry(&handle);
            if cfg!(target_os = "macos") {
                api.prevent_close();
                if let Some(window) = handle.get_webview_window(MAIN_WINDOW_LABEL) {
                    // Hiding a full-screen window leaves its empty space
                    // behind; leave full screen first.
                    if window.is_fullscreen().unwrap_or(false) {
                        let _ = window.set_fullscreen(false);
                    }
                    if let Err(error) = window.hide() {
                        log::warn!("window: could not hide on close: {error}");
                    }
                }
            }
        }
        _ => {}
    });

    Ok(())
}

/// Bring the hidden main window back (a Dock click on macOS).
pub(crate) fn show_main_window(app: &AppHandle) {
    let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) else {
        return;
    };
    let shown = window
        .show()
        .and_then(|()| window.unminimize())
        .and_then(|()| window.set_focus());
    if let Err(error) = shown {
        log::warn!("window: could not show the main window: {error}");
    }
}

/// The window's last known place, kept in memory as it moves. Quit on macOS
/// ends the event loop without a close event, and by then the window cannot
/// be asked, so the value to save has to be at hand already.
static LAST_GEOMETRY: std::sync::Mutex<Option<WindowGeometry>> = std::sync::Mutex::new(None);

/// Note where the window is now. No file is written. A full-screen or
/// zoomed window is not noted: its frame is the display's, not the user's
/// choice. The position is the content area's, which is what the window
/// builder's `position` sets; the frame's would climb by one title bar on
/// each launch.
fn note_window_geometry(app: &AppHandle) {
    let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) else {
        return;
    };
    if window.is_fullscreen().unwrap_or(false) || window.is_maximized().unwrap_or(false) {
        return;
    }
    let (Ok(position), Ok(size), Ok(scale)) = (
        window.inner_position(),
        window.inner_size(),
        window.scale_factor(),
    ) else {
        return;
    };
    let position = position.to_logical::<f64>(scale);
    let size = size.to_logical::<f64>(scale);
    let geometry = WindowGeometry {
        x: position.x,
        y: position.y,
        width: size.width,
        height: size.height,
    };
    if let Ok(mut last) = LAST_GEOMETRY.lock() {
        *last = Some(geometry);
    }
}

/// Write the last noted place to the settings file, for the next launch.
/// Called when the window closes and when the app exits.
pub(crate) fn save_window_geometry(app: &AppHandle) {
    let Some(geometry) = LAST_GEOMETRY.lock().ok().and_then(|last| *last) else {
        return;
    };
    let saved = read_window_settings(app).and_then(|mut settings| {
        if settings.geometry == Some(geometry) {
            return Ok(());
        }
        settings.geometry = Some(geometry);
        write_window_settings(app, &settings)
    });
    if let Err(error) = saved {
        log::warn!("window: could not save the window's size and position: {error}");
    }
}

fn connected_displays(app: &AppHandle) -> Vec<DisplayBounds> {
    app.available_monitors()
        .unwrap_or_default()
        .iter()
        .map(|monitor| {
            let scale = monitor.scale_factor();
            let position = monitor.position().to_logical::<f64>(scale);
            let size = monitor.size().to_logical::<f64>(scale);
            DisplayBounds {
                x: position.x,
                y: position.y,
                width: size.width,
                height: size.height,
            }
        })
        .collect()
}

/// The app is the local node, and a hidden or covered app is a candidate for
/// App Nap, which slows timers and lowers priority: connected programs would
/// wait on a napping node. One activity, held for the life of the process,
/// says the work is wanted. It does not stop the display or the Mac from
/// sleeping.
#[cfg(target_os = "macos")]
fn keep_node_awake_while_hidden() {
    use objc2::msg_send;
    use objc2::runtime::{AnyClass, AnyObject};

    // NSActivityUserInitiatedAllowingIdleSystemSleep
    const ACTIVITY_OPTIONS: u64 = 0x00FF_FFFF & !(1 << 20);
    let (Some(process_info_class), Some(string_class)) =
        (AnyClass::get(c"NSProcessInfo"), AnyClass::get(c"NSString"))
    else {
        return;
    };
    // SAFETY: public Foundation API. The returned activity object is
    // retained and never released, so the activity lasts until exit.
    unsafe {
        let process_info: *mut AnyObject = msg_send![process_info_class, processInfo];
        let reason: *mut AnyObject = msg_send![
            string_class,
            stringWithUTF8String: c"Serving the local graph to connected programs".as_ptr()
        ];
        if process_info.is_null() || reason.is_null() {
            return;
        }
        let activity: *mut AnyObject = msg_send![
            process_info,
            beginActivityWithOptions: ACTIVITY_OPTIONS,
            reason: reason
        ];
        if !activity.is_null() {
            let _: *mut AnyObject = msg_send![activity, retain];
        }
    }
}

/// Red underlines while typing. A WKWebView reads this from the app's own
/// defaults, where it is off until set; the page's editor leaves `spellcheck`
/// at its default. Set before the web view exists. A user who turns it off
/// from the context menu writes the same key, so only an absent key is set.
#[cfg(target_os = "macos")]
fn enable_continuous_spellcheck() {
    use objc2::msg_send;
    use objc2::runtime::{AnyClass, AnyObject};

    let (Some(defaults_class), Some(string_class)) =
        (AnyClass::get(c"NSUserDefaults"), AnyClass::get(c"NSString"))
    else {
        return;
    };
    // SAFETY: NSUserDefaults and NSString are public Foundation classes;
    // the key is a NUL-terminated UTF-8 literal; objectForKey: returns nil
    // for an absent key.
    unsafe {
        let defaults: *mut AnyObject = msg_send![defaults_class, standardUserDefaults];
        let key: *mut AnyObject = msg_send![
            string_class,
            stringWithUTF8String: c"WebContinuousSpellCheckingEnabled".as_ptr()
        ];
        if defaults.is_null() || key.is_null() {
            return;
        }
        let existing: *mut AnyObject = msg_send![defaults, objectForKey: key];
        if existing.is_null() {
            let _: () = msg_send![defaults, setBool: true, forKey: key];
        }
    }
}

/// WebKit's internal preference that holds page rendering near 60fps, even
/// on a 120Hz ProMotion display.
#[cfg(target_os = "macos")]
const WEBKIT_60FPS_FEATURE: &std::ffi::CStr = c"PreferPageRenderingUpdatesNear60FPSEnabled";

/// Let the page draw at the display's full rate (Eschaton, 2026-10-17:
/// Firefox drew cloud-1 at 120fps, and Garden's motion felt stiffer).
///
/// WKWebView keeps WebKit's "Prefer Page Rendering Updates near 60fps"
/// preference on, so `requestAnimationFrame` and CSS animation run at 60fps
/// on a 120Hz panel. Measured on a 16" M4 Max (macOS 15.6.1), in a bare
/// WKWebView: 60.2fps with the preference on, 120.0fps with it off. Turning
/// it off after the web view exists works, which is the only point where
/// Tauri hands us the view.
///
/// The preference is private WebKit API (`+[WKPreferences _features]`,
/// `-[WKPreferences _setEnabled:forFeature:]`). The flag is looked up by key,
/// and each selector is checked first. If a later macOS renames or removes
/// either, this does nothing and the page stays at 60fps.
#[cfg(target_os = "macos")]
fn lift_webkit_60fps_cap(window: &WebviewWindow) {
    use objc2::runtime::{AnyClass, AnyObject};
    use objc2::{msg_send, sel};

    let result = window.with_webview(|webview| {
        let wk_web_view = webview.inner() as *mut AnyObject;
        if wk_web_view.is_null() {
            return;
        }
        let Some(preferences_class) = AnyClass::get(c"WKPreferences") else {
            return;
        };
        // SAFETY: with_webview runs this on the main thread with the live
        // WKWebView. Every private selector is checked with
        // respondsToSelector: before it is sent, and the feature objects come
        // from WebKit's own list.
        unsafe {
            let has_features: bool =
                msg_send![preferences_class, respondsToSelector: sel!(_features)];
            if !has_features {
                return;
            }
            let configuration: *mut AnyObject = msg_send![wk_web_view, configuration];
            let preferences: *mut AnyObject = msg_send![configuration, preferences];
            if preferences.is_null() {
                return;
            }
            let can_set: bool =
                msg_send![preferences, respondsToSelector: sel!(_setEnabled:forFeature:)];
            if !can_set {
                return;
            }
            let features: *mut AnyObject = msg_send![preferences_class, _features];
            if features.is_null() {
                return;
            }
            let count: usize = msg_send![features, count];
            for index in 0..count {
                let feature: *mut AnyObject = msg_send![features, objectAtIndex: index];
                let key: *mut AnyObject = msg_send![feature, key];
                if key.is_null() {
                    continue;
                }
                let utf8: *const std::ffi::c_char = msg_send![key, UTF8String];
                if utf8.is_null() || std::ffi::CStr::from_ptr(utf8) != WEBKIT_60FPS_FEATURE {
                    continue;
                }
                let _: () = msg_send![preferences, _setEnabled: false, forFeature: feature];
                log::info!("webview: lifted WebKit's 60fps rendering preference");
                return;
            }
            log::warn!(
                "webview: WebKit has no {WEBKIT_60FPS_FEATURE:?} feature; page stays near 60fps"
            );
        }
    });
    if let Err(error) = result {
        log::warn!("webview: could not reach the WKWebView to lift the 60fps cap: {error}");
    }
}

/// Apply a title-bar mode to a live window - no restart. Toggles the OS frame
/// and, on macOS, the transparent/overlay title-bar attributes.
pub(crate) fn apply_title_bar_mode(
    window: &WebviewWindow,
    mode: TitleBarMode,
) -> Result<(), String> {
    window
        .set_decorations(mode.decorations())
        .map_err(|error| format!("set decorations: {error}"))?;

    #[cfg(target_os = "macos")]
    if mode.decorations() {
        apply_macos_title_bar(window, mode.overlay_title_bar())?;
    }

    Ok(())
}

/// Flip the macOS title-bar between standard and overlay at runtime. Must touch
/// AppKit on the main thread, so the work is dispatched there.
#[cfg(target_os = "macos")]
fn apply_macos_title_bar(window: &WebviewWindow, overlay: bool) -> Result<(), String> {
    use objc2::runtime::AnyObject;

    // NSWindowStyleMaskFullSizeContentView = 1 << 15
    const FULL_SIZE_CONTENT_VIEW: u64 = 1 << 15;
    // NSWindowTitleVisibility: Visible = 0, Hidden = 1
    let title_visibility: i64 = if overlay { 1 } else { 0 };

    let win = window.clone();
    window
        .run_on_main_thread(move || {
            let Ok(ns_window) = win.ns_window() else {
                return;
            };
            let ns_window = ns_window as *mut AnyObject;
            // SAFETY: ns_window() hands back the live NSWindow* and this runs on
            // the main thread, satisfying AppKit's threading requirement.
            unsafe {
                let _: () = objc2::msg_send![ns_window, setTitlebarAppearsTransparent: overlay];
                let _: () = objc2::msg_send![ns_window, setTitleVisibility: title_visibility];
                let mask: u64 = objc2::msg_send![ns_window, styleMask];
                let new_mask = if overlay {
                    mask | FULL_SIZE_CONTENT_VIEW
                } else {
                    mask & !FULL_SIZE_CONTENT_VIEW
                };
                let _: () = objc2::msg_send![ns_window, setStyleMask: new_mask];
            }
        })
        .map_err(|error| format!("dispatch title-bar update: {error}"))?;

    Ok(())
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GraphicsSettingsResponse {
    supported: bool,
    disable_compositing: bool,
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn graphics_settings_get(app: AppHandle) -> Result<GraphicsSettingsResponse, String> {
    Ok(GraphicsSettingsResponse {
        supported: cfg!(target_os = "linux"),
        disable_compositing: read_window_settings(&app)?.disable_compositing,
    })
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn graphics_settings_set(app: AppHandle, disabled: bool) -> Result<(), String> {
    if !cfg!(target_os = "linux") {
        return Err("The compositing preference is available only on Linux".to_string());
    }
    let mut settings = read_window_settings(&app)?;
    settings.disable_compositing = disabled;
    write_window_settings(&app, &settings)
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn get_window_appearance(app: AppHandle) -> Result<WindowSettings, String> {
    read_window_settings(&app)
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn set_window_appearance(app: AppHandle, mode: TitleBarMode) -> Result<(), String> {
    let mut settings = read_window_settings(&app)?;
    settings.title_bar = mode;
    write_window_settings(&app, &settings)?;
    let window = app
        .get_webview_window(MAIN_WINDOW_LABEL)
        .ok_or_else(|| "main webview window not found".to_string())?;
    apply_title_bar_mode(&window, mode)
}

/// Start dragging the main window, for the frontend's own drag region.
///
/// In the `Minimal` title-bar mode the web content covers the title-bar strip,
/// so the window moves only when the page asks. Tauri's `data-tauri-drag-region`
/// reads `event.target`, which the frontend's shadow DOM retargets to the top
/// bar's host element: every button in the bar would drag instead of click. The
/// top bar decides what is empty bar space and calls this. An app command, so
/// the default capability keeps its no-core-window-permissions rule.
#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn start_window_drag(app: AppHandle) -> Result<(), String> {
    let window = app
        .get_webview_window(MAIN_WINDOW_LABEL)
        .ok_or_else(|| "main webview window not found".to_string())?;
    window.start_dragging().map_err(|error| format!("start dragging: {error}"))
}

/// Double-click on the drag region: zoom (maximize) or restore, as a native
/// title bar does.
#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn toggle_window_maximize(app: AppHandle) -> Result<(), String> {
    let window = app
        .get_webview_window(MAIN_WINDOW_LABEL)
        .ok_or_else(|| "main webview window not found".to_string())?;
    let maximized = window.is_maximized().map_err(|error| format!("read maximized: {error}"))?;
    if maximized {
        window.unmaximize().map_err(|error| format!("unmaximize: {error}"))
    } else {
        window.maximize().map_err(|error| format!("maximize: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compositing_preference_is_backward_compatible_and_serialized() {
        let defaults: WindowSettings = serde_json::from_str("{}").unwrap();
        assert!(!defaults.disable_compositing);
        let enabled = WindowSettings {
            disable_compositing: true,
            ..WindowSettings::default()
        };
        let value = serde_json::to_value(enabled).unwrap();
        assert_eq!(value["disableCompositing"], true);
    }
}
