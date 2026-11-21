//! One shared search snapshot, not a second authoritative index. Replacements,
//! in-place edits and deletions are checked on every read; callers retain an Arc.
use crate::{paths::semantic_index_path, semantic_index::SemanticIndexFile};
use std::{
    fs::{self, File, Metadata},
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::SystemTime,
};

// Bound retention to one graph and reject caching unusually large files. This
// is a serialized-size admission limit, not a precise Rust heap-size bound.
const MAX_CACHED_FILE_BYTES: u64 = 768 * 1024 * 1024;
#[derive(Debug, PartialEq, Eq)]
struct Stamp {
    len: u64,
    modified: SystemTime,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
}
fn stamp(meta: Metadata) -> Result<Stamp, String> {
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    Ok(Stamp {
        len: meta.len(),
        modified: meta.modified().map_err(|e| e.to_string())?,
        #[cfg(unix)]
        identity: (meta.dev(), meta.ino(), meta.ctime(), meta.ctime_nsec()),
    })
}
const MISSING_INDEX: &str = "semantic index not found; run refresh_semantic_index first";
fn io_error(path: &Path, error: std::io::Error) -> String {
    if error.kind() == std::io::ErrorKind::NotFound {
        MISSING_INDEX.to_string()
    } else {
        format!("read semantic index {}: {error}", path.display())
    }
}
fn file_stamp(path: &Path, metadata: std::io::Result<Metadata>) -> Result<Stamp, String> {
    stamp(metadata.map_err(|e| io_error(path, e))?)
        .map_err(|e| format!("stat semantic index {}: {e}", path.display()))
}
struct Entry {
    path: PathBuf,
    stamp: Stamp,
    index: Arc<SemanticIndexFile>,
}
#[derive(Default)]
struct Cache {
    entry: Option<Entry>,
}
impl Cache {
    fn read(&mut self, path: &Path) -> Result<Arc<SemanticIndexFile>, String> {
        let current = match file_stamp(path, fs::metadata(path)) {
            Ok(stamp) => stamp,
            Err(error) => {
                self.entry = None;
                return Err(error);
            }
        };
        if let Some(entry) = &self.entry {
            if entry.path == path && entry.stamp == current {
                crate::search_timing::event("index_cache_hit");
                return Ok(entry.index.clone());
            }
        }
        // Do not retain the previous graph/generation while allocating a new one.
        crate::search_timing::event("index_cache_miss");
        let _load = crate::search_timing::stage("index_read_parse");
        self.entry = None;
        for _ in 0..2 {
            let mut file = File::open(path).map_err(|e| io_error(path, e))?;
            let before = file_stamp(path, file.metadata())?;
            let mut data = String::new();
            file.read_to_string(&mut data)
                .map_err(|e| io_error(path, e))?;
            let parsed = serde_json::from_str::<SemanticIndexFile>(&data);
            let after = file_stamp(path, file.metadata())?;
            let published = file_stamp(path, fs::metadata(path))?;
            if before != after || after != published {
                continue;
            }
            let index = Arc::new(
                parsed.map_err(|e| format!("parse semantic index {}: {e}", path.display()))?,
            );
            if published.len <= MAX_CACHED_FILE_BYTES {
                self.entry = Some(Entry {
                    path: path.to_owned(),
                    stamp: published,
                    index: index.clone(),
                });
            }
            return Ok(index);
        }
        Err("semantic index changed while loading; retry search".into())
    }
}
pub(crate) fn read_cached_index(graph_dir: &Path) -> Result<Arc<SemanticIndexFile>, String> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    let waiting = crate::search_timing::stage("index_cache_lock_wait");
    let mut cache = CACHE.get_or_init(|| Mutex::new(Cache::default())).lock()
        .map_err(|_| "semantic search cache lock poisoned".to_string())?;
    drop(waiting);
    cache.read(&semantic_index_path(graph_dir))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn write(path: &Path, date: &str) {
        let value = serde_json::json!({"manifest": {"schemaVersion":1,"graphId":"g","providerId":"candle","modelId":"test","dimensions":1,"blockCount":0,"documentCount":0,"indexedAt":date,"indexPath":"blocks.json"},"blocks":[],"entities":[]});
        fs::write(path, value.to_string()).unwrap();
    }
    #[test]
    fn cache_reuses_replaces_isolates_and_fails_closed() {
        let dir = std::env::temp_dir().join(format!("search-cache-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.json");
        let b = dir.join("b.json");
        write(&a, "1");
        write(&b, "2");
        let mut cache = Cache::default();
        let first = cache.read(&a).unwrap();
        assert!(Arc::ptr_eq(&first, &cache.read(&a).unwrap()));
        // Same-length atomic replacement must invalidate even at one timestamp tick.
        let replacement = dir.join("replacement");
        write(&replacement, "3");
        fs::rename(replacement, &a).unwrap();
        assert_eq!(cache.read(&a).unwrap().manifest.indexed_at, "3");
        assert_eq!(first.manifest.indexed_at, "1");
        assert_eq!(cache.read(&b).unwrap().manifest.indexed_at, "2");
        assert_eq!(cache.entry.as_ref().unwrap().path, b);
        fs::write(&b, "broken").unwrap();
        assert!(cache
            .read(&b)
            .unwrap_err()
            .contains(&format!("parse semantic index {}:", b.display())));
        assert!(cache.entry.is_none());
        write(&b, "4");
        assert_eq!(cache.read(&b).unwrap().manifest.indexed_at, "4");
        fs::remove_file(&b).unwrap();
        assert_eq!(cache.read(&b).unwrap_err(), MISSING_INDEX);
        assert!(cache.entry.is_none());
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    #[ignore = "read-only benchmark against an explicitly selected local graph"]
    fn benchmark_local_snapshot_cache() {
        let graph =
            std::env::var("GARDEN_SEARCH_BENCH_GRAPH").expect("explicit graph directory required");
        let path = semantic_index_path(Path::new(&graph));
        let mut cache = Cache::default();
        let start = std::time::Instant::now();
        let cold = cache.read(&path).unwrap();
        eprintln!(
            "cold parse: {:?}, blocks: {}",
            start.elapsed(),
            cold.blocks.len()
        );
        let start = std::time::Instant::now();
        for _ in 0..100 {
            assert!(Arc::ptr_eq(&cold, &cache.read(&path).unwrap()));
        }
        eprintln!("100 warm cache lookups: {:?}", start.elapsed());
    }
}
