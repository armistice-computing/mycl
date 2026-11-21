//! Shared release sink construction, exercised without launching a live profile.
use std::path::Path;
pub(crate) fn enabled(debug: bool, flag: Option<&str>) -> bool {
    !debug && flag == Some("1")
}
#[cfg(feature = "desktop")]
pub(crate) fn builder(
    profile: &Path,
    max_bytes: u128,
) -> Result<tauri_plugin_log::Builder, Box<dyn std::error::Error>> {
    let path = profile.join("diagnostics");
    std::fs::create_dir_all(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(tauri_plugin_log::Builder::default()
        .level(log::LevelFilter::Info)
        .filter(|metadata| metadata.target() == "search_timing")
        .targets([tauri_plugin_log::Target::new(
            tauri_plugin_log::TargetKind::Folder {
                path,
                file_name: Some("search-timing".into()),
            },
        )])
        .max_file_size(max_bytes)
        .rotation_strategy(tauri_plugin_log::RotationStrategy::KeepOne))
}
