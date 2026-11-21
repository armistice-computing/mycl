use crate::{
    clock::timestamp,
    ids::safe_filename,
    original_file_types::OriginalFileManifest,
    storage::{display_path, read_json, write_json},
};
use std::{
    fs,
    path::{Component, Path, PathBuf},
};

/// The metadata file that sits beside every stored original.
const MANIFEST_FILENAME: &str = "manifest.json";
/// The name a stored original takes when its own name is the metadata
/// file's name.
const RESERVED_ORIGINAL_FILENAME: &str = "source-manifest.json";

/// The name an original is stored under. A file that is itself called
/// `manifest.json` (in any case: macOS volumes fold case) would be overwritten
/// by the metadata written beside it, so it is stored under another name. The
/// artifact record keeps the user's name in `label` and `originalFilename`.
pub(crate) fn stored_original_filename(filename: &str) -> &str {
    if folds_to_manifest_filename(filename) {
        RESERVED_ORIGINAL_FILENAME
    } else {
        filename
    }
}

/// True when a case-insensitive volume would treat `filename` as the
/// metadata file's name. APFS folds case with Unicode rules, so the long s
/// (U+017F) is an `s` to it; no other letter of the name has such a twin.
fn folds_to_manifest_filename(filename: &str) -> bool {
    let folded = filename
        .chars()
        .flat_map(|c| if c == '\u{17f}' { 's'.to_lowercase() } else { c.to_lowercase() });
    folded.eq(MANIFEST_FILENAME.chars())
}

/// The volume's own answer, for a fold this code does not know: the stored
/// original and the metadata file must be two directory entries.
#[cfg(unix)]
fn refuse_original_stored_as_manifest(file_path: &Path, manifest_path: &Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let (Ok(file), Ok(manifest)) = (fs::metadata(file_path), fs::metadata(manifest_path)) else {
        return Ok(());
    };
    if file.dev() == manifest.dev() && file.ino() == manifest.ino() {
        return Err(format!(
            "original file {} has the name of its metadata file; nothing was changed",
            display_path(file_path)
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn refuse_original_stored_as_manifest(_file_path: &Path, _manifest_path: &Path) -> Result<(), String> {
    Ok(())
}

pub(crate) fn write_original_manifest(
    original_dir: &Path,
    file_path: &Path,
    filename: String,
    mime_type: &str,
    size_bytes: usize,
) -> Result<OriginalFileManifest, String> {
    write_original_manifest_hashed(original_dir, file_path, filename, mime_type, size_bytes, None)
}

pub(crate) fn write_original_manifest_hashed(
    original_dir: &Path,
    file_path: &Path,
    filename: String,
    mime_type: &str,
    size_bytes: usize,
    sha256: Option<String>,
) -> Result<OriginalFileManifest, String> {
    let manifest_path = original_dir.join(MANIFEST_FILENAME);
    refuse_original_stored_as_manifest(file_path, &manifest_path)?;
    let created_at = if manifest_path.is_file() {
        read_json::<OriginalFileManifest>(&manifest_path)
            .map(|manifest| manifest.created_at)
            .unwrap_or_else(|_| timestamp())
    } else {
        timestamp()
    };
    let updated_at = timestamp();
    let manifest = OriginalFileManifest {
        filename,
        mime_type: if mime_type.trim().is_empty() {
            "application/octet-stream".to_string()
        } else {
            mime_type.to_string()
        },
        size_bytes,
        local_path: display_path(file_path),
        created_at,
        updated_at: updated_at.clone(),
        sha256,
    };
    write_json(&manifest_path, &manifest)?;
    Ok(manifest)
}

pub(crate) fn read_original_manifest(original_dir: &Path) -> Result<OriginalFileManifest, String> {
    read_json::<OriginalFileManifest>(&original_dir.join("manifest.json")).map_err(Into::into)
}

pub(crate) fn original_manifest_file_path(
    original_dir: &Path,
    filename: &str,
) -> Result<PathBuf, String> {
    let mut components = Path::new(filename).components();
    let is_single_normal_component =
        matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none();
    if safe_filename(filename) != filename || !is_single_normal_component {
        return Err(format!(
            "original file manifest filename contains unsafe path characters: {filename}"
        ));
    }
    Ok(original_dir.join(filename))
}

pub(crate) fn rewrite_original_manifests_under(root: &Path) -> Result<usize, String> {
    if !root.is_dir() {
        return Ok(0);
    }
    let mut count = 0usize;
    for entry in
        fs::read_dir(root).map_err(|error| format!("read {}: {error}", display_path(root)))?
    {
        let entry = entry.map_err(|error| format!("read original manifest entry: {error}"))?;
        if !entry.path().is_dir() {
            continue;
        }
        let original_dir = entry.path().join("original");
        if original_dir.join("manifest.json").is_file() {
            rewrite_original_manifest_local_path(&original_dir)?;
            count += 1;
        }
    }
    Ok(count)
}

pub(crate) fn rewrite_original_manifest_local_path(original_dir: &Path) -> Result<(), String> {
    let manifest_path = original_dir.join("manifest.json");
    if !manifest_path.is_file() {
        return Ok(());
    }
    let mut manifest = read_original_manifest(original_dir)?;
    manifest.local_path = display_path(&original_manifest_file_path(
        original_dir,
        &manifest.filename,
    )?);
    write_json(&manifest_path, &manifest).map_err(Into::into)
}
