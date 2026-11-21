//! The owner's allowed roots for artifacts: the fence a reference or a
//! disk upload must sit inside.
//!
//! "Artifacts — a body outside the house" (design seed, 2026-10-23) and
//! Epsilon's engineering review there: a root is an explicit grant to
//! Garden's own file broker, never an inheritance of a caller's harness
//! sandbox. The list lives in the graph's settings file on the owner
//! control plane (a Tauri command and `GET/PUT /owner/graphs/{id}/
//! artifact-roots`), never in the CRDT and never behind `artifacts.write`.
//! Every open re-checks the roots, walks the path component by component
//! without following symlinks, refuses special files, and answers
//! `missing` and `denied` as different things.

use crate::app_runtime::AppHandle;
use crate::{
    clock::timestamp,
    paths::existing_graph_dir,
    profile_paths::profile_dir,
    storage::{read_json, write_json},
};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    path::{Component, Path, PathBuf},
};

pub(crate) const ARTIFACT_ROOTS_VERSION: u32 = 1;
/// More roots than this is a configuration mistake, not a grant.
pub(crate) const MAX_ARTIFACT_ROOTS: usize = 64;

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ArtifactRoot {
    /// Canonical (symlinks resolved) absolute directory path.
    pub(crate) path: String,
    /// Whether files at any depth under the root are reachable, or only
    /// its direct children.
    #[serde(default = "default_true")]
    pub(crate) recursive: bool,
    /// Whether dot-prefixed components are reachable.
    #[serde(default)]
    pub(crate) include_hidden: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) added_at: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ArtifactRoots {
    #[serde(default)]
    pub(crate) version: u32,
    #[serde(default)]
    pub(crate) roots: Vec<ArtifactRoot>,
}

/// What the owner sends: the path as typed (canonicalized here) and the
/// two consent flags, both explicit in Epsilon's terms (recursive, hidden).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ArtifactRootInput {
    pub(crate) path: String,
    #[serde(default = "default_true")]
    pub(crate) recursive: bool,
    #[serde(default)]
    pub(crate) include_hidden: bool,
}

pub(crate) fn roots_path(graph_dir: &Path) -> PathBuf {
    graph_dir.join("artifact-roots.json")
}

/// The stored roots; empty when the file is absent or unreadable (the
/// second value says why, for the settings page).
pub(crate) fn read_roots_reported(graph_dir: &Path) -> (ArtifactRoots, Option<String>) {
    let path = roots_path(graph_dir);
    if !path.is_file() {
        return (ArtifactRoots::default(), None);
    }
    match read_json::<ArtifactRoots>(&path) {
        Ok(roots) => (roots, None),
        Err(error) => (ArtifactRoots::default(), Some(error.to_string())),
    }
}

pub(crate) fn read_roots(graph_dir: &Path) -> ArtifactRoots {
    read_roots_reported(graph_dir).0
}

/// Replace the list. Each root must be an absolute path to an existing
/// directory; it is stored canonical. Refused: the filesystem root, and any
/// directory that is the profile directory or contains it (the house
/// never grants itself its own store). Duplicates collapse to the first.
pub(crate) fn replace_roots(
    app: &AppHandle,
    graph_id: &str,
    inputs: Vec<ArtifactRootInput>,
) -> Result<ArtifactRoots, String> {
    if inputs.len() > MAX_ARTIFACT_ROOTS {
        return Err(format!(
            "too many roots ({}); the cap is {MAX_ARTIFACT_ROOTS}",
            inputs.len()
        ));
    }
    let graph_dir = existing_graph_dir(app, graph_id)?;
    let profile = profile_dir(app)?;
    let profile_canonical = std::fs::canonicalize(&profile).unwrap_or(profile);
    let existing = read_roots(&graph_dir);
    let mut roots: Vec<ArtifactRoot> = Vec::new();
    for input in inputs {
        let requested = PathBuf::from(input.path.trim());
        if !requested.is_absolute() {
            return Err(format!("root must be an absolute path: {}", input.path));
        }
        let canonical = std::fs::canonicalize(&requested)
            .map_err(|error| format!("root {}: {error}", input.path))?;
        if !canonical.is_dir() {
            return Err(format!("root is not a directory: {}", canonical.display()));
        }
        if canonical.parent().is_none() {
            return Err("the filesystem root cannot be a root".to_string());
        }
        if let Some(reason) = root_conflicts_with_profile(&profile_canonical, &canonical) {
            return Err(reason);
        }
        let path = canonical.to_string_lossy().to_string();
        if roots.iter().any(|root| root.path == path) {
            continue;
        }
        let added_at = existing
            .roots
            .iter()
            .find(|root| root.path == path)
            .and_then(|root| root.added_at.clone())
            .or_else(|| Some(timestamp()));
        roots.push(ArtifactRoot {
            path,
            recursive: input.recursive,
            include_hidden: input.include_hidden,
            added_at,
        });
    }
    let stored = ArtifactRoots {
        version: ARTIFACT_ROOTS_VERSION,
        roots,
    };
    write_json(&roots_path(&graph_dir), &stored).map_err(|error| error.to_string())?;
    Ok(stored)
}

/// The house never grants itself its own store: a root may not sit inside
/// the profile directory (the token files and every graph's bytes live
/// there). A root that CONTAINS the profile is allowed (Eschaton, 2026-10-26:
/// "you can have all of my home"), and the profile is carved out of it at
/// every authorize, so a reference under such a root still cannot reach
/// the store.
pub(crate) fn root_conflicts_with_profile(profile: &Path, root: &Path) -> Option<String> {
    if root.starts_with(profile) {
        return Some(format!(
            "root {} is inside the Mycl profile; the app never grants itself its own store",
            root.display()
        ));
    }
    None
}

/// Why a path was not opened. `Missing` is "nothing there"; `Denied` is
/// "there, but not yours to read" (outside the roots, hidden, a symlink or
/// special file in the way, or a root that has gone).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RootsDenial {
    Missing(String),
    Denied(String),
}

impl RootsDenial {
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            RootsDenial::Missing(_) => "missing",
            RootsDenial::Denied(_) => "denied",
        }
    }
    pub(crate) fn message(&self) -> &str {
        match self {
            RootsDenial::Missing(message) | RootsDenial::Denied(message) => message,
        }
    }
}

impl std::fmt::Display for RootsDenial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind(), self.message())
    }
}

/// A path the roots allow, with where it sits.
#[derive(Debug, Clone)]
pub(crate) struct Authorized {
    pub(crate) root: ArtifactRoot,
    pub(crate) root_path: PathBuf,
    pub(crate) canonical: PathBuf,
    pub(crate) relative: PathBuf,
}

/// Check a path against the graph's roots now (not at reference time:
/// at every open). The path is canonicalized; it must sit under a root,
/// at any depth when the root is recursive and as a direct child
/// otherwise, with no hidden component unless the root allows them.
pub(crate) fn authorize(
    app: &AppHandle,
    graph_id: &str,
    requested: &Path,
) -> Result<Authorized, RootsDenial> {
    let graph_dir = existing_graph_dir(app, graph_id).map_err(RootsDenial::Denied)?;
    let roots = read_roots(&graph_dir);
    let profile = profile_dir(app).map_err(RootsDenial::Denied)?;
    let profile = std::fs::canonicalize(&profile).unwrap_or(profile);
    authorize_against_excluding(&roots, requested, Some(&profile))
}

pub(crate) fn authorize_against(
    roots: &ArtifactRoots,
    requested: &Path,
) -> Result<Authorized, RootsDenial> {
    authorize_against_excluding(roots, requested, None)
}

/// `excluded` is the profile directory: whatever the roots say, nothing
/// under it is ever authorized.
pub(crate) fn authorize_against_excluding(
    roots: &ArtifactRoots,
    requested: &Path,
    excluded: Option<&Path>,
) -> Result<Authorized, RootsDenial> {
    if roots.roots.is_empty() {
        return Err(RootsDenial::Denied(
            "this graph has no allowed roots; the owner adds them in Settings → Files".to_string(),
        ));
    }
    if !requested.is_absolute() {
        return Err(RootsDenial::Denied(format!(
            "path must be absolute: {}",
            requested.display()
        )));
    }
    let canonical = match std::fs::canonicalize(requested) {
        Ok(path) => path,
        // A path that cannot be resolved is answered from the roots alone:
        // only a path that lexically sits under a root may be told "does
        // not exist" (or why it could not be read). Anything else gets the
        // same "outside" as an existing file would, so the fence is not an
        // oracle for what exists beyond it (angel review of steps 4 and 5).
        Err(error) => {
            let resolved = match resolve_through_existing_ancestor(requested) {
                Some(resolved) => resolved,
                None => {
                    return Err(RootsDenial::Denied(format!(
                        "{} is outside this graph's allowed roots",
                        requested.display()
                    )))
                }
            };
            if excluded.is_some_and(|excluded| resolved.starts_with(excluded)) {
                return Err(RootsDenial::Denied(format!(
                    "{} is inside the Mycl profile; the app never reads its own store through a reference",
                    requested.display()
                )));
            }
            if !roots.roots.iter().any(|root| {
                let root_path = Path::new(&root.path);
                resolved != root_path && resolved.starts_with(root_path)
            }) {
                return Err(RootsDenial::Denied(format!(
                    "{} is outside this graph's allowed roots",
                    requested.display()
                )));
            }
            if error.kind() == std::io::ErrorKind::NotFound {
                return Err(RootsDenial::Missing(format!(
                    "{} does not exist",
                    requested.display()
                )));
            }
            return Err(RootsDenial::Denied(format!(
                "{}: {error}",
                requested.display()
            )));
        }
    };
    if let Some(excluded) = excluded {
        if canonical.starts_with(excluded) {
            return Err(RootsDenial::Denied(format!(
                "{} is inside the Mycl profile; the app never reads its own store through a reference",
                requested.display()
            )));
        }
    }
    let mut nearest: Option<String> = None;
    for root in &roots.roots {
        let root_path = PathBuf::from(&root.path);
        if canonical == root_path || !canonical.starts_with(&root_path) {
            continue;
        }
        let relative = canonical
            .strip_prefix(&root_path)
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let components: Vec<Component<'_>> = relative.components().collect();
        if !root.recursive && components.len() > 1 {
            nearest = Some(format!(
                "root {} is not recursive; {} is nested below it",
                root.path,
                relative.display()
            ));
            continue;
        }
        if !root.include_hidden
            && components.iter().any(|component| {
                component
                    .as_os_str()
                    .to_str()
                    .map(|name| name.starts_with('.'))
                    .unwrap_or(true)
            })
        {
            nearest = Some(format!(
                "root {} does not include hidden files; {} has a hidden component",
                root.path,
                relative.display()
            ));
            continue;
        }
        return Ok(Authorized {
            root: root.clone(),
            root_path,
            canonical,
            relative,
        });
    }
    Err(RootsDenial::Denied(nearest.unwrap_or_else(|| {
        format!(
            "{} is outside this graph's allowed roots",
            requested.display()
        )
    })))
}

/// Where a path that does not resolve would sit: its deepest existing
/// ancestor canonicalized (so a symlink planted inside a root points where
/// it really points), with the missing tail appended as typed. A tail with
/// a `.` or `..` component is not placed anywhere: it could point back out.
fn resolve_through_existing_ancestor(requested: &Path) -> Option<PathBuf> {
    for ancestor in requested.ancestors().skip(1) {
        let Ok(canonical) = std::fs::canonicalize(ancestor) else {
            continue;
        };
        let tail = requested.strip_prefix(ancestor).ok()?;
        if tail
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        {
            return None;
        }
        return Some(canonical.join(tail));
    }
    None
}

/// Open an authorized path for reading, the race-resistant way: from the
/// root, one component at a time, never following a symlink, and only a
/// regular file at the end. A symlink or parent swapped between the check
/// and the open cannot redirect the read outside the root.
pub(crate) fn open_authorized(authorized: &Authorized) -> Result<File, RootsDenial> {
    open_walk(&authorized.root_path, &authorized.relative)
}

/// The same walk for a helper that needs a small file beside an
/// authorized one (a repository's HEAD): relative to the root, no-follow,
/// regular files only.
pub(crate) fn open_relative_no_follow(root: &Path, relative: &Path) -> Result<File, RootsDenial> {
    open_walk(root, relative)
}

#[cfg(unix)]
fn open_walk(root: &Path, relative: &Path) -> Result<File, RootsDenial> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::{AsRawFd, FromRawFd};

    fn map_os(error: std::io::Error, what: &Path) -> RootsDenial {
        match error.raw_os_error() {
            Some(code) if code == libc::ENOENT || code == libc::ENOTDIR => {
                RootsDenial::Missing(format!("{} does not exist", what.display()))
            }
            Some(code) if code == libc::ELOOP => RootsDenial::Denied(format!(
                "{} is a symlink; references never follow links",
                what.display()
            )),
            _ => RootsDenial::Denied(format!("{}: {error}", what.display())),
        }
    }

    let root_c = CString::new(root.as_os_str().as_bytes())
        .map_err(|_| RootsDenial::Denied("root path has an interior NUL".to_string()))?;
    let dir_flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: a valid C string, flags from libc; the fd is owned by `File`.
    let root_fd = unsafe { libc::open(root_c.as_ptr(), dir_flags) };
    if root_fd < 0 {
        return Err(map_os(std::io::Error::last_os_error(), root));
    }
    let mut current = unsafe { File::from_raw_fd(root_fd) };
    let components: Vec<Component<'_>> = relative.components().collect();
    if components.is_empty() {
        return Err(RootsDenial::Denied("a root itself is not a file".to_string()));
    }
    let mut walked = root.to_path_buf();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            return Err(RootsDenial::Denied(format!(
                "{} has a non-normal component",
                relative.display()
            )));
        };
        walked.push(name);
        let name_c = CString::new(name.as_bytes())
            .map_err(|_| RootsDenial::Denied("path has an interior NUL".to_string()))?;
        // Ask what the name is before opening it, without following: a
        // symlink is refused by name (on macOS a symlinked directory under
        // O_NOFOLLOW reads as "not a directory", which would look missing).
        {
            let mut stat: libc::stat = unsafe { std::mem::zeroed() };
            // SAFETY: valid fd, valid C string, stat buffer we own.
            let rc = unsafe {
                libc::fstatat(
                    current.as_raw_fd(),
                    name_c.as_ptr(),
                    &mut stat,
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            };
            if rc != 0 {
                return Err(map_os(std::io::Error::last_os_error(), &walked));
            }
            let last = index + 1 == components.len();
            let file_type = stat.st_mode & libc::S_IFMT;
            if file_type == libc::S_IFLNK {
                return Err(RootsDenial::Denied(format!(
                    "{} is a symlink; references never follow links",
                    walked.display()
                )));
            }
            // A FIFO or a device would block the open (a FIFO waits for a
            // writer) and pin a worker: only directories on the way and a
            // regular file at the end, decided before the open.
            if last && file_type != libc::S_IFREG {
                return Err(RootsDenial::Denied(format!(
                    "{} is not a regular file",
                    walked.display()
                )));
            }
            if !last && file_type != libc::S_IFDIR {
                return Err(RootsDenial::Denied(format!(
                    "{} is not a directory",
                    walked.display()
                )));
            }
        }
        let last = index + 1 == components.len();
        // O_NONBLOCK: should the entry be swapped for a FIFO between the
        // stat and the open, the open returns instead of waiting; the
        // regular-file check below then refuses it. A regular file ignores
        // the flag.
        let flags = if last {
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK
        } else {
            dir_flags
        };
        // SAFETY: as above; openat relative to a directory fd we own.
        let next = unsafe { libc::openat(current.as_raw_fd(), name_c.as_ptr(), flags) };
        if next < 0 {
            return Err(map_os(std::io::Error::last_os_error(), &walked));
        }
        current = unsafe { File::from_raw_fd(next) };
    }
    let metadata = current
        .metadata()
        .map_err(|error| RootsDenial::Denied(format!("{}: {error}", walked.display())))?;
    if !metadata.is_file() {
        return Err(RootsDenial::Denied(format!(
            "{} is not a regular file",
            walked.display()
        )));
    }
    // A hard link is a regular file whose bytes may live outside the root;
    // the walk cannot tell where an inode "really" is, so a linked file is
    // refused rather than guessed (angel review of step 2).
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() > 1 {
            return Err(RootsDenial::Denied(format!(
                "{} is hard-linked ({} names); a reference needs a file with one name",
                walked.display(),
                metadata.nlink()
            )));
        }
    }
    Ok(current)
}

#[cfg(not(unix))]
fn open_walk(root: &Path, relative: &Path) -> Result<File, RootsDenial> {
    let path = root.join(relative);
    let metadata = std::fs::symlink_metadata(&path)
        .map_err(|error| RootsDenial::Missing(format!("{}: {error}", path.display())))?;
    if !metadata.is_file() {
        return Err(RootsDenial::Denied(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    File::open(&path).map_err(|error| RootsDenial::Denied(format!("{}: {error}", path.display())))
}

/// The settings page's view: the roots and whether the file was readable.
pub(crate) fn roots_report(app: &AppHandle, graph_id: &str) -> Result<serde_json::Value, String> {
    let graph_dir = existing_graph_dir(app, graph_id)?;
    let (roots, error) = read_roots_reported(&graph_dir);
    Ok(serde_json::json!({
        "graphId": graph_id,
        "version": ARTIFACT_ROOTS_VERSION,
        "roots": roots.roots,
        "fileError": error,
    }))
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn get_artifact_roots(app: AppHandle, graph_id: String) -> Result<serde_json::Value, String> {
    roots_report(&app, &graph_id)
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn set_artifact_roots(
    app: AppHandle,
    graph_id: String,
    roots: Vec<ArtifactRootInput>,
) -> Result<serde_json::Value, String> {
    replace_roots(&app, &graph_id, roots)?;
    roots_report(&app, &graph_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("garden-roots-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(&dir).unwrap()
    }

    fn roots_of(root: &Path, recursive: bool, include_hidden: bool) -> ArtifactRoots {
        ArtifactRoots {
            version: 1,
            roots: vec![ArtifactRoot {
                path: root.to_string_lossy().to_string(),
                recursive,
                include_hidden,
                added_at: None,
            }],
        }
    }

    #[test]
    fn a_path_is_allowed_only_under_a_root_and_hidden_and_depth_are_consents() {
        let root = scratch("auth");
        std::fs::create_dir_all(root.join("sub/.hidden")).unwrap();
        std::fs::write(root.join("a.md"), "a").unwrap();
        std::fs::write(root.join("sub/b.md"), "b").unwrap();
        std::fs::write(root.join("sub/.hidden/c.md"), "c").unwrap();
        let outside = scratch("outside");
        std::fs::write(outside.join("o.md"), "o").unwrap();

        let open = roots_of(&root, true, false);
        let ok = authorize_against(&open, &root.join("a.md")).expect("direct child");
        assert_eq!(ok.relative, PathBuf::from("a.md"));
        assert_eq!(ok.canonical, root.join("a.md"));
        assert!(authorize_against(&open, &root.join("sub/b.md")).is_ok());
        let hidden = authorize_against(&open, &root.join("sub/.hidden/c.md")).unwrap_err();
        assert_eq!(hidden.kind(), "denied");
        assert!(hidden.message().contains("hidden"), "{hidden}");
        let out = authorize_against(&open, &outside.join("o.md")).unwrap_err();
        assert_eq!(out.kind(), "denied");
        assert!(out.message().contains("outside"), "{out}");
        let missing = authorize_against(&open, &root.join("nope.md")).unwrap_err();
        assert_eq!(missing.kind(), "missing");
        // The fence is not an oracle: a missing path outside the roots, or
        // one that climbs out with "..", is "outside", never "missing".
        let missing_outside = authorize_against(&open, &outside.join("nope.md")).unwrap_err();
        assert_eq!(missing_outside.kind(), "denied");
        assert!(missing_outside.message().contains("outside"), "{missing_outside}");
        let climbing = authorize_against(&open, &root.join("../nope-climb.md")).unwrap_err();
        assert_eq!(climbing.kind(), "denied");
        assert!(climbing.message().contains("outside"), "{climbing}");
        let missing_under_excluded = authorize_against_excluding(
            &roots_of(&outside, true, false),
            &outside.join("nope.md"),
            Some(&outside),
        )
        .unwrap_err();
        assert_eq!(missing_under_excluded.kind(), "denied");
        let itself = authorize_against(&open, &root).unwrap_err();
        assert_eq!(itself.kind(), "denied");
        assert_eq!(authorize_against(&open, Path::new("relative.md")).unwrap_err().kind(), "denied");

        let shallow = roots_of(&root, false, true);
        assert!(authorize_against(&shallow, &root.join("a.md")).is_ok());
        let deep = authorize_against(&shallow, &root.join("sub/b.md")).unwrap_err();
        assert!(deep.message().contains("not recursive"), "{deep}");
        assert!(authorize_against(&roots_of(&root, true, true), &root.join("sub/.hidden/c.md")).is_ok());

        let none = ArtifactRoots::default();
        assert!(authorize_against(&none, &root.join("a.md")).unwrap_err().message().contains("no allowed roots"));
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(outside);
    }

    #[cfg(unix)]
    #[test]
    fn the_open_walk_refuses_symlinks_and_special_files_and_reads_the_real_thing() {
        use std::io::Read;
        let root = scratch("walk");
        let outside = scratch("walk-outside");
        std::fs::write(outside.join("secret.md"), "secret").unwrap();
        std::fs::create_dir_all(root.join("dir")).unwrap();
        std::fs::write(root.join("dir/real.md"), "real").unwrap();
        std::os::unix::fs::symlink(outside.join("secret.md"), root.join("link.md")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("linkdir")).unwrap();
        let roots = roots_of(&root, true, false);

        // A missing name BELOW the link is placed where the link points, so
        // it is "outside" too: a planted link cannot ask what exists beyond.
        let beyond = authorize_against(&roots, &root.join("linkdir/nope.md")).unwrap_err();
        assert_eq!(beyond.kind(), "denied");
        assert!(beyond.message().contains("outside"), "{beyond}");
        assert!(!beyond.message().contains(outside.to_str().unwrap()), "{beyond}");
        // canonicalize follows the link, so authorize already says "outside"…
        let via_link = authorize_against(&roots, &root.join("link.md")).unwrap_err();
        assert_eq!(via_link.kind(), "denied");
        // …and the walk refuses a link that appears after the check.
        let authorized = Authorized {
            root: roots.roots[0].clone(),
            root_path: root.clone(),
            canonical: root.join("link.md"),
            relative: PathBuf::from("link.md"),
        };
        let swapped = open_authorized(&authorized).unwrap_err();
        assert_eq!(swapped.kind(), "denied");
        assert!(swapped.message().contains("symlink"), "{swapped}");
        let through_dir = Authorized {
            relative: PathBuf::from("linkdir/secret.md"),
            canonical: root.join("linkdir/secret.md"),
            ..authorized.clone()
        };
        assert_eq!(open_authorized(&through_dir).unwrap_err().kind(), "denied");
        let dir_not_file = Authorized {
            relative: PathBuf::from("dir"),
            canonical: root.join("dir"),
            ..authorized.clone()
        };
        assert!(open_authorized(&dir_not_file).unwrap_err().message().contains("regular file"));
        let gone = Authorized {
            relative: PathBuf::from("dir/gone.md"),
            canonical: root.join("dir/gone.md"),
            ..authorized.clone()
        };
        assert_eq!(open_authorized(&gone).unwrap_err().kind(), "missing");

        let real = authorize_against(&roots, &root.join("dir/real.md")).unwrap();
        let mut file = open_authorized(&real).expect("open the real file");
        let mut text = String::new();
        file.read_to_string(&mut text).unwrap();
        assert_eq!(text, "real");

        // A FIFO under the root is refused before the open, so nothing waits.
        {
            use std::ffi::CString;
            use std::os::unix::ffi::OsStrExt;
            let fifo = root.join("dir/pipe");
            let c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
            let started = std::time::Instant::now();
            let denied = authorize_against(&roots, &fifo)
                .and_then(|authorized| open_authorized(&authorized))
                .expect_err("a FIFO is refused");
            assert!(started.elapsed() < std::time::Duration::from_secs(2), "the open did not block");
            assert!(denied.message().contains("regular file"), "{denied}");
        }
        // A hard link to a file outside the root is refused.
        std::fs::hard_link(outside.join("secret.md"), root.join("dir/hard.md")).unwrap();
        let hard = authorize_against(&roots, &root.join("dir/hard.md")).unwrap();
        let denied = open_authorized(&hard).expect_err("hard link refused");
        assert!(denied.message().contains("hard-linked"), "{denied}");
        // A root inside the profile is refused; a root containing it is
        // allowed, and the profile is carved out at authorize.
        let profile = PathBuf::from("/p/profile");
        assert!(root_conflicts_with_profile(&profile, Path::new("/p")).is_none());
        assert!(root_conflicts_with_profile(&profile, Path::new("/p/profile")).is_some());
        assert!(root_conflicts_with_profile(&profile, Path::new("/p/profile/graphs/g")).is_some());
        assert!(root_conflicts_with_profile(&profile, Path::new("/p/other")).is_none());
        let home = scratch("home");
        let store = home.join("Library/profile/graphs/g");
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(store.join("loopback.json"), "{}").unwrap();
        std::fs::create_dir_all(home.join("Documents")).unwrap();
        std::fs::write(home.join("Documents/notes.md"), "mine").unwrap();
        let wide = roots_of(&home, true, true);
        let profile_dir = home.join("Library/profile");
        assert!(authorize_against_excluding(&wide, &home.join("Documents/notes.md"), Some(&profile_dir)).is_ok());
        let carved = authorize_against_excluding(&wide, &store.join("loopback.json"), Some(&profile_dir)).unwrap_err();
        assert_eq!(carved.kind(), "denied");
        assert!(carved.message().contains("Mycl profile"), "{carved}");
        let _ = std::fs::remove_dir_all(home);
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(outside);
    }
}
