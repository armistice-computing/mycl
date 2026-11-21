//! Offline, compare-and-set recovery of an accidentally empty shared Song head.
//! No HTTP endpoint, listener, scheduler, or queue replay is started.
use std::{fs, path::Path, sync::Arc};

fn fill_empty_head(mut store: crate::geist_song_store::LocalSongStore, verse: &str)
    -> Result<crate::geist_song_store::LocalSongStore, String>
{
    if verse.trim().is_empty() { return Err("replacement verse is empty".into()); }
    let head = store.verses.first_mut().ok_or("Song has no head")?;
    if !head.text.is_empty() || !head.counterpoints.is_empty() {
        return Err("refusing to replace a nonempty verse or its counterpoints".into());
    }
    head.text = verse.to_string();
    Ok(store)
}

/// Caller must retain a graph backup before running this explicit maintenance command.
/// The expected file is a byte-for-byte snapshot taken before the repair.
pub fn repair(graph_id: &str, expected: &Path, verse_file: &Path) -> Result<(), String> {
    crate::ids::validate_local_id(graph_id, "graph_id")?;
    let raw_profile = std::env::var("GARDEN_PROFILE_DIR").map_err(|_| "GARDEN_PROFILE_DIR required")?;
    let profile = Path::new(&raw_profile).canonicalize().map_err(|e| e.to_string())?;
    // All persistence paths must address the same cached RDF store, not two
    // aliases of it. Refuse relative/symlink paths before opening any store.
    if Path::new(&raw_profile) != profile {
        return Err("GARDEN_PROFILE_DIR must be its canonical absolute path".into());
    }
    let _owner = crate::profile_lock::ProfileLock::acquire(&profile)?;
    let graph_dir = profile.join("graphs").join(graph_id).canonicalize().map_err(|e| e.to_string())?;
    if !graph_dir.starts_with(&profile) { return Err("graph escapes profile".into()); }
    let path = graph_dir.join("narrative/song.json");
    let before = fs::read(&path).map_err(|e| e.to_string())?;
    if before != fs::read(expected).map_err(|e| e.to_string())? {
        return Err("Song changed since the captured snapshot; refusing repair".into());
    }
    let store: crate::geist_song_store::LocalSongStore = serde_json::from_slice(&before).map_err(|e| e.to_string())?;
    let raw: serde_json::Value = serde_json::from_slice(&before).map_err(|e| e.to_string())?;
    if serde_json::to_value(&store).map_err(|e| e.to_string())? != raw {
        return Err("Song contains unrepresented fields; refusing lossy recovery".into());
    }
    if store.graph_id != graph_id || !store.observer.is_empty() {
        return Err("snapshot is not this graph's shared Song".into());
    }
    let verse = fs::read_to_string(verse_file).map_err(|e| e.to_string())?;
    let fixed = fill_empty_head(store, &verse)?;
    let app = crate::app_runtime::App::new();
    crate::tauri_runtime::register_managed_state(app.handle(),
        Arc::new(crate::cell_graph_boundary::CellGraphBoundary::from_process_env()?));
    let _guard = crate::legacy_geist_import::mutation_guard()?;
    let warnings = crate::geist_song_store::persist_song_store(app.handle(), &graph_dir, &fixed)?;
    if !warnings.is_empty() {
        return Err(format!("authority repaired but projection needs attention: {warnings:?}"));
    }
    let actual: serde_json::Value = serde_json::from_slice(&fs::read(&path).map_err(|e|e.to_string())?).map_err(|e|e.to_string())?;
    let mut expected: serde_json::Value = serde_json::from_slice(&before).map_err(|e|e.to_string())?;
    expected["verses"][0]["text"] = serde_json::json!(verse);
    if actual != expected { return Err("post-repair authority verification failed".into()); }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repair_changes_only_empty_head_text() {
        let raw = serde_json::json!({"schemaVersion":1,"graphId":"g","observer":"",
            "verses":[{"text":"","counterpoints":[],"createdAt":"123","updatedAt":"123"},
                {"text":"prior","counterpoints":[],"createdAt":"100","updatedAt":"100"}],
            "coda":null,"archives":[]});
        let store = serde_json::from_value(raw.clone()).unwrap();
        let fixed = fill_empty_head(store, " exact recovered text ").unwrap();
        let actual = serde_json::to_value(&fixed).unwrap();
        let mut expected = raw;
        expected["verses"][0]["text"] = serde_json::json!(" exact recovered text ");
        assert_eq!(actual, expected);
        assert!(fill_empty_head(fixed, "second attempt").is_err());
    }
}
