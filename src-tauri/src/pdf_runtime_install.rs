//! Where the PDF helpers live on a machine that is not the build machine.
//!
//! The helper scripts and the pinned Docling project are compiled into the
//! binary and written into the profile before use, so nothing depends on the
//! source tree. `uv` is not shipped: when the person asks for Docling and no
//! `uv` is found, one pinned release is downloaded, checked against a hash
//! compiled in here, and kept in the profile. Nothing is downloaded before
//! that request.

use sha2::{Digest, Sha256};
use std::{
    io::Read,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::Duration,
};

const DOCLING_HELPER: &str = include_str!("../../scripts/docling_pdf_to_markdown.py");
const PYMUPDF4LLM_HELPER: &str = include_str!("../../scripts/pymupdf4llm_pdf_to_markdown.py");
const PROJECT_PYPROJECT: &str = include_str!("../../scripts/pdf-runtime/pyproject.toml");
const PROJECT_LOCK: &str = include_str!("../../scripts/pdf-runtime/uv.lock");
/// One page of text. Set-up converts it, so the models Docling fetches on its
/// first conversion arrive during set-up and not inside an import's timeout.
const WARMUP_PDF: &[u8] = include_bytes!("../../scripts/pdf-runtime/warmup.pdf");

pub(crate) const DOCLING_HELPER_FILE: &str = "docling_pdf_to_markdown.py";
pub(crate) const PYMUPDF4LLM_HELPER_FILE: &str = "pymupdf4llm_pdf_to_markdown.py";

const UV_VERSION: &str = "0.10.4";
/// The archive is about 20 MB; refuse anything far beyond it.
const UV_ARCHIVE_LIMIT_BYTES: u64 = 80 * 1024 * 1024;
const UV_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);

static HOME: OnceLock<PathBuf> = OnceLock::new();

/// Called once at startup with the profile directory.
pub(crate) fn remember_pdf_runtime_home(profile_dir: &Path) {
    let _ = HOME.set(profile_dir.join("models/pdf-runtime"));
}

/// The directory that holds the scripts, the Docling project and its
/// environment, and a downloaded `uv`.
pub(crate) fn pdf_runtime_home() -> PathBuf {
    HOME.get()
        .cloned()
        .unwrap_or_else(|| std::env::temp_dir().join("garden-pdf-runtime"))
}

pub(crate) fn pdf_runtime_scripts_dir() -> PathBuf {
    pdf_runtime_home().join("scripts")
}

pub(crate) fn pdf_runtime_project_dir() -> PathBuf {
    pdf_runtime_home().join("project")
}

pub(crate) fn warmup_pdf_path() -> PathBuf {
    pdf_runtime_home().join("warmup.pdf")
}

/// Where a `uv` downloaded by set-up is kept.
pub(crate) fn installed_uv_path() -> PathBuf {
    pdf_runtime_home().join("bin").join(uv_binary_name())
}

fn uv_binary_name() -> &'static str {
    if cfg!(windows) {
        "uv.exe"
    } else {
        "uv"
    }
}

/// True when a Docling environment built by set-up is still there to run:
/// the record of a past set-up does not prove it (the environment moved into
/// the profile on 2026-11-17, and a uv can be uninstalled).
pub(crate) fn docling_environment_present(project_dir: &Path, uv_found: bool) -> bool {
    uv_found && project_dir.join(".venv").is_dir()
}

/// Imports and set-up write the same files from several threads.
static WRITER: Mutex<()> = Mutex::new(());

fn write_if_changed(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let _one_writer = WRITER.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if std::fs::read(path).is_ok_and(|current| current == bytes) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
    }
    // Written beside the target and renamed, so a helper that is running
    // never reads half a file.
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&temporary, bytes)
        .map_err(|error| format!("write {}: {error}", temporary.display()))?;
    std::fs::rename(&temporary, path).map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        format!("place {}: {error}", path.display())
    })
}

fn write_pdf_runtime_files_into(home: &Path) -> Result<(), String> {
    let scripts = home.join("scripts");
    let project = home.join("project");
    write_if_changed(&scripts.join(DOCLING_HELPER_FILE), DOCLING_HELPER.as_bytes())?;
    write_if_changed(
        &scripts.join(PYMUPDF4LLM_HELPER_FILE),
        PYMUPDF4LLM_HELPER.as_bytes(),
    )?;
    write_if_changed(&project.join("pyproject.toml"), PROJECT_PYPROJECT.as_bytes())?;
    write_if_changed(&project.join("uv.lock"), PROJECT_LOCK.as_bytes())?;
    write_if_changed(&home.join("warmup.pdf"), WARMUP_PDF)
}

/// Writes the compiled-in scripts and project files into the profile. Cheap
/// when they are already there; a new build replaces what changed.
pub(crate) fn write_pdf_runtime_files() -> Result<(), String> {
    write_pdf_runtime_files_into(&pdf_runtime_home())
}

struct UvRelease {
    target: &'static str,
    sha256: &'static str,
}

/// The pinned `uv` for this platform. The hashes are the ones Astral
/// publishes beside each archive of the release.
fn pinned_uv_release() -> Option<UvRelease> {
    let (target, sha256) = if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        (
            "aarch64-apple-darwin",
            "a6852e4dc565c8fedcf5adcdf09fca7caf5347739bed512bd95b15dada36db51",
        )
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        (
            "x86_64-apple-darwin",
            "df6dd1c3ebeab4369a098c516c15c233c62bf789a40a4864b30dad1d38d7604e",
        )
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu")) {
        (
            "x86_64-unknown-linux-gnu",
            "6b52a47358deea1c5e173278bf46b2b489747a59ae31f2a4362ed5c6c1c269f7",
        )
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64", target_env = "gnu")) {
        (
            "aarch64-unknown-linux-gnu",
            "c84a6e6405715caa6e2f5ef8e5f29a5d0bc558a954e9f1b5c082b9d4708c222e",
        )
    } else {
        return None;
    };
    Some(UvRelease { target, sha256 })
}

fn uv_release_url(release: &UvRelease) -> String {
    format!(
        "https://github.com/astral-sh/uv/releases/download/{UV_VERSION}/uv-{}.tar.gz",
        release.target
    )
}

/// What set-up will fetch, for the person to read before they start it.
pub(crate) fn uv_download_note() -> Option<String> {
    pinned_uv_release().map(|release| {
        format!(
            "uv {UV_VERSION} (about 20 MB) from {}",
            uv_release_url(&release)
        )
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Checks the archive against `expected_sha256`, then writes its `uv` binary
/// to `destination`. Nothing is written when the hash differs.
fn install_uv_from_archive(
    archive: &[u8],
    expected_sha256: &str,
    destination: &Path,
) -> Result<(), String> {
    let actual = sha256_hex(archive);
    if actual != expected_sha256 {
        return Err(format!(
            "the downloaded uv archive does not match its expected hash (got {actual}); nothing was installed"
        ));
    }
    let wanted = destination
        .file_name()
        .ok_or_else(|| "uv destination has no file name".to_string())?
        .to_owned();
    let mut entries = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    for entry in entries
        .entries()
        .map_err(|error| format!("read uv archive: {error}"))?
    {
        let mut entry = entry.map_err(|error| format!("read uv archive entry: {error}"))?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry
            .path()
            .map_err(|error| format!("read uv archive path: {error}"))?
            .into_owned();
        if path.file_name() != Some(wanted.as_os_str()) {
            continue;
        }
        let mut binary = Vec::new();
        entry
            .read_to_end(&mut binary)
            .map_err(|error| format!("unpack uv: {error}"))?;
        write_if_changed(destination, &binary)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(destination, std::fs::Permissions::from_mode(0o755))
                .map_err(|error| format!("mark uv executable: {error}"))?;
        }
        return Ok(());
    }
    Err("the uv archive holds no uv binary".to_string())
}

/// Downloads the pinned `uv` into the profile and returns its path.
pub(crate) fn download_pinned_uv() -> Result<PathBuf, String> {
    let destination = installed_uv_path();
    download_pinned_uv_to(&destination)?;
    Ok(destination)
}

fn download_pinned_uv_to(destination: &Path) -> Result<(), String> {
    let release = pinned_uv_release().ok_or_else(|| {
        "uv is not installed, and this build cannot fetch it for this platform; install uv and try again"
            .to_string()
    })?;
    let url = uv_release_url(&release);
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(UV_DOWNLOAD_TIMEOUT))
        .build()
        .into();
    let mut response = agent
        .get(&url)
        .call()
        .map_err(|error| format!("download uv from {url}: {error}"))?;
    let archive = response
        .body_mut()
        .with_config()
        .limit(UV_ARCHIVE_LIMIT_BYTES)
        .read_to_vec()
        .map_err(|error| format!("download uv from {url}: {error}"))?;
    install_uv_from_archive(&archive, release.sha256, destination)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn archive_with(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        for (path, bytes) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder.append_data(&mut header, path, *bytes).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn the_runtime_files_are_written_and_a_changed_one_is_replaced() {
        let home = tempfile::tempdir().unwrap();
        write_pdf_runtime_files_into(home.path()).unwrap();
        let helper = home.path().join("scripts").join(DOCLING_HELPER_FILE);
        assert_eq!(std::fs::read_to_string(&helper).unwrap(), DOCLING_HELPER);
        assert!(home.path().join("scripts").join(PYMUPDF4LLM_HELPER_FILE).is_file());
        assert_eq!(
            std::fs::read_to_string(home.path().join("project/uv.lock")).unwrap(),
            PROJECT_LOCK
        );
        assert!(std::fs::read(home.path().join("warmup.pdf")).unwrap().starts_with(b"%PDF-"));

        std::fs::write(&helper, "an older build's helper").unwrap();
        write_pdf_runtime_files_into(home.path()).unwrap();
        assert_eq!(std::fs::read_to_string(&helper).unwrap(), DOCLING_HELPER);
    }

    #[test]
    fn a_record_of_set_up_is_not_an_environment() {
        let project = tempfile::tempdir().unwrap();
        assert!(!docling_environment_present(project.path(), true));
        std::fs::create_dir(project.path().join(".venv")).unwrap();
        assert!(docling_environment_present(project.path(), true));
        assert!(!docling_environment_present(project.path(), false));
    }

    #[test]
    fn many_writers_of_one_file_leave_it_whole() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().to_path_buf();
        let writers: Vec<_> = (0..8)
            .map(|_| {
                let root = root.clone();
                std::thread::spawn(move || write_pdf_runtime_files_into(&root))
            })
            .collect();
        for writer in writers {
            writer.join().unwrap().unwrap();
        }
        assert_eq!(
            std::fs::read_to_string(root.join("project/uv.lock")).unwrap(),
            PROJECT_LOCK
        );
        let leftovers = std::fs::read_dir(root.join("project"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
            .count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn the_compiled_in_project_pins_docling_and_carries_its_lock() {
        assert!(PROJECT_PYPROJECT.contains("\"docling=="));
        assert!(PROJECT_LOCK.contains("name = \"docling\""));
    }

    #[test]
    fn uv_is_installed_only_from_an_archive_with_the_expected_hash() {
        let home = tempfile::tempdir().unwrap();
        let destination = home.path().join("bin").join(uv_binary_name());
        let archive = archive_with(&[
            ("uv-some-target/uvx", b"not this one"),
            (&format!("uv-some-target/{}", uv_binary_name()), b"#!/bin/sh\nexit 0\n"),
        ]);

        let refused = install_uv_from_archive(&archive, &"0".repeat(64), &destination);
        assert!(refused.unwrap_err().contains("does not match"));
        assert!(!destination.exists());

        install_uv_from_archive(&archive, &sha256_hex(&archive), &destination).unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), b"#!/bin/sh\nexit 0\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&destination).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111);
        }
    }

    #[test]
    fn an_archive_without_uv_installs_nothing() {
        let home = tempfile::tempdir().unwrap();
        let destination = home.path().join("bin").join(uv_binary_name());
        let archive = archive_with(&[("uv-some-target/uvx", b"only the other tool")]);
        let error = install_uv_from_archive(&archive, &sha256_hex(&archive), &destination);
        assert!(error.unwrap_err().contains("holds no uv binary"));
        assert!(!destination.exists());
    }

    /// Reaches the network: `cargo test -- --ignored the_pinned_uv_downloads`.
    #[test]
    #[ignore]
    fn the_pinned_uv_downloads_checks_and_runs() {
        let home = tempfile::tempdir().unwrap();
        let destination = home.path().join("bin").join(uv_binary_name());
        download_pinned_uv_to(&destination).unwrap();
        let output = std::process::Command::new(&destination)
            .arg("--version")
            .output()
            .unwrap();
        let version = String::from_utf8_lossy(&output.stdout);
        assert!(version.starts_with(&format!("uv {UV_VERSION}")), "{version}");
    }

    #[test]
    fn the_pinned_release_names_a_real_archive_for_this_platform() {
        if let Some(release) = pinned_uv_release() {
            assert_eq!(release.sha256.len(), 64);
            let url = uv_release_url(&release);
            assert!(url.starts_with("https://github.com/astral-sh/uv/releases/download/0.10.4/uv-"));
            assert!(url.ends_with(".tar.gz"));
        }
    }
}
