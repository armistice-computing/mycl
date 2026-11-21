//! Referenced mode: a body outside the house, addressed and not held.
//!
//! A reference records a path under one of the owner's roots
//! ([`crate::artifact_roots`]), the hash of the bytes as the graph saw
//! them, a light index, and where the file sits in a repository when a
//! `.git` lies at or under the root. Nothing is copied. Every later read
//! re-authorizes the path against the roots and re-opens it without
//! following links, and answers `ok | stale | missing | denied` with the
//! current hash beside the record's. The path and the index never enter
//! RDF or the vector store (Eschaton's release-candidate ruling).

use crate::app_runtime::AppHandle;
use crate::{
    artifact_attach::build_light_index_from_file,
    artifact_roots::{authorize, open_authorized, open_relative_no_follow, Authorized, RootsDenial},
    clock::timestamp,
    crdt_engine::workspace_ops::{
        artifact_file_type, ARTIFACT_MODE_REFERENCED, ARTIFACT_STORAGE_EXTERNAL,
    },
    crdt_projection_flush::flush_graph_projection,
    crdt_queue::{enqueue_crdt_operation, EnqueueCrdtOperationInput},
    ids::local_upload_mime_type_for_filename,
    json_utils::json_string,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    path::{Path, PathBuf},
};
use uuid::Uuid;

/// Above this a referenced file is recorded and read without a hash, so a
/// reference to a multi-gigabyte file never reads it whole for staleness.
pub(crate) const REFERENCE_HASH_CAP_BYTES: u64 = 512 * 1024 * 1024;
/// The download of a reference is served from memory; above this it is
/// refused with a note (the file is on this computer; open it in place).
pub(crate) const REFERENCE_DOWNLOAD_CAP_BYTES: u64 = crate::runtime_config::LOCAL_UPLOAD_MAX_BYTES as u64;
/// A repository's HEAD or ref file is a line; anything longer is not one.
const REPO_REF_FILE_CAP: u64 = 64 * 1024;
const REPO_PACKED_REFS_CAP: u64 = 4 * 1024 * 1024;

pub(crate) struct ReferenceInput {
    pub(crate) graph_id: String,
    pub(crate) path: String,
    pub(crate) label: Option<String>,
    pub(crate) parent_id: Option<String>,
}

/// Hash a file through an already-open descriptor, keeping the first
/// `keep` bytes. Returns (sha256 when the file is under the cap, total
/// length, kept bytes).
fn hash_and_keep(
    file: &mut std::fs::File,
    keep: usize,
) -> Result<(Option<String>, u64, Vec<u8>), String> {
    let len = file
        .metadata()
        .map_err(|error| format!("stat referenced file: {error}"))?
        .len();
    let mut kept: Vec<u8> = Vec::with_capacity(keep.min(len as usize));
    let hash_it = len <= REFERENCE_HASH_CAP_BYTES;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    let mut total: u64 = 0;
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("read referenced file: {error}"))?;
        if read == 0 {
            break;
        }
        total += read as u64;
        if kept.len() < keep {
            let take = (keep - kept.len()).min(read);
            kept.extend_from_slice(&buffer[..take]);
        }
        if hash_it {
            hasher.update(&buffer[..read]);
        } else if kept.len() >= keep {
            break;
        }
    }
    let sha256 = hash_it.then(|| format!("{:x}", hasher.finalize()));
    Ok((sha256, if hash_it { total } else { len }, kept))
}

/// Where the file sits in a repository, when a `.git` lies at or under the
/// root (the walk up stops at the root: metadata never reads past the
/// grant). The commit is HEAD's sha when it can be read without a git
/// binary; a worktree's `.git` file is left alone.
pub(crate) fn detect_repo(authorized: &Authorized) -> Option<Value> {
    let mut dir = authorized.canonical.parent()?.to_path_buf();
    loop {
        if !dir.starts_with(&authorized.root_path) {
            return None;
        }
        let git = dir.join(".git");
        // symlink_metadata: a `.git` that is a link is not a repository here.
        if std::fs::symlink_metadata(&git).map(|meta| meta.is_dir()).unwrap_or(false) {
            let relative = authorized
                .canonical
                .strip_prefix(&dir)
                .ok()?
                .to_string_lossy()
                .to_string();
            let mut repo = json!({ "root": dir.to_string_lossy(), "relative": relative });
            if let Ok(git_relative) = git.strip_prefix(&authorized.root_path) {
                if let Some(commit) = read_head(&authorized.root_path, git_relative) {
                    repo["commit"] = commit;
                }
            }
            return Some(repo);
        }
        if dir == authorized.root_path {
            return None;
        }
        dir = dir.parent()?.to_path_buf();
    }
}

/// A small file under the root, read through the same no-follow walk as
/// the reference itself: the repository metadata never reads past the
/// grant (angel review of step 2: `Path::join` with an absolute ref name
/// would have replaced the base, and std reads follow links).
fn read_small_under_root(root: &Path, relative: &Path, cap: u64) -> Option<String> {
    let mut file = open_relative_no_follow(root, relative).ok()?;
    let mut bytes = Vec::new();
    file.take(cap + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > cap {
        return None;
    }
    String::from_utf8(bytes).ok()
}

fn is_commit_sha(value: &str) -> bool {
    (value.len() == 40 || value.len() == 64) && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A ref name is `refs/…` with plain components: no absolute path, no
/// `..`, nothing a `join` could walk out with.
fn safe_ref_name(reference: &str) -> Option<PathBuf> {
    let path = Path::new(reference);
    if path.is_absolute() || !reference.starts_with("refs/") {
        return None;
    }
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(name) if name != "." && name != ".." => out.push(name),
            _ => return None,
        }
    }
    Some(out)
}

fn read_head(root: &Path, git_relative: &Path) -> Option<Value> {
    let head = read_small_under_root(root, &git_relative.join("HEAD"), REPO_REF_FILE_CAP)?;
    let head = head.trim();
    if let Some(reference) = head.strip_prefix("ref: ") {
        let reference = reference.trim();
        let ref_path = safe_ref_name(reference)?;
        let sha = read_small_under_root(root, &git_relative.join(&ref_path), REPO_REF_FILE_CAP)
            .map(|s| s.trim().to_string())
            .filter(|s| is_commit_sha(s))
            .or_else(|| {
                let packed = read_small_under_root(
                    root,
                    &git_relative.join("packed-refs"),
                    REPO_PACKED_REFS_CAP,
                )?;
                packed.lines().find_map(|line| {
                    let (sha, name) = line.split_once(' ')?;
                    (name.trim() == reference && is_commit_sha(sha.trim()))
                        .then(|| sha.trim().to_string())
                })
            })?;
        return Some(json!({ "sha": sha, "ref": reference }));
    }
    is_commit_sha(head).then(|| json!({ "sha": head }))
}

/// Make a reference: authorize, open through the walk, hash, index, put.
pub(crate) async fn reference_path(app: AppHandle, input: ReferenceInput) -> Result<Value, String> {
    let requested = PathBuf::from(input.path.trim());
    let authorized = authorize(&app, &input.graph_id, &requested).map_err(|denial| denial.to_string())?;
    let filename = authorized
        .canonical
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| "unnamed".to_string());
    let mime_type = local_upload_mime_type_for_filename(&filename);
    let (sha256, size_bytes, index, repo) = {
        let authorized = authorized.clone();
        let filename = filename.clone();
        let mime_type = mime_type.clone();
        tokio::task::spawn_blocking(move || -> Result<_, String> {
            let mut file = open_authorized(&authorized).map_err(|denial| denial.to_string())?;
            let (sha256, size, _) = hash_and_keep(&mut file, 0)?;
            // The index reads through the same descriptor: no second open by name.
            let index = build_light_index_from_file(&mut file, &filename, &mime_type, size as usize);
            drop(file);
            let repo = detect_repo(&authorized);
            Ok((sha256, size, index, repo))
        })
        .await
        .map_err(|error| format!("reference worker failed: {error}"))??
    };
    let artifact_id = format!("artifact-{}", Uuid::new_v4().simple());
    let label = input
        .label
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| filename.clone());
    let mut source = json!({
        "kind": "path",
        "path": authorized.canonical.to_string_lossy(),
        "root": authorized.root.path,
        "relative": authorized.relative.to_string_lossy(),
    });
    if let Some(repo) = repo {
        source["repo"] = repo;
    }
    let mut payload = json!({
        "artifactId": artifact_id,
        "label": label,
        "originalFilename": filename,
        "mimeType": mime_type,
        "sizeBytes": size_bytes,
        "fileType": artifact_file_type(&filename),
        "status": "ready",
        "parentId": input.parent_id,
        "mode": ARTIFACT_MODE_REFERENCED,
        "storage": ARTIFACT_STORAGE_EXTERNAL,
        "source": source,
        "index": index,
    });
    if let (Some(object), Some(hash)) = (payload.as_object_mut(), sha256.as_deref()) {
        object.insert("sha256".into(), json!(hash));
        object.insert("hashedAt".into(), json!(timestamp()));
    }
    let record = enqueue_crdt_operation(
        app.clone(),
        EnqueueCrdtOperationInput {
            kind: "workspace.putArtifact".to_string(),
            graph_id: input.graph_id.clone(),
            document_id: Some(artifact_id.clone()),
            payload,
        },
    )
    .await?;
    flush_graph_projection(app, &input.graph_id).await?;
    Ok(json!({
        "success": true,
        "graphId": input.graph_id,
        "graph_id": input.graph_id,
        "artifactId": artifact_id,
        "artifact_id": artifact_id,
        "mode": ARTIFACT_MODE_REFERENCED,
        "sha256": sha256,
        "sizeBytes": size_bytes,
        "mimeType": mime_type,
        "source": record.get("source").cloned().unwrap_or(Value::Null),
        "index": record.get("index").cloned().unwrap_or(Value::Null),
        "artifact": record,
    }))
}

/// The record for a file under the roots, found or made: an existing
/// referenced record with the same canonical path is reused (one file, one
/// record, however many wires); otherwise the file is referenced now, with
/// everything that implies (authorization, hash, index). A relative path
/// is resolved against the roots and must match exactly one of them.
pub(crate) async fn find_or_reference_by_path(
    app: AppHandle,
    graph_id: &str,
    path: &str,
) -> Result<String, String> {
    let (canonical, existing) = find_referenced_by_path(&app, graph_id, path)?;
    if let Some(id) = existing {
        return Ok(id);
    }
    let created = reference_path(
        app,
        ReferenceInput { graph_id: graph_id.to_string(), path: canonical, label: None, parent_id: None },
    )
    .await?;
    json_string(created.get("artifactId")).ok_or_else(|| "reference made no record".to_string())
}

/// The canonical path a request names (authorized against the roots) and
/// the referenced record that already holds it, if any. Makes nothing: a
/// question from the repository side (step 5) must not leave a record.
pub(crate) fn find_referenced_by_path(
    app: &AppHandle,
    graph_id: &str,
    path: &str,
) -> Result<(String, Option<String>), String> {
    let requested = resolve_requested_path(app, graph_id, path)?;
    let authorized = authorize(app, graph_id, &requested).map_err(|denial| denial.to_string())?;
    let canonical = authorized.canonical.to_string_lossy().to_string();
    let graph_dir = crate::paths::existing_graph_dir(app, graph_id)?;
    let workspace = crate::document_service::read_workspace_record(&graph_dir, graph_id)?;
    let existing = workspace
        .snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.get("artifacts"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|record| record_is_referenced(record))
        .find(|record| {
            record
                .get("source")
                .and_then(|source| json_string(source.get("path")))
                .as_deref()
                == Some(canonical.as_str())
        })
        .and_then(|record| json_string(record.get("id")));
    Ok((canonical, existing))
}

fn resolve_requested_path(app: &AppHandle, graph_id: &str, path: &str) -> Result<PathBuf, String> {
    let requested = PathBuf::from(path.trim());
    if requested.is_absolute() {
        return Ok(requested);
    }
    let graph_dir = crate::paths::existing_graph_dir(app, graph_id)?;
    let roots = crate::artifact_roots::read_roots(&graph_dir);
    let mut matches: Vec<PathBuf> = roots
        .roots
        .iter()
        .map(|root| Path::new(&root.path).join(&requested))
        .filter(|candidate| candidate.is_file())
        .collect();
    match matches.len() {
        0 => Err(format!("missing: {path} is under none of the graph's roots")),
        1 => Ok(matches.remove(0)),
        count => Err(format!("ambiguous: {path} exists under {count} roots; give an absolute path")),
    }
}

/// What a read of a reference answers, beside the bytes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum ReadState {
    Ok,
    Stale,
    Missing,
    Denied,
}

impl ReadState {
    pub(crate) fn name(self) -> &'static str {
        match self {
            ReadState::Ok => "ok",
            ReadState::Stale => "stale",
            ReadState::Missing => "missing",
            ReadState::Denied => "denied",
        }
    }
}

pub(crate) struct ReferencedRead {
    pub(crate) state: ReadState,
    pub(crate) note: Option<String>,
    pub(crate) bytes: Vec<u8>,
    pub(crate) size_bytes: u64,
    pub(crate) sha256_now: Option<String>,
    pub(crate) filename: String,
    pub(crate) mime_type: String,
    pub(crate) path: String,
}

pub(crate) fn record_is_referenced(record: &Value) -> bool {
    json_string(record.get("mode")).as_deref() == Some(ARTIFACT_MODE_REFERENCED)
}

/// Code and plain files often carry no useful mime type; bytes that are
/// valid UTF-8 with no NUL in the window are text to a reader.
pub(crate) fn looks_like_text(bytes: &[u8]) -> bool {
    !bytes.is_empty()
        && !bytes.iter().take(8192).any(|byte| *byte == 0)
        && std::str::from_utf8(bytes).is_ok()
}

/// Read a referenced record's bytes now: re-authorize, re-open through the
/// walk, hash while reading (bounded), keep `byte_cap` bytes for the
/// caller, and say whether the file moved, vanished, or is no longer
/// granted.
pub(crate) fn read_referenced(
    app: &AppHandle,
    graph_id: &str,
    record: &Value,
    byte_cap: usize,
) -> ReferencedRead {
    let path = record
        .get("source")
        .and_then(|source| json_string(source.get("path")))
        .unwrap_or_default();
    // The record may be the navigation shape (originalFilename) or the
    // snapshot entity (sourceFile.sf_originalFilename, name).
    let filename = json_string(record.get("originalFilename"))
        .or_else(|| {
            record
                .get("sourceFile")
                .and_then(|source| json_string(source.get("sf_originalFilename")))
        })
        .or_else(|| json_string(record.get("label")))
        .or_else(|| json_string(record.get("name")))
        .or_else(|| {
            Path::new(&path)
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "unnamed".to_string());
    let mime_type = json_string(record.get("mimeType"))
        .filter(|mime| mime != "application/octet-stream")
        .unwrap_or_else(|| local_upload_mime_type_for_filename(&filename));
    let empty = |state: ReadState, note: String| ReferencedRead {
        state,
        note: Some(note),
        bytes: Vec::new(),
        size_bytes: 0,
        sha256_now: None,
        filename: filename.clone(),
        mime_type: mime_type.clone(),
        path: path.clone(),
    };
    if path.is_empty() {
        return empty(ReadState::Denied, "the record names no path".to_string());
    }
    let authorized = match authorize(app, graph_id, Path::new(&path)) {
        Ok(authorized) => authorized,
        Err(RootsDenial::Missing(message)) => return empty(ReadState::Missing, message),
        Err(RootsDenial::Denied(message)) => return empty(ReadState::Denied, message),
    };
    if authorized.canonical != Path::new(&path) {
        return empty(
            ReadState::Denied,
            format!("{path} now resolves to {}", authorized.canonical.display()),
        );
    }
    let mut file = match open_authorized(&authorized) {
        Ok(file) => file,
        Err(RootsDenial::Missing(message)) => return empty(ReadState::Missing, message),
        Err(RootsDenial::Denied(message)) => return empty(ReadState::Denied, message),
    };
    let (sha256_now, size_bytes, bytes) = match hash_and_keep(&mut file, byte_cap) {
        Ok(read) => read,
        Err(message) => return empty(ReadState::Denied, message),
    };
    let recorded = json_string(record.get("sha256"));
    let (state, note) = match (recorded.as_deref(), sha256_now.as_deref()) {
        (Some(recorded), Some(now)) if recorded != now => (
            ReadState::Stale,
            Some("the file changed since it was referenced; both hashes are reported".to_string()),
        ),
        (_, None) => (
            ReadState::Ok,
            Some(format!(
                "file is over {REFERENCE_HASH_CAP_BYTES} bytes; staleness is not checked"
            )),
        ),
        _ => (ReadState::Ok, None),
    };
    ReferencedRead {
        state,
        note,
        bytes,
        size_bytes,
        sha256_now,
        filename,
        mime_type,
        path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_repo_is_detected_only_at_or_under_the_root_and_head_is_read_without_git() {
        let base = std::env::temp_dir().join(format!("garden-ref-repo-{}", Uuid::new_v4()));
        let root = base.join("root");
        std::fs::create_dir_all(root.join("repo/src")).unwrap();
        std::fs::create_dir_all(root.join("repo/.git/refs/heads")).unwrap();
        std::fs::write(root.join("repo/.git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(root.join("repo/.git/refs/heads/main"), "abc123def4567890abc123def4567890abc123de\n").unwrap();
        std::fs::write(root.join("repo/src/rooms.rs"), "fn x() {}").unwrap();
        std::fs::create_dir_all(base.join(".git")).unwrap();
        std::fs::write(base.join(".git/HEAD"), "ffff000011112222\n").unwrap();
        std::fs::create_dir_all(root.join("plain")).unwrap();
        std::fs::write(root.join("plain/notes.md"), "notes").unwrap();
        let root = std::fs::canonicalize(&root).unwrap();
        let roots = crate::artifact_roots::ArtifactRoots {
            version: 1,
            roots: vec![crate::artifact_roots::ArtifactRoot {
                path: root.to_string_lossy().to_string(),
                recursive: true,
                include_hidden: false,
                added_at: None,
            }],
        };
        let in_repo = crate::artifact_roots::authorize_against(&roots, &root.join("repo/src/rooms.rs")).unwrap();
        let repo = detect_repo(&in_repo).expect("repo under the root");
        assert_eq!(repo["relative"], "src/rooms.rs");
        assert_eq!(repo["commit"]["sha"], "abc123def4567890abc123def4567890abc123de");
        assert_eq!(repo["commit"]["ref"], "refs/heads/main");
        // The .git above the root is never consulted.
        let plain = crate::artifact_roots::authorize_against(&roots, &root.join("plain/notes.md")).unwrap();
        assert!(detect_repo(&plain).is_none());
        // A HEAD that points at an absolute path, or at a non-hex "sha",
        // yields no commit; the file it names is never read.
        let secret = base.join("secret.txt");
        std::fs::write(&secret, "TOP-SECRET").unwrap();
        std::fs::write(root.join("repo/.git/HEAD"), format!("ref: {}\n", secret.display())).unwrap();
        let repo = detect_repo(&in_repo).expect("still a repo");
        assert!(repo.get("commit").is_none(), "{repo}");
        std::fs::write(root.join("repo/.git/HEAD"), "ref: refs/heads/../../../secret.txt\n").unwrap();
        assert!(detect_repo(&in_repo).unwrap().get("commit").is_none());
        std::fs::write(root.join("repo/.git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(root.join("repo/.git/refs/heads/main"), "not a sha\n").unwrap();
        assert!(detect_repo(&in_repo).unwrap().get("commit").is_none());
        // A symlinked .git is not a repository here.
        std::fs::remove_dir_all(root.join("repo/.git")).unwrap();
        std::fs::create_dir_all(base.join("elsewhere/.git")).unwrap();
        std::os::unix::fs::symlink(base.join("elsewhere/.git"), root.join("repo/.git")).unwrap();
        assert!(detect_repo(&in_repo).is_none());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn hashing_keeps_a_window_and_reports_the_whole_length() {
        let dir = std::env::temp_dir().join(format!("garden-ref-hash-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f.bin");
        let data: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &data).unwrap();
        let mut file = std::fs::File::open(&path).unwrap();
        let (sha, len, kept) = hash_and_keep(&mut file, 100).unwrap();
        assert_eq!(len, 5000);
        assert_eq!(kept, &data[..100]);
        assert_eq!(sha.unwrap(), format!("{:x}", Sha256::digest(&data)));
        let _ = std::fs::remove_dir_all(dir);
    }
}
