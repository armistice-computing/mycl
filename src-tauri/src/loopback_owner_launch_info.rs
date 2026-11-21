//! What a Connected Agents page prints for a new agent: the command that
//! speaks MCP over stdio for this install, from the running executable's
//! own path (never a guessed bundle name: the app may be Garden.app today
//! and something else at release). `translocated` says the app runs from
//! macOS App Translocation, where the path is a random read-only mount and
//! must not be written into anyone's config.

use serde_json::{json, Value};

pub(crate) fn agent_launch_info_value() -> Value {
    let exe = std::env::current_exe().ok();
    let command = exe
        .as_ref()
        .map(|path| path.to_string_lossy().to_string());
    let translocated = command
        .as_deref()
        .is_some_and(|path| path.contains("/AppTranslocation/"));
    json!({
        "command": command,
        "args": ["--mcp-stdio"],
        "envNames": ["GARDEN_MCP_TOKEN"],
        "translocated": translocated,
        "note": if translocated {
            "the app runs from App Translocation; move it to /Applications before printing a launch command"
        } else {
            "run the command with the connector token in GARDEN_MCP_TOKEN; the app must be running"
        },
    })
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn agent_launch_info() -> Result<Value, String> {
    Ok(agent_launch_info_value())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_info_names_the_running_executable_and_the_stdio_flag() {
        let info = agent_launch_info_value();
        assert_eq!(info["args"], json!(["--mcp-stdio"]));
        assert_eq!(info["envNames"], json!(["GARDEN_MCP_TOKEN"]));
        assert!(info["command"].as_str().is_some_and(|c| !c.is_empty()));
        assert_eq!(info["translocated"], json!(false));
    }
}
