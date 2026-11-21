//! What a person keeps out of their dreams, per graph: folders (and
//! everything under them) and single documents. Empty by default: a graph
//! dreams on everything (Eschaton, 2026-10-02). This is setup, not a
//! mechanic, so it sits outside the agree-to-notice gate.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Exclusions {
    #[serde(default)]
    pub(crate) folders: Vec<String>,
    #[serde(default)]
    pub(crate) documents: Vec<String>,
}

pub(crate) fn exclusions_path(graph_dir: &Path) -> PathBuf {
    graph_dir.join("dreaming").join("exclusions.json")
}

pub(crate) fn read_reported(graph_dir: &Path) -> (Exclusions, Option<String>) {
    let path = exclusions_path(graph_dir);
    if !path.is_file() {
        return (Exclusions::default(), None);
    }
    match crate::storage::read_json::<Exclusions>(&path) {
        Ok(exclusions) => (exclusions, None),
        Err(error) => (Exclusions::default(), Some(error.to_string())),
    }
}

/// The exclusions a cycle runs with. Absent is "nothing excluded". A file
/// that exists but can't be read or parsed is an error: a cycle must not
/// treat a broken privacy setting as "nothing excluded".
pub(crate) fn read_for_cycle(graph_dir: &Path) -> Result<Exclusions, String> {
    let path = exclusions_path(graph_dir);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Exclusions::default()),
        Err(error) => {
            return Err(format!("the dream exclusions file can't be read; nothing dreams until it is fixed or reset: {error}"))
        }
    };
    serde_json::from_str(&text)
        .map_err(|e| format!("the dream exclusions file is malformed; nothing dreams until it is fixed or reset: {e}"))
}

/// Every document the exclusions cover right now. Folder exclusions follow
/// the current folder tree, so a document moved into an excluded folder is
/// covered at once. The tree is read from both the live workspace Y.Doc
/// state file (rewritten under the graph lease when the app moves
/// something) and the workspace snapshot, and a document excluded by
/// either counts as excluded.
pub(crate) fn current_closure(graph_dir: &Path, graph_id: &str) -> Result<HashSet<String>, String> {
    let exclusions = read_for_cycle(graph_dir)?;
    if exclusions.folders.is_empty() {
        return Ok(exclusions.documents.into_iter().collect());
    }
    let workspaces = current_workspaces(graph_dir, graph_id)?;
    if workspaces.is_empty() {
        // Unknown, not empty: without the folder tree, folder exclusions
        // can't be honored, so nothing may be sent.
        return Err("the graph's folder tree can't be read, so excluded folders can't be honored; nothing dreams until it can".into());
    }
    let mut out = HashSet::new();
    for snapshot in workspaces {
        let documents = crate::workspace_entity_projection::workspace_documents(Some(&snapshot));
        let folders = crate::workspace_entity_projection::workspace_folders(Some(&snapshot));
        out.extend(excluded_documents(&exclusions, &documents, &folders));
    }
    out.extend(exclusions.documents);
    Ok(out)
}

/// A workspace that can be trusted as a folder tree: an object whose
/// documents and folders are arrays. Anything else is unknown.
fn usable(snapshot: &Value) -> bool {
    snapshot.get("documents").is_some_and(Value::is_array) && snapshot.get("folders").is_some_and(Value::is_array)
}

/// Every readable, well-formed representation of the current tree.
fn current_workspaces(graph_dir: &Path, graph_id: &str) -> Result<Vec<Value>, String> {
    use yrs::{updates::decoder::Decode, Transact};
    let mut out = Vec::new();
    match std::fs::read(crate::ydoc_paths::workspace_ydoc_state_path(graph_dir)) {
        Ok(bytes) if !bytes.is_empty() => {
            let doc = yrs::Doc::new();
            let update = yrs::Update::decode_v1(&bytes).map_err(|e| format!("decode the workspace: {e}"))?;
            doc.transact_mut().apply_update(update).map_err(|e| format!("read the workspace: {e}"))?;
            let snapshot = crate::crdt_engine::workspace_ops::materialize_workspace_snapshot_json(graph_id, &doc)?;
            if usable(&snapshot) {
                out.push(snapshot);
            }
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("read the workspace: {error}")),
    }
    if let Some(snapshot) = crate::workspace_record_store::read_workspace_record(graph_dir, graph_id)?.snapshot {
        if usable(&snapshot) {
            out.push(snapshot);
        }
    }
    Ok(out)
}

pub(crate) fn write(graph_dir: &Path, exclusions: &Exclusions) -> Result<(), String> {
    for id in exclusions.folders.iter().chain(&exclusions.documents) {
        crate::ids::validate_local_id(id, "excluded id")?;
    }
    let path = exclusions_path(graph_dir);
    crate::storage::create_dir_all(path.parent().ok_or("exclusions path")?).map_err(|e| e.to_string())?;
    crate::storage::write_json(&path, exclusions).map_err(|e| e.to_string())
}

fn text<'a>(entity: &'a Value, key: &str) -> Option<&'a str> {
    entity.get(key).and_then(Value::as_str)
}

/// Every document the exclusions cover, following each document's folder
/// chain up to the root. A cycle in the folder tree stops the walk.
pub(crate) fn excluded_documents(exclusions: &Exclusions, documents: &[Value], folders: &[Value]) -> HashSet<String> {
    let mut out: HashSet<String> = exclusions.documents.iter().cloned().collect();
    if exclusions.folders.is_empty() {
        return out;
    }
    let excluded: HashSet<&str> = exclusions.folders.iter().map(String::as_str).collect();
    let parent_of: HashMap<&str, &str> = folders
        .iter()
        .filter_map(|f| Some((text(f, "id")?, text(f, "parentId")?)))
        .collect();
    for doc in documents {
        let Some(id) = text(doc, "id") else { continue };
        let mut at = text(doc, "parentId");
        let mut steps = 0;
        while let Some(folder) = at {
            if excluded.contains(folder) {
                out.insert(id.to_string());
                break;
            }
            steps += 1;
            if steps > folders.len() {
                break;
            }
            at = parent_of.get(folder).copied();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_folder_excludes_everything_under_it() {
        let folders = vec![
            json!({"id": "f-private", "parentId": null}),
            json!({"id": "f-inner", "parentId": "f-private"}),
            json!({"id": "f-open", "parentId": null}),
        ];
        let documents = vec![
            json!({"id": "d-top", "parentId": "f-private"}),
            json!({"id": "d-deep", "parentId": "f-inner"}),
            json!({"id": "d-open", "parentId": "f-open"}),
            json!({"id": "d-root", "parentId": null}),
            json!({"id": "d-single", "parentId": null}),
        ];
        let exclusions = Exclusions { folders: vec!["f-private".into()], documents: vec!["d-single".into()] };
        let mut got: Vec<String> = excluded_documents(&exclusions, &documents, &folders).into_iter().collect();
        got.sort();
        assert_eq!(got, ["d-deep", "d-single", "d-top"]);
        assert!(excluded_documents(&Exclusions::default(), &documents, &folders).is_empty());
    }

    #[test]
    fn a_broken_exclusions_file_stops_a_cycle_instead_of_excluding_nothing() {
        let dir = std::env::temp_dir().join(format!("dream-excl-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("dreaming")).unwrap();
        assert_eq!(read_for_cycle(&dir).unwrap(), Exclusions::default());
        std::fs::write(exclusions_path(&dir), "{\"documents\": [\"d\"").unwrap();
        assert!(read_for_cycle(&dir).is_err());
        std::fs::remove_file(exclusions_path(&dir)).unwrap();
        std::fs::create_dir(exclusions_path(&dir)).unwrap();
        assert!(read_for_cycle(&dir).is_err());
        std::fs::remove_dir(exclusions_path(&dir)).unwrap();
        write(&dir, &Exclusions { folders: vec![], documents: vec!["d".into()] }).unwrap();
        assert_eq!(read_for_cycle(&dir).unwrap().documents, vec!["d".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn folder_exclusions_without_a_readable_tree_refuse_instead_of_excluding_nothing() {
        let dir = std::env::temp_dir().join(format!("dream-tree-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("graph.json"), r#"{"graphId":"g","title":"g","origin":"local","providerId":"local","localPath":"","createdAt":"0","updatedAt":"0","capabilities":[]}"#).unwrap();
        // Document exclusions alone need no tree.
        write(&dir, &Exclusions { folders: vec![], documents: vec!["d".into()] }).unwrap();
        assert_eq!(current_closure(&dir, "g").unwrap(), HashSet::from(["d".to_string()]));
        // A folder exclusion with both workspace representations missing.
        write(&dir, &Exclusions { folders: vec!["private".into()], documents: vec![] }).unwrap();
        assert!(current_closure(&dir, "g").is_err());
        // A snapshot that isn't a tree is no better.
        let snapshot = crate::paths::workspace_snapshot_path(&dir);
        std::fs::create_dir_all(snapshot.parent().unwrap()).unwrap();
        for broken in ["null", r#"{"documents": null, "folders": []}"#, r#"{"folders": []}"#] {
            std::fs::write(&snapshot, broken).unwrap();
            assert!(current_closure(&dir, "g").is_err(), "{broken}");
        }
        // A real tree is honored.
        std::fs::write(&snapshot, r#"{"documents": [{"id": "p", "parentId": "private"}, {"id": "q", "parentId": null}], "folders": []}"#).unwrap();
        assert_eq!(current_closure(&dir, "g").unwrap(), HashSet::from(["p".to_string()]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_folder_cycle_does_not_hang() {
        let folders = vec![json!({"id": "a", "parentId": "b"}), json!({"id": "b", "parentId": "a"})];
        let documents = vec![json!({"id": "d", "parentId": "a"})];
        let exclusions = Exclusions { folders: vec!["z".into()], documents: vec![] };
        assert!(excluded_documents(&exclusions, &documents, &folders).is_empty());
    }
}
