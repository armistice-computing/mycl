use std::collections::{HashMap, HashSet};

use crate::app_runtime::AppHandle;
use crate::{
    crdt_projection_flush::flush_graph_projection,
    document_service::{read_graph_documents_cold, read_workspace_record},
    graph_service::GraphRecord,
    mcp_utils::{mcp_arg_bool, mcp_arg_string, mcp_arg_usize, mcp_required_graph_id},
    paths::existing_graph_dir,
    storage::read_json,
    workspace_document_projection::{
        fallback_workspace_documents, merge_workspace_document_counts,
    },
};

const DEFAULT_WORKSPACE_LIMIT: usize = 50;
const MAX_WORKSPACE_LIMIT: usize = 200;
const DEFAULT_WORKSPACE_BYTES: usize = 64 * 1024;
const MIN_WORKSPACE_BYTES: usize = 16 * 1024;
const MAX_WORKSPACE_BYTES: usize = 256 * 1024;
const MAX_WORKSPACE_DEPTH: usize = 5;

pub(super) async fn mcp_local_get_workspace(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_required_graph_id(arguments)?;
    flush_graph_projection(app.clone(), &graph_id).await?;

    let graph_dir = existing_graph_dir(&app, &graph_id)?;
    let graph = read_json::<GraphRecord>(&graph_dir.join("graph.json"))?;
    let workspace = read_workspace_record(&graph_dir, &graph_id)?;
    let document_records = read_graph_documents_cold(&graph_dir)?;
    let fallback_documents = fallback_workspace_documents(&document_records);
    let snapshot = workspace.snapshot.clone();
    let folders = snapshot_array(snapshot.as_ref(), "folders");
    let mut documents = snapshot_array(snapshot.as_ref(), "documents");
    if documents.is_empty() {
        documents = fallback_documents;
    }
    let artifacts = snapshot_array(snapshot.as_ref(), "artifacts");
    let wires = snapshot_array(snapshot.as_ref(), "wires");
    merge_workspace_document_counts(&mut documents, &document_records);
    crate::document_access::enrich_workspace_documents(
        &graph_dir,
        &mut documents,
        &document_records,
    )?;

    project_workspace_page(
        &graph,
        &workspace,
        &folders,
        &documents,
        &artifacts,
        wires.len(),
        arguments,
    )
}

fn project_workspace_page(
    graph: &GraphRecord,
    workspace: &crate::document_service::WorkspaceRecord,
    folders: &[serde_json::Value],
    documents: &[serde_json::Value],
    artifacts: &[serde_json::Value],
    wire_count: usize,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    // Zero is unlimited; positive depths retain the existing safety cap.
    let depth = mcp_arg_usize(arguments, &["depth"], 1).min(MAX_WORKSPACE_DEPTH);
    let limit =
        mcp_arg_usize(arguments, &["limit"], DEFAULT_WORKSPACE_LIMIT).clamp(1, MAX_WORKSPACE_LIMIT);
    let max_bytes = mcp_arg_usize(
        arguments,
        &["maxBytes", "max_bytes"],
        DEFAULT_WORKSPACE_BYTES,
    )
    .clamp(MIN_WORKSPACE_BYTES, MAX_WORKSPACE_BYTES);
    let cursor = workspace_cursor(arguments)?;
    let root_folder = mcp_arg_string(arguments, &["folderId", "folder_id"]);
    let folders_only = mcp_arg_bool(arguments, &["foldersOnly", "folders_only"], false);
    let folder_parents = folder_parent_map(folders);

    let mut eligible = Vec::new();
    for folder in folders {
        if let Some(relative_depth) =
            workspace_entry_depth(folder, root_folder.as_deref(), &folder_parents)
        {
            if depth != 0 && relative_depth > depth {
                continue;
            }
            let mut compact = compact_workspace_entry("folder", folder);
            if depth != 0 && relative_depth == depth {
                if let Some(folder_id) = workspace_entry_id(folder) {
                    let (nested_folders, nested_documents) =
                        descendant_counts(folder_id, folders, documents, &folder_parents);
                    compact["collapsed"] = serde_json::json!({
                        "folders": nested_folders,
                        "documents": nested_documents,
                    });
                }
            }
            eligible.push(compact);
        }
    }
    if !folders_only {
        for document in documents {
            if visible_at_depth(document, root_folder.as_deref(), depth, &folder_parents) {
                eligible.push(compact_workspace_entry("document", document));
            }
        }
        for artifact in artifacts {
            if visible_at_depth(artifact, root_folder.as_deref(), depth, &folder_parents) {
                eligible.push(compact_workspace_entry("artifact", artifact));
            }
        }
    }
    eligible.sort_by_key(workspace_entry_sort_key);

    let total_eligible = eligible.len();
    let start = cursor.min(total_eligible);
    let mut page_entries = Vec::new();
    let mut next_offset = start;
    for entry in eligible.into_iter().skip(start).take(limit) {
        let mut candidate = page_entries.clone();
        candidate.push(entry.clone());
        let candidate_next = start + candidate.len();
        let candidate_value = workspace_page_value(
            graph,
            workspace,
            workspace.snapshot.is_some(),
            depth,
            root_folder.as_deref(),
            candidate,
            total_eligible,
            candidate_next,
            limit,
            max_bytes,
            folders.len(),
            documents.len(),
            artifacts.len(),
            wire_count,
        );
        let (_, candidate_bytes) = finalize_serialized_bytes(candidate_value)?;
        if candidate_bytes > max_bytes {
            break;
        }
        page_entries.push(entry);
        next_offset = candidate_next;
    }

    let response = workspace_page_value(
        graph,
        workspace,
        workspace.snapshot.is_some(),
        depth,
        root_folder.as_deref(),
        page_entries,
        total_eligible,
        next_offset,
        limit,
        max_bytes,
        folders.len(),
        documents.len(),
        artifacts.len(),
        wire_count,
    );
    let (response, _) = finalize_serialized_bytes(response)?;
    Ok(response)
}

fn finalize_serialized_bytes(
    mut value: serde_json::Value,
) -> Result<(serde_json::Value, usize), String> {
    // The byte count is part of the payload, so setting it can change the
    // payload's size. Iterate until the stored count and actual JSON size agree.
    loop {
        let bytes = serialized_bytes(&value)?;
        if value["page"]["serializedBytes"].as_u64() == Some(bytes as u64) {
            return Ok((value, bytes));
        }
        value["page"]["serializedBytes"] = serde_json::json!(bytes);
    }
}

#[allow(clippy::too_many_arguments)]
fn workspace_page_value(
    graph: &GraphRecord,
    workspace: &crate::document_service::WorkspaceRecord,
    has_snapshot: bool,
    depth: usize,
    root_folder: Option<&str>,
    entries: Vec<serde_json::Value>,
    total_eligible: usize,
    next_offset: usize,
    limit: usize,
    max_bytes: usize,
    folder_count: usize,
    document_count: usize,
    artifact_count: usize,
    wire_count: usize,
) -> serde_json::Value {
    let folders = entries
        .iter()
        .filter(|entry| entry["type"] == "folder")
        .cloned()
        .collect::<Vec<_>>();
    let documents = entries
        .iter()
        .filter(|entry| entry["type"] == "document")
        .cloned()
        .collect::<Vec<_>>();
    let artifacts = entries
        .iter()
        .filter(|entry| entry["type"] == "artifact")
        .cloned()
        .collect::<Vec<_>>();
    let complete = next_offset >= total_eligible;
    serde_json::json!({
        "graph_id": graph.graph_id,
        "graphId": graph.graph_id,
        "title": graph.title,
        "depth": depth,
        "folderId": root_folder,
        "folders": folders,
        "documents": documents,
        "artifacts": artifacts,
        "wires": [],
        "tree": {
            "type": "page",
            "title": graph.title,
            "children": entries,
        },
        "countsScope": {
            "totals": "graph",
            "eligible": "folder-depth-and-type-filter",
        },
        "counts": {
            "documents": document_count,
            "folders": folder_count,
            "artifacts": artifact_count,
            "wires": wire_count,
            "eligible": total_eligible,
        },
        "page": {
            "limit": limit,
            "maxBytes": max_bytes,
            "cursor": next_offset.saturating_sub(
                documents.len() + folders.len() + artifacts.len()
            ).to_string(),
            "nextCursor": if complete { serde_json::Value::Null } else { serde_json::json!(next_offset.to_string()) },
            "complete": complete,
            "returned": documents.len() + folders.len() + artifacts.len(),
            "serializedBytes": 0,
        },
        "workspace": {
            "updatedAt": workspace.updated_at,
            "ydocUpdateBytes": workspace.ydoc_update_base64.len() * 3 / 4,
            "materialization": if has_snapshot { "workspace-snapshot" } else { "flat-document-fallback" },
        },
    })
}

fn snapshot_array(snapshot: Option<&serde_json::Value>, key: &str) -> Vec<serde_json::Value> {
    snapshot
        .and_then(|value| value.get(key))
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// None means a document ID; Some(empty) is an existing but empty folder.
pub(super) fn folder_document_ids(
    snapshot: Option<&serde_json::Value>,
    folder_id: &str,
) -> Option<std::collections::BTreeSet<String>> {
    let folders = snapshot_array(snapshot, "folders");
    let parents = folder_parent_map(&folders);
    if !parents.contains_key(folder_id) {
        return None;
    }
    Some(snapshot_array(snapshot, "documents").iter().filter(|document| {
        parent_descends_from(workspace_parent_id(document), folder_id, &parents)
    }).filter_map(|document| workspace_entry_id(document).map(str::to_owned)).collect())
}

fn workspace_cursor(arguments: &serde_json::Value) -> Result<usize, String> {
    let Some(value) = arguments.get("cursor") else {
        return Ok(0);
    };
    if let Some(offset) = value.as_u64() {
        return usize::try_from(offset).map_err(|_| "workspace cursor is too large".to_string());
    }
    value
        .as_str()
        .ok_or_else(|| "workspace cursor must be a non-negative integer string".to_string())?
        .parse::<usize>()
        .map_err(|_| "workspace cursor must be a non-negative integer string".to_string())
}

fn folder_parent_map(folders: &[serde_json::Value]) -> HashMap<String, Option<String>> {
    folders
        .iter()
        .filter_map(|folder| {
            workspace_entry_id(folder).map(|id| (id.to_string(), workspace_parent_id(folder)))
        })
        .collect()
}

fn visible_at_depth(
    entry: &serde_json::Value,
    root_folder: Option<&str>,
    max_depth: usize,
    folder_parents: &HashMap<String, Option<String>>,
) -> bool {
    workspace_entry_depth(entry, root_folder, folder_parents)
        .is_some_and(|depth| max_depth == 0 || depth <= max_depth)
}

fn workspace_entry_depth(
    entry: &serde_json::Value,
    root_folder: Option<&str>,
    folder_parents: &HashMap<String, Option<String>>,
) -> Option<usize> {
    let mut current = workspace_parent_id(entry);
    let mut distance = 1;
    let mut seen = HashSet::new();
    loop {
        match (root_folder, current.as_deref()) {
            (Some(root), Some(parent)) if parent == root => return Some(distance),
            (Some(_), None) => return None,
            (None, None) => return Some(distance),
            _ => {}
        }
        let Some(parent) = current else {
            return None;
        };
        if !seen.insert(parent.clone()) {
            return None;
        }
        current = folder_parents.get(&parent).cloned().flatten();
        distance += 1;
    }
}

fn descendant_counts(
    folder_id: &str,
    folders: &[serde_json::Value],
    documents: &[serde_json::Value],
    folder_parents: &HashMap<String, Option<String>>,
) -> (usize, usize) {
    let nested_folders = folders
        .iter()
        .filter(|folder| {
            workspace_entry_id(folder) != Some(folder_id)
                && parent_descends_from(workspace_parent_id(folder), folder_id, folder_parents)
        })
        .count();
    let nested_documents = documents
        .iter()
        .filter(|document| {
            parent_descends_from(workspace_parent_id(document), folder_id, folder_parents)
        })
        .count();
    (nested_folders, nested_documents)
}

fn parent_descends_from(
    mut parent: Option<String>,
    ancestor: &str,
    folder_parents: &HashMap<String, Option<String>>,
) -> bool {
    let mut seen = HashSet::new();
    while let Some(current) = parent {
        if current == ancestor {
            return true;
        }
        if !seen.insert(current.clone()) {
            return false;
        }
        parent = folder_parents.get(&current).cloned().flatten();
    }
    false
}

fn compact_workspace_entry(kind: &str, entry: &serde_json::Value) -> serde_json::Value {
    let id = workspace_entry_id(entry).unwrap_or_default();
    let title = entry
        .get("title")
        .or_else(|| entry.get("name"))
        .and_then(serde_json::Value::as_str)
        .map(|value| value.chars().take(512).collect::<String>())
        .unwrap_or_default();
    let mut compact = serde_json::json!({
        "type": kind,
        "id": id,
        "documentId": if kind == "document" { serde_json::json!(id) } else { serde_json::Value::Null },
        "title": title,
        "parentId": workspace_parent_id(entry),
        "order": entry.get("order").cloned().unwrap_or(serde_json::Value::Null),
        "updatedAt": entry.get("updatedAt").or_else(|| entry.get("updated_at")).cloned().unwrap_or(serde_json::Value::Null),
        "blockCount": entry.get("blockCount").or_else(|| entry.get("block_count")).cloned().unwrap_or(serde_json::Value::Null),
        "rdfTripleCount": entry.get("rdfTripleCount").cloned().unwrap_or(serde_json::Value::Null),
        // Chronicle (documents only; folders/artifacts carry nulls).
        "createdAt": entry.get("createdAt").or_else(|| entry.get("created_at")).cloned().unwrap_or(serde_json::Value::Null),
        "revision": entry.get("revision").cloned().unwrap_or(serde_json::Value::Null),
        "updatedBy": entry.get("updatedBy").cloned().unwrap_or(serde_json::Value::Null),
    });
    if kind == "document" {
        // Forward authority-enriched metadata; projection does not infer ownership.
        for field in ["readOnly", "derived", "readOnlyOwner"] {
            compact[field] = entry.get(field).cloned().unwrap_or(serde_json::Value::Null);
        }
    }
    compact
}

fn workspace_entry_id(entry: &serde_json::Value) -> Option<&str> {
    entry
        .get("id")
        .or_else(|| entry.get("documentId"))
        .or_else(|| entry.get("document_id"))
        .or_else(|| entry.get("folderId"))
        .or_else(|| entry.get("artifactId"))
        .and_then(serde_json::Value::as_str)
}

fn workspace_parent_id(entry: &serde_json::Value) -> Option<String> {
    entry
        .get("parentId")
        .or_else(|| entry.get("parent_id"))
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn workspace_entry_sort_key(entry: &serde_json::Value) -> (u8, String, String, String) {
    (
        match entry["type"].as_str() {
            Some("folder") => 0,
            Some("document") => 1,
            _ => 2,
        },
        entry["parentId"].as_str().unwrap_or_default().to_string(),
        entry["title"].as_str().unwrap_or_default().to_lowercase(),
        entry["id"].as_str().unwrap_or_default().to_string(),
    )
}

fn serialized_bytes(value: &serde_json::Value) -> Result<usize, String> {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len())
        .map_err(|error| format!("serialize bounded workspace response: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_scope_is_recursive_and_cycle_safe() {
        let snapshot = serde_json::json!({
            "folders": [{"id":"root"}, {"id":"child","parentId":"root"},
                {"id":"empty"}, {"id":"x","parentId":"y"}, {"id":"y","parentId":"x"}],
            "documents": [{"id":"direct","parentId":"root"},
                {"id":"nested","parentId":"child"}, {"id":"outside"},
                {"id":"cyclic","parentId":"x"}]
        });
        assert_eq!(folder_document_ids(Some(&snapshot), "root").unwrap(),
            ["direct".to_string(), "nested".to_string()].into_iter().collect());
        assert!(folder_document_ids(Some(&snapshot), "empty").unwrap().is_empty());
        assert!(folder_document_ids(Some(&snapshot), "missing").is_none());
        assert!(folder_document_ids(None, "root").is_none());
    }

    #[test]
    fn folders_precede_documents_across_page_boundaries() {
        let folders = vec![serde_json::json!({"id":"f","title":"Z folder"})];
        let documents = vec![serde_json::json!({"id":"d","title":"A document"})];
        let first = project(&folders, &documents, &[], serde_json::json!({"limit":1}));
        assert_eq!(first["folders"][0]["id"], "f");
        assert_eq!(first["page"]["nextCursor"], "1");
        let next = project(&folders, &documents, &[], serde_json::json!({"limit":1,"cursor":"1"}));
        assert_eq!(next["documents"][0]["id"], "d");
        assert_eq!(next["page"]["complete"], true);
        assert_eq!(first["countsScope"]["totals"], "graph");
        assert!(first["counts"].as_object().unwrap().values().all(|v| v.is_number()));
    }

    fn project(
        folders: &[serde_json::Value],
        documents: &[serde_json::Value],
        artifacts: &[serde_json::Value],
        arguments: serde_json::Value,
    ) -> serde_json::Value {
        let graph: GraphRecord = serde_json::from_value(serde_json::json!({
            "graphId": "test", "title": "Test", "origin": "local",
            "providerId": "local", "localPath": "", "createdAt": "",
            "updatedAt": "", "capabilities": []
        }))
        .unwrap();
        let workspace = crate::document_service::WorkspaceRecord {
            graph_id: "test".into(),
            ydoc_update_base64: String::new(),
            ydoc_state_path: String::new(),
            snapshot: Some(serde_json::json!({})),
            snapshot_path: String::new(),
            updated_at: String::new(),
        };
        project_workspace_page(
            &graph, &workspace, folders, documents, artifacts, 0, &arguments,
        )
        .unwrap()
    }

    fn deep_folders() -> Vec<serde_json::Value> {
        (0..8)
            .map(|index| {
                serde_json::json!({
                    "id": format!("f{index}"),
                    "parentId": if index == 0 { None } else { Some(format!("f{}", index - 1)) },
                })
            })
            .collect()
    }

    #[test]
    fn zero_depth_traverses_beyond_five_and_positive_depth_keeps_cap() {
        let folders = deep_folders();
        let documents = vec![serde_json::json!({"id": "deep-doc", "parentId": "f7"})];
        let artifacts = vec![serde_json::json!({"id": "deep-artifact", "parentId": "f7"})];
        let full = project(
            &folders,
            &documents,
            &artifacts,
            serde_json::json!({"depth": 0}),
        );
        assert_eq!(full["depth"], 0);
        assert_eq!(full["counts"]["eligible"], 10);
        assert_eq!(full["documents"][0]["id"], "deep-doc");
        assert_eq!(full["artifacts"][0]["id"], "deep-artifact");
        assert!(full["folders"]
            .as_array()
            .unwrap()
            .iter()
            .all(|f| f.get("collapsed").is_none()));

        let default = project(&folders, &documents, &artifacts, serde_json::json!({}));
        assert_eq!(default["depth"], 1);
        assert_eq!(default["counts"]["eligible"], 1);
        assert_eq!(default["folders"][0]["collapsed"]["documents"], 1);
        let capped = project(
            &folders,
            &documents,
            &artifacts,
            serde_json::json!({"depth": 99}),
        );
        assert_eq!(capped["depth"], 5);
        assert_eq!(capped["counts"]["eligible"], 5);
    }

    #[test]
    fn zero_depth_stays_in_scoped_subtree_and_respects_folders_only() {
        let mut folders = deep_folders();
        folders.push(serde_json::json!({"id": "outside"}));
        let documents = vec![
            serde_json::json!({"id": "inside", "parentId": "f7"}),
            serde_json::json!({"id": "outside-doc", "parentId": "outside"}),
            serde_json::json!({"id": "unfiled"}),
        ];
        let page = project(
            &folders,
            &documents,
            &[],
            serde_json::json!({"depth": 0, "folder_id": "f0"}),
        );
        assert_eq!(page["counts"]["eligible"], 8);
        assert_eq!(page["documents"].as_array().unwrap().len(), 1);
        assert_eq!(page["documents"][0]["id"], "inside");
        assert!(page["folders"]
            .as_array()
            .unwrap()
            .iter()
            .all(|f| f["id"] != "f0" && f["id"] != "outside"));
        let page = project(
            &folders,
            &documents,
            &[],
            serde_json::json!({"depth": 0, "folderId": "f0", "foldersOnly": true}),
        );
        assert_eq!(page["counts"]["eligible"], 7);
        assert!(page["documents"].as_array().unwrap().is_empty());
    }

    #[test]
    fn zero_depth_cycles_terminate_without_leaking_into_other_scopes() {
        let folders = vec![
            serde_json::json!({"id": "a", "parentId": "b"}),
            serde_json::json!({"id": "b", "parentId": "a"}),
            serde_json::json!({"id": "self", "parentId": "self"}),
            serde_json::json!({"id": "root"}),
        ];
        let documents = vec![serde_json::json!({"id": "cycle-doc", "parentId": "a"})];
        let global = project(&folders, &documents, &[], serde_json::json!({"depth": 0}));
        assert_eq!(global["counts"]["eligible"], 1);
        let scoped = project(
            &folders,
            &documents,
            &[],
            serde_json::json!({"depth": 0, "folderId": "root"}),
        );
        assert_eq!(scoped["counts"]["eligible"], 0);
        // Preserve scoped traversal's existing behavior: reaching the requested
        // root succeeds even if its own ancestry is cyclic.
        let parents = folder_parent_map(&folders);
        assert!(visible_at_depth(&documents[0], Some("a"), 0, &parents));
    }

    #[test]
    fn unlimited_depth_pages_obey_item_and_byte_limits_without_duplicates() {
        let folders = deep_folders();
        let documents: Vec<_> = (0..250)
            .map(|index| {
                serde_json::json!({
                    "id": format!("doc-{index:03}"), "parentId": "f7", "title": "x".repeat(512),
                })
            })
            .collect();
        for (limit, max_bytes) in [(3, MAX_WORKSPACE_BYTES), (999, MIN_WORKSPACE_BYTES)] {
            let mut cursor = serde_json::json!("0");
            let mut seen = HashSet::new();
            loop {
                let page = project(
                    &folders,
                    &documents,
                    &[],
                    serde_json::json!({
                        "depth": 0, "limit": limit, "maxBytes": max_bytes, "cursor": cursor,
                    }),
                );
                let entries = page["tree"]["children"].as_array().unwrap();
                assert!(!entries.is_empty());
                assert!(entries.len() <= limit.min(MAX_WORKSPACE_LIMIT));
                assert_eq!(page["page"]["limit"], limit.min(MAX_WORKSPACE_LIMIT));
                let bytes = serialized_bytes(&page).unwrap();
                assert!(bytes <= max_bytes);
                assert_eq!(page["page"]["serializedBytes"], bytes);
                for entry in entries {
                    assert!(seen.insert(entry["id"].as_str().unwrap().to_string()));
                }
                if page["page"]["complete"] == true {
                    assert!(page["page"]["nextCursor"].is_null());
                    break;
                }
                cursor = page["page"]["nextCursor"].clone();
            }
            assert_eq!(seen.len(), folders.len() + documents.len());
        }
    }

    #[test]
    fn compact_document_entries_forward_access_metadata_without_inference() {
        let entry = serde_json::json!({
            "id": "doc", "readOnly": false, "derived": true, "readOnlyOwner": "authority",
        });
        let compact = compact_workspace_entry("document", &entry);
        for field in ["readOnly", "derived", "readOnlyOwner"] {
            assert_eq!(compact[field], entry[field]);
            for kind in ["folder", "artifact"] {
                assert!(compact_workspace_entry(kind, &entry).get(field).is_none());
            }
        }
        let missing = compact_workspace_entry("document", &serde_json::json!({"id": "ordinary"}));
        assert!(missing["readOnly"].is_null());
        assert!(missing["derived"].is_null());
        assert!(missing["readOnlyOwner"].is_null());
    }

    #[test]
    fn depth_filter_is_relative_to_the_requested_folder() {
        let folders = vec![
            serde_json::json!({"id": "a", "title": "A"}),
            serde_json::json!({"id": "b", "parentId": "a", "title": "B"}),
            serde_json::json!({"id": "c", "parentId": "b", "title": "C"}),
        ];
        let parents = folder_parent_map(&folders);
        assert!(visible_at_depth(&folders[1], Some("a"), 1, &parents));
        assert!(!visible_at_depth(&folders[2], Some("a"), 1, &parents));
        assert!(visible_at_depth(&folders[2], Some("a"), 2, &parents));
    }

    #[test]
    fn compact_entries_drop_unbounded_document_content() {
        let entry = serde_json::json!({
            "documentId": "doc-a",
            "title": "A",
            "body": "x".repeat(900_000),
            "ydocUpdateBase64": "y".repeat(900_000),
            "blockCount": 7,
        });
        let compact = compact_workspace_entry("document", &entry);
        let bytes = serde_json::to_vec(&compact).unwrap().len();
        assert!(bytes < 1_024);
        assert_eq!(compact["blockCount"], 7);
        assert!(compact.get("body").is_none());
    }

    #[test]
    fn depth_boundary_reports_collapsed_descendant_counts() {
        let folders = vec![
            serde_json::json!({"id": "a"}),
            serde_json::json!({"id": "b", "parentId": "a"}),
            serde_json::json!({"id": "c", "parentId": "b"}),
        ];
        let documents = vec![
            serde_json::json!({"documentId": "one", "parentId": "a"}),
            serde_json::json!({"documentId": "two", "parentId": "c"}),
        ];
        let parents = folder_parent_map(&folders);
        assert_eq!(
            descendant_counts("a", &folders, &documents, &parents),
            (2, 2)
        );
    }

    #[test]
    fn cursor_accepts_opaque_decimal_strings_and_rejects_garbage() {
        assert_eq!(
            workspace_cursor(&serde_json::json!({"cursor": "42"})).unwrap(),
            42
        );
        assert!(workspace_cursor(&serde_json::json!({"cursor": "later"})).is_err());
    }

    #[test]
    fn serialized_byte_metadata_is_exact() {
        let value = serde_json::json!({
            "tree": { "children": ["x".repeat(10_000)] },
            "page": { "serializedBytes": 0 }
        });
        let (value, bytes) = finalize_serialized_bytes(value).unwrap();
        assert_eq!(value["page"]["serializedBytes"], bytes);
        assert_eq!(serde_json::to_vec(&value).unwrap().len(), bytes);
    }
}
