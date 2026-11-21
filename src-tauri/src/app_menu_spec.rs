//! The menu-bar spec the frontend sends to `set_app_menu` (see app_menu.rs).
//! Plain data with no Tauri dependency, so it builds and is tested in the
//! headless configuration too.

use serde::Deserialize;

/// Menu item ids for frontend commands carry this prefix, so a click can be
/// told apart from Tauri's predefined items.
pub(crate) const COMMAND_ID_PREFIX: &str = "cmd:";

/// The event a click on a frontend command emits, with `{ id }` as payload.
pub(crate) const APP_MENU_EVENT: &str = "app-menu";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AppMenuSpec {
    /// Items for the app menu, between About and Services (e.g. Settings).
    #[serde(default)]
    pub(crate) app_items: Vec<AppMenuItemSpec>,
    /// The menus after the app menu, in order. "Window" gets the native
    /// window items appended.
    pub(crate) menus: Vec<AppSubmenuSpec>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AppSubmenuSpec {
    pub(crate) title: String,
    pub(crate) items: Vec<AppMenuItemSpec>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub(crate) enum AppMenuItemSpec {
    Command {
        id: String,
        label: String,
        #[serde(default)]
        accelerator: Option<String>,
        #[serde(default = "enabled_default")]
        enabled: bool,
        /// Present only for a checkable item.
        #[serde(default)]
        checked: Option<bool>,
    },
    Separator,
    /// A macOS-provided item: undo, redo, cut, copy, paste, fullscreen,
    /// minimize, maximize, closeWindow.
    Native { item: String },
    /// A nested menu (Go > Switch Graph).
    Submenu { label: String, items: Vec<AppMenuItemSpec> },
}

fn enabled_default() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_frontend_spec() {
        let spec: AppMenuSpec = serde_json::from_str(
            r#"{
              "appItems": [{ "kind": "command", "id": "app.settings", "label": "Settings…", "accelerator": "CmdOrCtrl+," }],
              "menus": [
                { "title": "Edit", "items": [
                  { "kind": "native", "item": "undo" },
                  { "kind": "separator" },
                  { "kind": "command", "id": "edit.find", "label": "Find", "accelerator": "CmdOrCtrl+F", "enabled": false },
                  { "kind": "command", "id": "view.appearance.dark", "label": "Dark", "checked": true }
                ] }
              ]
            }"#,
        )
        .expect("spec parses");
        assert_eq!(spec.app_items.len(), 1);
        let items = &spec.menus[0].items;
        assert!(matches!(&items[0], AppMenuItemSpec::Native { item } if item == "undo"));
        assert!(matches!(items[1], AppMenuItemSpec::Separator));
        assert!(matches!(
            &items[2],
            AppMenuItemSpec::Command { id, enabled: false, checked: None, .. } if id == "edit.find"
        ));
        assert!(matches!(&items[3], AppMenuItemSpec::Command { checked: Some(true), enabled: true, .. }));
    }

    #[test]
    fn parses_a_nested_submenu() {
        let item: AppMenuItemSpec = serde_json::from_str(
            r#"{ "kind": "submenu", "label": "Switch Graph", "items": [
                 { "kind": "command", "id": "go.graph.default", "label": "Default", "checked": true }
               ] }"#,
        )
        .expect("submenu parses");
        let AppMenuItemSpec::Submenu { label, items } = item else { panic!("not a submenu") };
        assert_eq!(label, "Switch Graph");
        assert!(matches!(&items[0], AppMenuItemSpec::Command { checked: Some(true), .. }));
    }

    #[test]
    fn only_command_ids_carry_the_prefix() {
        assert_eq!("cmd:edit.find".strip_prefix(COMMAND_ID_PREFIX), Some("edit.find"));
        assert_eq!("undo".strip_prefix(COMMAND_ID_PREFIX), None);
    }
}
