//! Pending-byte reservations for the single owner process. Graph leases fence
//! admission/execution; this registry fences returned handles and copy workers.
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
};

const MAX_COUNT: usize = 1024;
const MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_ENTRY: u64 = 256 * 1024 * 1024;
pub(super) const QUOTA_ERROR: &str =
    "pending import quota reached; release unused handles or wait for execution, then retry";
static LIVE: LazyLock<Mutex<HashSet<PathBuf>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

pub(super) fn mutation_guard() -> Result<std::sync::MutexGuard<'static, HashSet<PathBuf>>, String> {
    LIVE.lock().map_err(|_| "pending custody poisoned".into())
}

pub(super) struct Custody(PathBuf);

/// Cheap hint only. False can postpone cleanup, never authorize deletion.
pub(super) fn has_unheld(root: &Path) -> Result<bool, String> {
    let live = LIVE.lock().map_err(|_| "pending custody poisoned")?;
    let root = match fs::canonicalize(root) {
        Ok(root) => root,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.to_string()),
    };
    for entry in fs::read_dir(root).map_err(|e| e.to_string())? {
        if !live.contains(&entry.map_err(|e| e.to_string())?.path()) {
            return Ok(true);
        }
    }
    Ok(false)
}
impl Drop for Custody {
    fn drop(&mut self) {
        // Poison deliberately disables future allocation/cleanup rather than
        // guessing which live handles survived a panic.
        if let Ok(mut live) = LIVE.lock() {
            live.remove(&self.0);
        }
    }
}

struct Entry {
    path: PathBuf,
    files: Vec<PathBuf>,
    bytes: u64,
}

fn inventory(root: &Path) -> Result<Vec<Entry>, String> {
    match fs::symlink_metadata(root) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e.to_string()),
        Ok(m) if !m.is_dir() => return Err("pending root is not a real directory".into()),
        Ok(_) => {}
    }
    let mut entries = Vec::new();
    for child in fs::read_dir(root).map_err(|e| e.to_string())? {
        if entries.len() >= 4096 {
            return Err("pending inventory exceeds safety bound".into());
        }
        let child = child.map_err(|e| e.to_string())?;
        let name = child
            .file_name()
            .into_string()
            .map_err(|_| "invalid pending name")?;
        if uuid::Uuid::parse_str(&name)
            .map_err(|_| "unknown pending directory")?
            .to_string()
            != name
            || !child.file_type().map_err(|e| e.to_string())?.is_dir()
        {
            return Err("unknown or linked pending directory".into());
        }
        let mut entry = Entry {
            path: child.path(),
            files: vec![],
            bytes: 0,
        };
        let mut reserved = None;
        for file in fs::read_dir(&entry.path).map_err(|e| e.to_string())? {
            if entry.files.len() >= 8 {
                return Err("pending directory has unexpected files".into());
            }
            let file = file.map_err(|e| e.to_string())?;
            let name = file
                .file_name()
                .into_string()
                .map_err(|_| "invalid pending filename")?;
            let meta = match fs::symlink_metadata(file.path()) {
                Ok(meta) => meta,
                // Failed copy workers can drop their temporary file outside
                // LIVE; their durable reservation still accounts for capacity.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && name.starts_with(".tmp") => {
                    continue
                }
                Err(e) => return Err(e.to_string()),
            };
            if !meta.is_file() {
                return Err("pending payload is not a regular file".into());
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if meta.nlink() != 1 {
                    return Err("pending payload is multiply linked".into());
                }
            }
            if let Some(size) = name.strip_prefix(".reserve-") {
                let size: u64 = size.parse().map_err(|_| "invalid pending reservation")?;
                if size > MAX_ENTRY || meta.len() != 0 || reserved.replace(size).is_some() {
                    return Err("invalid pending reservation".into());
                }
            } else if name != "body" && !name.starts_with(".tmp") {
                return Err("unknown pending payload file".into());
            }
            entry.bytes = entry
                .bytes
                .checked_add(meta.len())
                .ok_or("pending size overflow")?;
            entry.files.push(file.path());
        }
        entry.bytes = entry.bytes.max(reserved.unwrap_or(0));
        entries.push(entry);
    }
    Ok(entries)
}

pub(super) fn reserve(root: &Path, id: &str, bytes: u64) -> Result<Custody, String> {
    if !fs::symlink_metadata(root)
        .map_err(|e| e.to_string())?
        .is_dir()
    {
        return Err("pending root is not a real directory".into());
    }
    let root = &fs::canonicalize(root).map_err(|e| e.to_string())?;
    let mut live = LIVE.lock().map_err(|_| "pending custody poisoned")?;
    let entries = inventory(root)?;
    let used = entries.iter().try_fold(0u64, |n, e| {
        n.checked_add(e.bytes).ok_or("pending size overflow")
    })?;
    if bytes > MAX_ENTRY
        || entries.len() >= MAX_COUNT
        || used.checked_add(bytes).is_none_or(|n| n > MAX_BYTES)
    {
        return Err(QUOTA_ERROR.into());
    }
    super::fresh_preparation_directory(root, id)?;
    let path = root.join(id);
    // Filename encodes the reservation atomically: there is no partially
    // written JSON metadata to interpret after a killed copy worker.
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path.join(format!(".reserve-{bytes}")))
        .map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    crate::storage_atomic::sync_parent_dir(&path)?;
    live.insert(path.clone());
    Ok(Custody(path))
}

/// Caller holds the graph hot lease and supplies strict WAL ownership. The
/// owner process is exclusive for this profile; no second process may prepare.
pub(super) fn reap(root: &Path, pending: &HashSet<String>) -> Result<usize, String> {
    let live = LIVE.lock().map_err(|_| "pending custody poisoned")?;
    if !root.try_exists().map_err(|e| e.to_string())? {
        return Ok(0);
    }
    if !fs::symlink_metadata(root)
        .map_err(|e| e.to_string())?
        .is_dir()
    {
        return Err("pending root is not a real directory".into());
    }
    let root = &fs::canonicalize(root).map_err(|e| e.to_string())?;
    let entries = inventory(root)?; // validate everything before removing anything
    let mut count = 0;
    for entry in entries {
        let id = entry
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or("invalid pending name")?;
        if live.contains(&entry.path) || pending.contains(id) {
            continue;
        }
        for file in entry.files {
            fs::remove_file(file).map_err(|e| e.to_string())?;
        }
        fs::remove_dir(&entry.path).map_err(|e| e.to_string())?;
        count += 1;
    }
    if count > 0 {
        crate::storage_atomic::sync_parent_dir(root)?;
    }
    Ok(count)
}

/// Successful execution/discard already holds the graph lease. The active
/// handle may remain alive, but it can no longer need these bytes.
pub(super) fn remove_reservation(parent: &Path) -> Result<(), String> {
    for child in fs::read_dir(parent).map_err(|e| e.to_string())? {
        let child = child.map_err(|e| e.to_string())?;
        if child
            .file_name()
            .to_str()
            .is_some_and(|n| n.starts_with(".reserve-"))
        {
            if !child.file_type().map_err(|e| e.to_string())?.is_file() {
                return Err("invalid reservation file".into());
            }
            fs::remove_file(child.path()).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_and_journal_owners_survive_abandoned_copy_cleanup() {
        let root = tempfile::tempdir().unwrap();
        let live_id = uuid::Uuid::new_v4().to_string();
        let pending_id = uuid::Uuid::new_v4().to_string();
        let orphan_id = uuid::Uuid::new_v4().to_string();
        let live = reserve(root.path(), &live_id, 12).unwrap();
        drop(reserve(root.path(), &pending_id, 12).unwrap());
        drop(reserve(root.path(), &orphan_id, 12).unwrap());
        fs::write(
            root.path().join(&orphan_id).join(".tmpinterrupted"),
            b"partial",
        )
        .unwrap();
        let owners = HashSet::from([pending_id.clone()]);
        assert_eq!(reap(root.path(), &owners).unwrap(), 1);
        assert!(root.path().join(&live_id).exists());
        assert!(root.path().join(&pending_id).exists());
        drop(live);
        assert_eq!(reap(root.path(), &owners).unwrap(), 1);
        assert_eq!(reap(root.path(), &HashSet::new()).unwrap(), 1);
    }

    #[test]
    fn reservations_bound_bytes_before_copying_and_release_after_reaping() {
        let root = tempfile::tempdir().unwrap();
        let mut held = vec![];
        for _ in 0..8 {
            held.push(reserve(root.path(), &uuid::Uuid::new_v4().to_string(), MAX_ENTRY).unwrap());
        }
        assert!(reserve(root.path(), &uuid::Uuid::new_v4().to_string(), 1).is_err());
        assert_eq!(reap(root.path(), &HashSet::new()).unwrap(), 0);
        drop(held);
        assert_eq!(reap(root.path(), &HashSet::new()).unwrap(), 8);
        assert!(reserve(root.path(), &uuid::Uuid::new_v4().to_string(), 1).is_ok());
    }

    #[test]
    fn unknown_contents_refuse_entire_cleanup_and_allocation() {
        let root = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        drop(reserve(root.path(), &id, 12).unwrap());
        let unknown = root.path().join(&id).join("user-file");
        fs::write(&unknown, b"keep").unwrap();
        assert!(reap(root.path(), &HashSet::new()).is_err());
        assert!(reserve(root.path(), &uuid::Uuid::new_v4().to_string(), 1).is_err());
        assert_eq!(fs::read(unknown).unwrap(), b"keep");
    }

    #[test]
    fn concurrent_reservations_cannot_overbook_the_byte_limit() {
        let root = tempfile::tempdir().unwrap();
        let threads: Vec<_> = (0..16)
            .map(|_| {
                let root = root.path().to_owned();
                std::thread::spawn(move || {
                    reserve(&root, &uuid::Uuid::new_v4().to_string(), MAX_ENTRY)
                })
            })
            .collect();
        let held: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert_eq!(held.iter().filter(|r| r.is_ok()).count(), 8);
        assert_eq!(held.iter().filter(|r| r.is_err()).count(), 8);
    }

    #[test]
    fn publishing_and_inventory_can_run_concurrently() {
        use sha2::{Digest, Sha256};
        let root = tempfile::tempdir().unwrap();
        let path = root.path().to_owned();
        let writer = std::thread::spawn(move || {
            let data = vec![42u8; 16384];
            let hash = format!("{:x}", Sha256::digest(&data));
            for _ in 0..50 {
                let id = uuid::Uuid::new_v4().to_string();
                let _custody = reserve(&path, &id, data.len() as u64).unwrap();
                crate::vault_adoption::publish_owned_payload_guarded(
                    &mut std::io::Cursor::new(&data),
                    &path.join(id).join("body"),
                    data.len() as u64,
                    &hash,
                    mutation_guard,
                )
                .unwrap();
            }
        });
        for _ in 0..100 {
            drop(reserve(root.path(), &uuid::Uuid::new_v4().to_string(), 1).unwrap());
        }
        writer.join().unwrap();
        assert_eq!(reap(root.path(), &HashSet::new()).unwrap(), 150);
    }

    #[test]
    fn zero_byte_preparations_are_still_count_bounded() {
        let root = tempfile::tempdir().unwrap();
        for _ in 0..MAX_COUNT {
            fs::create_dir(root.path().join(uuid::Uuid::new_v4().to_string())).unwrap();
        }
        assert!(reserve(root.path(), &uuid::Uuid::new_v4().to_string(), 0).is_err());
        assert_eq!(reap(root.path(), &HashSet::new()).unwrap(), MAX_COUNT);
        assert!(reserve(root.path(), &uuid::Uuid::new_v4().to_string(), 0).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn linked_payload_is_never_followed_or_removed() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        drop(reserve(root.path(), &id, 12).unwrap());
        let body = root.path().join(&id).join("body");
        std::os::unix::fs::symlink(outside.path(), &body).unwrap();
        assert!(reap(root.path(), &HashSet::new()).is_err());
        assert!(body.is_symlink());
        assert!(outside.path().exists());
    }
}
