use crate::{
    pdf_runtime_install::{
        installed_uv_path, pdf_runtime_home, pdf_runtime_project_dir, pdf_runtime_scripts_dir,
        DOCLING_HELPER_FILE, PYMUPDF4LLM_HELPER_FILE,
    },
    process_utils::executable_path,
    storage::display_path,
};
use std::{
    path::{Path, PathBuf},
    process,
};

pub(crate) const PYMUPDF4LLM_RUNTIME_ID: &str = "pymupdf4llm-python";
// Pinned to the known-good 1.28.2 line: PyPI published an empty pymupdf4llm
// 2.0 shim (2026-10-19) that resolves to an environment with no importable
// pymupdf4llm module, so `>=` bounds are unsafe here.
pub(crate) const PYMUPDF4LLM_PACKAGE_REQUIREMENT: &str = "pymupdf4llm==1.28.2";
pub(crate) const PYMUPDF_PACKAGE_REQUIREMENT: &str = "pymupdf==1.28.2";

pub(crate) const UV_ENV_OVERRIDE: &str = "MNEMOSYNE_UV";

/// The Python the Docling project runs on. Left to itself uv takes the newest
/// Python it finds, and the locked torch has no wheels for a Python released
/// after the lock was made (seen 2026-11-17: Python 3.15, torch 2.14.1). uv
/// downloads this version when the machine does not have it.
pub(crate) const DOCLING_PYTHON_VERSION: &str = "3.13";

pub(crate) fn uv_cache_path_hint() -> String {
    if let Ok(cache_dir) = std::env::var("UV_CACHE_DIR") {
        return display_path(Path::new(&cache_dir));
    }
    if let Ok(home) = std::env::var("HOME") {
        return display_path(&Path::new(&home).join(".cache/uv"));
    }
    "uv default cache".to_string()
}

pub(crate) fn local_platform_label() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

pub(crate) fn docling_helper_path_hint() -> String {
    display_path(&pdf_runtime_scripts_dir().join(DOCLING_HELPER_FILE))
}

pub(crate) fn pymupdf4llm_helper_path_hint() -> String {
    display_path(&pdf_runtime_scripts_dir().join(PYMUPDF4LLM_HELPER_FILE))
}

fn known_uv_locations() -> Vec<PathBuf> {
    // Finder-launched apps get PATH=/usr/bin:/bin:/usr/sbin:/sbin, so a
    // PATH-only lookup misses the usual uv install spots.
    let mut locations = vec![
        PathBuf::from("/opt/homebrew/bin/uv"),
        PathBuf::from("/usr/local/bin/uv"),
    ];
    if let Ok(home) = std::env::var("HOME") {
        let home = Path::new(&home);
        locations.push(home.join(".local/bin/uv"));
        locations.push(home.join(".cargo/bin/uv"));
    }
    // Last: the uv that Docling set-up downloads into the profile. A uv the
    // person installed themselves is preferred.
    locations.push(installed_uv_path());
    locations
}

fn resolve_uv_path(
    env_override: Option<String>,
    path_lookup: impl Fn(&str) -> Option<PathBuf>,
    known_locations: &[PathBuf],
) -> Option<PathBuf> {
    if let Some(override_path) = env_override
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        return Some(PathBuf::from(override_path));
    }
    if let Some(path) = path_lookup("uv") {
        return Some(path);
    }
    known_locations
        .iter()
        .find(|path| path.is_file())
        .cloned()
}

pub(crate) fn uv_command_path() -> Option<PathBuf> {
    resolve_uv_path(
        std::env::var(UV_ENV_OVERRIDE).ok(),
        executable_path,
        &known_uv_locations(),
    )
}

fn uv_command() -> process::Command {
    let uv_path = uv_command_path().unwrap_or_else(|| PathBuf::from("uv"));
    let mut command = process::Command::new(&uv_path);
    if uv_path == installed_uv_path() {
        // A Mac without developer tools has only a stub at /usr/bin/python3
        // that opens an installer dialog. Our own uv brings its own Python.
        command.env("UV_PYTHON_PREFERENCE", "only-managed");
    }
    // uv spawns grandchildren (python, pip internals); give them a usable
    // PATH even when uv itself came from an absolute fallback location.
    if let Some(parent) = uv_path.parent() {
        let current = std::env::var_os("PATH").unwrap_or_default();
        let already_present = std::env::split_paths(&current).any(|dir| dir == parent);
        if !already_present {
            let mut entries = vec![parent.to_path_buf()];
            entries.extend(std::env::split_paths(&current));
            if let Ok(joined) = std::env::join_paths(entries) {
                command.env("PATH", joined);
            }
        }
    }
    command
}

fn docling_uv_command() -> process::Command {
    let mut command = uv_command();
    command
        .arg("run")
        .arg("--locked")
        .arg("--project")
        .arg(pdf_runtime_project_dir())
        .arg("--python")
        .arg(DOCLING_PYTHON_VERSION)
        .arg("python");
    command
}

pub(crate) fn docling_python_command() -> process::Command {
    if let Ok(python) = std::env::var("MNEMOSYNE_DOCLING_PYTHON") {
        process::Command::new(python)
    } else {
        docling_uv_command()
    }
}

pub(crate) fn pymupdf4llm_python_command() -> process::Command {
    if let Ok(python) = std::env::var("MNEMOSYNE_PYMUPDF_PYTHON") {
        process::Command::new(python)
    } else {
        let mut command = uv_command();
        command
            .arg("run")
            .arg("--no-project")
            .arg("--python")
            .arg(">=3.10")
            .arg("--with")
            .arg(PYMUPDF4LLM_PACKAGE_REQUIREMENT)
            .arg("--with")
            .arg(PYMUPDF_PACKAGE_REQUIREMENT)
            .arg("python")
            .current_dir(pdf_runtime_home());
        command
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn runtime_helper_paths_point_at_packaged_scripts() {
        assert!(docling_helper_path_hint().ends_with("scripts/docling_pdf_to_markdown.py"));
        assert!(pymupdf4llm_helper_path_hint().ends_with("scripts/pymupdf4llm_pdf_to_markdown.py"));
    }

    #[test]
    fn local_platform_label_contains_os_and_arch() {
        let label = local_platform_label();

        assert!(label.contains(std::env::consts::OS));
        assert!(label.contains(std::env::consts::ARCH));
    }

    #[test]
    fn pymupdf_requirements_stay_on_the_known_good_line() {
        assert!(PYMUPDF4LLM_PACKAGE_REQUIREMENT.starts_with("pymupdf4llm==1.28."));
        assert!(PYMUPDF_PACKAGE_REQUIREMENT.starts_with("pymupdf==1.28."));
    }

    #[test]
    fn docling_uv_command_uses_the_lock_without_rewriting_it() {
        let command = docling_uv_command();
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let locked = args.iter().position(|arg| arg == "--locked");
        let project = args.iter().position(|arg| arg == "--project");

        assert!(locked.is_some(), "docling command must not re-resolve uv.lock: {args:?}");
        assert!(project.is_some(), "docling command must name the pinned project: {args:?}");
        assert!(locked.unwrap() < project.unwrap());
        let python = args.iter().position(|arg| arg == "--python").expect("pinned python");
        assert_eq!(args[python + 1], DOCLING_PYTHON_VERSION);
        assert_eq!(args.last().map(String::as_str), Some("python"));
    }

    fn shim_dir(label: &str) -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "mnemosyne-uv-resolver-{label}-{}-{suffix}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create shim dir");
        dir
    }

    fn shim_binary(dir: &Path) -> PathBuf {
        let path = dir.join("uv");
        std::fs::write(&path, b"#!/bin/sh\nexit 0\n").expect("write shim uv");
        path
    }

    #[test]
    fn uv_env_override_wins_over_path_and_known_locations() {
        let override_dir = shim_dir("override");
        let path_dir = shim_dir("path");
        let known_dir = shim_dir("known");
        let override_shim = shim_binary(&override_dir);
        let path_shim = shim_binary(&path_dir);
        let known_shim = shim_binary(&known_dir);

        let resolved = resolve_uv_path(
            Some(override_shim.to_string_lossy().into_owned()),
            |_| Some(path_shim.clone()),
            &[known_shim.clone()],
        );

        assert_eq!(resolved, Some(override_shim));
        let _ = std::fs::remove_dir_all(override_dir);
        let _ = std::fs::remove_dir_all(path_dir);
        let _ = std::fs::remove_dir_all(known_dir);
    }

    #[test]
    fn uv_path_lookup_wins_over_known_locations() {
        let path_dir = shim_dir("path-only");
        let known_dir = shim_dir("known-shadowed");
        let path_shim = shim_binary(&path_dir);
        let known_shim = shim_binary(&known_dir);

        let resolved = resolve_uv_path(None, |_| Some(path_shim.clone()), &[known_shim.clone()]);

        assert_eq!(resolved, Some(path_shim));
        let _ = std::fs::remove_dir_all(path_dir);
        let _ = std::fs::remove_dir_all(known_dir);
    }

    #[test]
    fn uv_absolute_fallback_used_when_override_and_path_miss() {
        let known_dir = shim_dir("fallback");
        let known_shim = shim_binary(&known_dir);
        let missing = known_dir.join("does-not-exist");

        let resolved = resolve_uv_path(None, |_| None, &[missing, known_shim.clone()]);

        assert_eq!(resolved, Some(known_shim));
        let _ = std::fs::remove_dir_all(known_dir);
    }

    #[test]
    fn uv_blank_override_is_ignored() {
        let known_dir = shim_dir("blank-override");
        let known_shim = shim_binary(&known_dir);

        let resolved = resolve_uv_path(Some("   ".to_string()), |_| None, &[known_shim.clone()]);

        assert_eq!(resolved, Some(known_shim));
        let _ = std::fs::remove_dir_all(known_dir);
    }

    #[test]
    fn uv_resolution_returns_none_when_nothing_exists() {
        let missing = std::env::temp_dir().join("mnemosyne-uv-resolver-missing-uv");

        let resolved = resolve_uv_path(None, |_| None, &[missing]);

        assert_eq!(resolved, None);
    }
}
