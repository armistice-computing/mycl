//! When the owner last opened each document, per graph.
//!
//! The record lives in the profile (`<profile>/human-activity/<graph>.json`),
//! not in the graph directory, so graph exports, snapshots and duplicates never
//! carry it. Only the owner's session writes it (the loopback route is
//! owner-only): an agent reading a document is not the human opening it.

use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Mutex,
};

use crate::ids::validate_local_id;

const ACTIVITY_DIR: &str = "human-activity";
const ACTIVITY_VERSION: u32 = 1;
/// Most documents remembered per graph; the least recently opened go first.
pub(crate) const MAX_ACTIVITY_ENTRIES: usize = 5_000;
/// A second open of the same document within this window is not counted, so
/// a restore, a back-and-forth or a double click does not inflate the record.
pub(crate) const OPEN_COALESCE_MILLIS: u64 = 30_000;

static ACTIVITY_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OpenRecord {
    /// Epoch milliseconds.
    pub(crate) last_opened_at: u64,
    pub(crate) open_count: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActivityFile {
    version: u32,
    #[serde(default)]
    documents: BTreeMap<String, OpenRecord>,
}

fn activity_path(profile_dir: &Path, graph_id: &str) -> Result<PathBuf, String> {
    validate_local_id(graph_id, "graph_id")?;
    Ok(profile_dir
        .join(ACTIVITY_DIR)
        .join(format!("{graph_id}.json")))
}

/// Read the file; a missing file is empty. A file that will not parse is set
/// aside (renamed `.corrupt`) and treated as empty, so one bad write can never
/// stop opens from being recorded again.
fn read_activity(path: &Path) -> ActivityFile {
    if !path.is_file() {
        return ActivityFile::default();
    }
    match crate::storage::read_json::<ActivityFile>(path) {
        Ok(file) => file,
        Err(error) => {
            log::warn!(
                "human activity: setting aside unreadable {}: {error}",
                path.display()
            );
            let _ = std::fs::rename(path, path.with_extension("json.corrupt"));
            ActivityFile::default()
        }
    }
}

/// Record that the owner opened `document_id` at `now_millis`. Returns whether
/// anything was recorded (false when coalesced with an open moments ago).
///
/// `graph_present` is asked again under the activity lock, just before the
/// write. A hard delete removes the graph and THEN forgets its activity under
/// this same lock, so an open racing a delete either lands first (and the
/// forget removes it) or finds the graph gone: it can never recreate the
/// record after the forget, for a graph later made with the same id.
pub(crate) fn record_opened(
    profile_dir: &Path,
    graph_id: &str,
    document_id: &str,
    now_millis: u64,
    graph_present: impl FnOnce() -> bool,
) -> Result<bool, String> {
    validate_local_id(document_id, "documentId")?;
    let path = activity_path(profile_dir, graph_id)?;
    let _guard = ACTIVITY_LOCK
        .lock()
        .map_err(|_| "human activity lock poisoned".to_string())?;
    if !graph_present() {
        return Err(format!("graph not found: {graph_id}"));
    }
    let mut file = read_activity(&path);
    if let Some(existing) = file.documents.get(document_id) {
        if now_millis.saturating_sub(existing.last_opened_at) < OPEN_COALESCE_MILLIS {
            return Ok(false);
        }
    }
    let record = file
        .documents
        .entry(document_id.to_string())
        .or_insert(OpenRecord {
            last_opened_at: now_millis,
            open_count: 0,
        });
    record.last_opened_at = now_millis;
    record.open_count += 1;
    if file.documents.len() > MAX_ACTIVITY_ENTRIES {
        let mut by_age: Vec<(u64, String)> = file
            .documents
            .iter()
            .map(|(id, record)| (record.last_opened_at, id.clone()))
            .collect();
        by_age.sort();
        let excess = file.documents.len() - MAX_ACTIVITY_ENTRIES;
        for (_, id) in by_age.into_iter().take(excess) {
            file.documents.remove(&id);
        }
    }
    file.version = ACTIVITY_VERSION;
    if let Some(dir) = path.parent() {
        crate::storage::create_dir_all(dir).map_err(|error| error.to_string())?;
    }
    crate::storage::write_json(&path, &file).map_err(|error| error.to_string())?;
    Ok(true)
}

/// Forget a graph's activity when the graph is hard-deleted: it is a
/// personal read log, and a graph later created with the same id must not
/// inherit it. A missing file is fine.
pub(crate) fn forget_graph(profile_dir: &Path, graph_id: &str) -> Result<(), String> {
    let path = activity_path(profile_dir, graph_id)?;
    let _guard = ACTIVITY_LOCK
        .lock()
        .map_err(|_| "human activity lock poisoned".to_string())?;
    crate::storage::remove_file_if_exists(&path)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// Every recorded open, most recent first.
pub(crate) fn opened_documents(
    profile_dir: &Path,
    graph_id: &str,
) -> Result<Vec<(String, OpenRecord)>, String> {
    let path = activity_path(profile_dir, graph_id)?;
    let _guard = ACTIVITY_LOCK
        .lock()
        .map_err(|_| "human activity lock poisoned".to_string())?;
    let mut entries: Vec<(String, OpenRecord)> =
        read_activity(&path).documents.into_iter().collect();
    entries.sort_by(|a, b| {
        b.1.last_opened_at
            .cmp(&a.1.last_opened_at)
            .then_with(|| a.0.cmp(&b.0))
    });
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Profile(PathBuf);
    impl Profile {
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for Profile {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn dir() -> Profile {
        let path = std::env::temp_dir().join(format!("human-activity-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("temp profile");
        Profile(path)
    }

    #[test]
    fn opens_are_recorded_counted_and_listed_newest_first() {
        let profile = dir();
        let p = profile.path();
        assert!(record_opened(p, "g", "a", 1_000_000, || true).unwrap());
        assert!(record_opened(p, "g", "b", 2_000_000, || true).unwrap());
        assert!(record_opened(p, "g", "a", 3_000_000, || true).unwrap());
        let listed = opened_documents(p, "g").unwrap();
        assert_eq!(
            listed,
            vec![
                (
                    "a".to_string(),
                    OpenRecord {
                        last_opened_at: 3_000_000,
                        open_count: 2
                    }
                ),
                (
                    "b".to_string(),
                    OpenRecord {
                        last_opened_at: 2_000_000,
                        open_count: 1
                    }
                ),
            ]
        );
        // Another graph keeps its own record.
        assert!(opened_documents(p, "other").unwrap().is_empty());
    }

    #[test]
    fn a_second_open_within_the_window_is_not_counted() {
        let profile = dir();
        let p = profile.path();
        assert!(record_opened(p, "g", "a", 1_000_000, || true).unwrap());
        assert!(!record_opened(p, "g", "a", 1_000_000 + OPEN_COALESCE_MILLIS - 1, || true).unwrap());
        assert!(record_opened(p, "g", "a", 1_000_000 + OPEN_COALESCE_MILLIS, || true).unwrap());
        let listed = opened_documents(p, "g").unwrap();
        assert_eq!(listed[0].1.open_count, 2);
    }

    #[test]
    fn the_least_recently_opened_go_first_past_the_cap() {
        let profile = dir();
        let p = profile.path();
        let mut file = ActivityFile {
            version: 1,
            documents: BTreeMap::new(),
        };
        for index in 0..MAX_ACTIVITY_ENTRIES as u64 {
            file.documents.insert(
                format!("d{index}"),
                OpenRecord {
                    last_opened_at: 10_000_000 + index * 100_000,
                    open_count: 1,
                },
            );
        }
        let path = activity_path(p, "g").unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        crate::storage::write_json(&path, &file).unwrap();
        assert!(record_opened(p, "g", "newest", u64::MAX / 2, || true).unwrap());
        let listed = opened_documents(p, "g").unwrap();
        assert_eq!(listed.len(), MAX_ACTIVITY_ENTRIES);
        assert_eq!(listed[0].0, "newest");
        assert!(
            !listed.iter().any(|(id, _)| id == "d0"),
            "the oldest was dropped"
        );
    }

    #[test]
    fn a_deleted_graph_leaves_no_activity_behind() {
        let profile = dir();
        let p = profile.path();
        assert!(record_opened(p, "g", "a", 1_000_000, || true).unwrap());
        assert!(record_opened(p, "other", "a", 1_000_000, || true).unwrap());
        forget_graph(p, "g").unwrap();
        assert!(opened_documents(p, "g").unwrap().is_empty());
        assert_eq!(opened_documents(p, "other").unwrap().len(), 1);
        forget_graph(p, "g").expect("forgetting twice is fine");
    }

    #[test]
    fn an_open_after_its_graph_is_gone_records_nothing() {
        let profile = dir();
        let p = profile.path();
        assert!(record_opened(p, "g", "a", 1_000_000, || false).is_err());
        assert!(opened_documents(p, "g").unwrap().is_empty());
        // A delete's forget, then a late open: still nothing comes back.
        assert!(record_opened(p, "g", "a", 1_000_000, || true).unwrap());
        forget_graph(p, "g").unwrap();
        assert!(record_opened(p, "g", "a", 2_000_000, || false).is_err());
        assert!(opened_documents(p, "g").unwrap().is_empty());
    }

    #[test]
    fn ids_that_could_leave_the_directory_are_refused() {
        let profile = dir();
        let p = profile.path();
        assert!(record_opened(p, "../escape", "a", 1, || true).is_err());
        assert!(record_opened(p, "g", "../a", 1, || true).is_err());
        assert!(opened_documents(p, "..").is_err());
    }

    #[test]
    fn an_unreadable_file_is_set_aside_and_recording_continues() {
        let profile = dir();
        let p = profile.path();
        let path = activity_path(p, "g").unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ not json").unwrap();
        assert!(record_opened(p, "g", "a", 5_000_000, || true).unwrap());
        assert_eq!(opened_documents(p, "g").unwrap().len(), 1);
        assert!(path.with_extension("json.corrupt").is_file());
    }
}
