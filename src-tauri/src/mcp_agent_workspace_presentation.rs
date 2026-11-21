//! Agent-view shaping for `get_workspace` and `write_document`.
//!
//! Presentation only, in the sense `mcp_agent_presentation` defines: pure
//! per-tool transformations of a handler's finished result. Nothing here reads
//! storage, reruns a handler, or changes an outcome. Every removal is of a
//! value the same response already carries somewhere else, and only when the
//! two copies agree — divergence is evidence and is left in place.
//!
//! Why these two tools: `get_workspace` is read at every attunement by every
//! agent and was ~38 KB at depth 1 on the default graph (2026-09-18), half of
//! it a `tree` that repeats `folders` + `documents` + `artifacts` verbatim and
//! much of the rest null placeholders on folder rows. `write_document` is the
//! most frequent mutation acknowledgment. Owner of this file: Beta. Wiring
//! into `compact_result` is Epsilon's.
use serde_json::{Map, Value};

/// Remove `redundant` only when it equals `canonical`. Same rule as the
/// generic presentation module; duplicated here so this file has no
/// dependency on that module's private items.
fn drop_agreeing_alias(object: &mut Map<String, Value>, redundant: &str, canonical: &str) {
    if object.get(canonical).is_some() && object.get(redundant) == object.get(canonical) {
        object.remove(redundant);
    }
}

/// Access semantics travel even when null: `readOnlyOwner: null` means "not
/// owned by a server view", which is not the same as unknown.
const KEEP_WHEN_NULL: [&str; 3] = ["readOnly", "derived", "readOnlyOwner"];

/// Fields the listing does not need per entry. They stay on the machine view
/// and on `read_document`, which is where an agent goes when it wants them.
const LISTING_NOISE: [&str; 1] = ["rdfTripleCount"];

fn compact_workspace_entry(entry: &Value) -> Value {
    let Some(object) = entry.as_object() else {
        return entry.clone();
    };
    let mut out = object.clone();
    // `documentId` on a document row repeats `id`; on folders/artifacts it is
    // the null placeholder and goes with the other nulls.
    drop_agreeing_alias(&mut out, "documentId", "id");
    for key in LISTING_NOISE {
        out.remove(key);
    }
    out.retain(|key, value| !value.is_null() || KEEP_WHEN_NULL.contains(&key.as_str()));
    Value::Object(out)
}

fn entry_key(entry: &Value) -> Option<(String, String)> {
    let kind = entry.get("type")?.as_str()?;
    let id = entry.get("id")?.as_str()?;
    Some((kind.to_string(), id.to_string()))
}

/// True when `tree.children` is exactly the union of the three typed lists,
/// same rows field for field, not merely the same ids, so dropping the tree
/// loses nothing an agent could not rebuild from them. A tree row that
/// carries a field the list row lacks, or disagrees on one, keeps the tree.
fn tree_repeats_lists(result: &Map<String, Value>) -> bool {
    let Some(children) = result
        .get("tree")
        .and_then(|tree| tree.get("children"))
        .and_then(Value::as_array)
    else {
        return false;
    };
    let lists = ["folders", "documents", "artifacts"]
        .iter()
        .filter_map(|key| result.get(*key).and_then(Value::as_array))
        .flatten()
        .collect::<Vec<_>>();
    if lists.len() != children.len() {
        return false;
    }
    let mut rows: std::collections::BTreeMap<(String, String), &Value> =
        std::collections::BTreeMap::new();
    for entry in &lists {
        let Some(key) = entry_key(entry) else {
            return false;
        };
        rows.insert(key, entry);
    }
    if rows.len() != children.len() {
        return false;
    }
    children.iter().all(|child| {
        entry_key(child)
            .and_then(|key| rows.get(&key))
            .is_some_and(|row| *row == child)
    })
}

/// `get_workspace` for an agent: the three typed lists, compacted per row; the
/// duplicate `tree` dropped when it is a duplicate; `page`, `counts`, and
/// `workspace` untouched (pagination and materialization are the ledger).
pub(super) fn compact_workspace(value: &Value) -> Value {
    let Some(object) = value.as_object() else {
        return value.clone();
    };
    let mut out = object.clone();
    drop_agreeing_alias(&mut out, "graph_id", "graphId");
    if tree_repeats_lists(&out) {
        out.remove("tree");
    }
    for key in ["folders", "documents", "artifacts"] {
        if let Some(Value::Array(entries)) = out.get(key) {
            let compacted = entries.iter().map(compact_workspace_entry).collect::<Vec<_>>();
            out.insert(key.to_string(), Value::Array(compacted));
        }
    }
    // A tree that survived (it disagreed with the lists) gets the same
    // per-row compaction so its rows are not the one fat copy.
    if let Some(children) = out
        .get_mut("tree")
        .and_then(|tree| tree.get_mut("children"))
        .and_then(Value::as_array_mut)
    {
        for child in children.iter_mut() {
            *child = compact_workspace_entry(child);
        }
    }
    Value::Object(out)
}

/// `write_document` for an agent: one spelling per field. Everything that is
/// a verdict stays: `success`, `revision`, `durability`, `warnings` (even when
/// empty — an empty list is the statement that nothing went wrong on the
/// derived side), `blockIds` in write order. No text preview is synthesized:
/// the handler result carries none, and a preview invented from an id would
/// be a claim about content this function never read.
pub(super) fn compact_write_document(value: &Value) -> Value {
    let Some(object) = value.as_object() else {
        return value.clone();
    };
    let mut out = object.clone();
    for (redundant, canonical) in [
        ("graph_id", "graphId"),
        ("document_id", "documentId"),
        ("block_ids", "blockIds"),
        ("durability_checked", "durabilityChecked"),
        ("source_format", "sourceFormat"),
        ("workspace_entry_durable", "workspaceEntryDurable"),
    ] {
        drop_agreeing_alias(&mut out, redundant, canonical);
    }
    drop_ids_named_by_previews(&mut out, &["blockIds", "block_ids"]);
    Value::Object(out)
}

/// Mutation acks carry `blocks`: one "id + first characters of the stored text"
/// line per touched block (e6d1324). The bare id list beside it names the same
/// blocks with less information, so an agent sees each id twice. Drop it when
/// the previews cover exactly those ids, in order. When the previews were
/// omitted for size, the ids stay: they are then the only way to learn which
/// blocks were made, and the omission note says to use them. Anything that
/// doesn't line up also stays, since a mismatch is evidence.
pub(super) fn drop_ids_named_by_previews(object: &mut Map<String, Value>, keys: &[&str]) {
    for key in keys {
        let (Some(ids), Some(previews)) = (
            object.get(*key).and_then(Value::as_array),
            object.get("blocks").and_then(Value::as_array),
        ) else {
            continue;
        };
        if previews.len() == ids.len()
            && previews.iter().zip(ids).all(|(preview, id)| preview_names(preview, id))
        {
            object.remove(*key);
        }
    }
}

fn preview_names(preview: &Value, id: &Value) -> bool {
    let (Some(preview), Some(id)) = (preview.as_str(), id.as_str()) else {
        return false;
    };
    let bare = |value: &str| value.strip_prefix("block-").unwrap_or(value).to_string();
    preview
        .split(' ')
        .next()
        .is_some_and(|head| bare(head) == bare(id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn doc(id: &str) -> Value {
        json!({
            "type": "document", "id": id, "documentId": id, "title": format!("Doc {id}"),
            "parentId": null, "order": 1.0, "updatedAt": 2.0, "blockCount": 3,
            "rdfTripleCount": 40, "createdAt": 1.0, "revision": 1, "updatedBy": null,
            "readOnly": false, "derived": false, "readOnlyOwner": null
        })
    }

    fn folder(id: &str) -> Value {
        json!({
            "type": "folder", "id": id, "documentId": null, "title": format!("Folder {id}"),
            "parentId": null, "order": 5, "updatedAt": null, "blockCount": null,
            "rdfTripleCount": null, "createdAt": null, "revision": null, "updatedBy": null,
            "collapsed": {"folders": 1, "documents": 4}
        })
    }

    fn workspace(children_match: bool) -> Value {
        let folders = vec![folder("f1")];
        let documents = vec![doc("d1"), doc("d2")];
        let artifacts: Vec<Value> = vec![];
        let mut children = folders.clone();
        children.extend(documents.clone());
        if !children_match {
            children.pop();
        }
        json!({
            "graph_id": "g", "graphId": "g", "title": "Default", "depth": 1, "folderId": null,
            "folders": folders, "documents": documents, "artifacts": artifacts, "wires": [],
            "tree": {"type": "page", "title": "Default", "children": children},
            "counts": {"documents": 2, "folders": 1, "artifacts": 0, "wires": 9, "eligible": 3},
            "page": {"limit": 50, "maxBytes": 65536, "cursor": "0", "nextCursor": null,
                     "complete": true, "returned": 3, "serializedBytes": 1234},
            "workspace": {"updatedAt": "1", "ydocUpdateBytes": 10, "materialization": "workspace-snapshot"}
        })
    }

    #[test]
    fn workspace_drops_the_duplicate_tree_and_folder_null_placeholders() {
        let full = workspace(true);
        let compact = compact_workspace(&full);
        assert!(compact.get("tree").is_none(), "tree repeated the lists");
        assert!(compact.get("graph_id").is_none());
        assert_eq!(compact["graphId"], "g");
        let f = &compact["folders"][0];
        assert_eq!(f["id"], "f1");
        assert_eq!(f["collapsed"], json!({"folders": 1, "documents": 4}));
        for gone in ["documentId", "updatedAt", "blockCount", "rdfTripleCount", "createdAt", "revision", "updatedBy"] {
            assert!(f.get(gone).is_none(), "{gone} was a null placeholder");
        }
        let d = &compact["documents"][0];
        assert!(d.get("documentId").is_none(), "repeats id");
        assert!(d.get("rdfTripleCount").is_none(), "listing noise");
        assert_eq!(d["blockCount"], 3);
        assert_eq!(d["readOnly"], false);
        assert!(d.get("readOnlyOwner").is_some(), "access semantics stay even when null");
        assert_eq!(d["readOnlyOwner"], Value::Null);
        // The ledger is untouched.
        assert_eq!(compact["page"], full["page"]);
        assert_eq!(compact["counts"], full["counts"]);
        assert_eq!(compact["workspace"], full["workspace"]);
        assert_eq!(compact["wires"], json!([]));
        // Real size win, and idempotent.
        assert!(compact.to_string().len() * 100 < full.to_string().len() * 55);
        assert_eq!(compact_workspace(&compact), compact);
    }

    #[test]
    fn workspace_keeps_a_tree_that_disagrees_with_the_lists() {
        let full = workspace(false);
        let compact = compact_workspace(&full);
        assert!(compact.get("tree").is_some(), "a missing child is divergence");
        assert_eq!(compact["tree"]["children"].as_array().unwrap().len(), 2);
        assert!(compact["tree"]["children"][0].get("rdfTripleCount").is_none());

        // Same ids, different rows: the tree says readOnly and a newer title
        // than the list does. That is evidence, so the tree stays.
        let mut full = workspace(true);
        full["tree"]["children"][1]["readOnly"] = json!(true);
        full["tree"]["children"][1]["title"] = json!("Doc d1 (renamed)");
        full["tree"]["children"][1]["warning"] = json!("tree-only");
        let compact = compact_workspace(&full);
        assert!(compact.get("tree").is_some(), "same ids, differing fields");
        assert_eq!(compact["tree"]["children"][1]["warning"], "tree-only");
        assert_eq!(compact["tree"]["children"][1]["readOnly"], true);
    }

    #[test]
    fn workspace_passes_non_objects_and_unknown_fields_through() {
        assert_eq!(compact_workspace(&json!("nope")), json!("nope"));
        let mut full = workspace(true);
        full["experimental"] = json!({"keep": true});
        full["documents"][0]["newField"] = json!("keep me");
        let compact = compact_workspace(&full);
        assert_eq!(compact["experimental"], json!({"keep": true}));
        assert_eq!(compact["documents"][0]["newField"], "keep me");
    }

    #[test]
    fn write_document_keeps_one_spelling_and_every_verdict() {
        let full = json!({
            "success": true, "graph_id": "g", "graphId": "g",
            "document_id": "d", "documentId": "d", "title": "T", "revision": 4,
            "block_ids": ["b1", "b2"], "blockIds": ["b1", "b2"],
            "durability_checked": true, "durabilityChecked": true, "durability": "local",
            "source_format": "markdown", "sourceFormat": "markdown", "warnings": []
        });
        let compact = compact_write_document(&full);
        assert_eq!(
            compact,
            json!({
                "success": true, "graphId": "g", "documentId": "d", "title": "T", "revision": 4,
                "blockIds": ["b1", "b2"], "durabilityChecked": true, "durability": "local",
                "sourceFormat": "markdown", "warnings": []
            })
        );
        assert_eq!(compact_write_document(&compact), compact);
    }

    #[test]
    fn write_document_drops_the_bare_id_list_when_previews_name_it() {
        let full = json!({
            "success": true, "documentId": "d",
            "blockIds": ["block-a1", "block-b2"],
            "blocks": ["block-a1 First line", "block-b2 Second lin"]
        });
        let compact = compact_write_document(&full);
        assert!(compact.get("blockIds").is_none());
        assert_eq!(compact["blocks"], full["blocks"]);
        // Previews written without the block- prefix still name the same ids.
        let bare = json!({"blockIds": ["block-a1"], "blocks": ["a1 First line"]});
        assert!(compact_write_document(&bare).get("blockIds").is_none());
        assert_eq!(compact_write_document(&compact), compact);
    }

    #[test]
    fn write_document_keeps_ids_the_previews_do_not_match() {
        let reordered = json!({"blockIds": ["block-a1", "block-b2"], "blocks": ["block-b2 x", "block-a1 y"]});
        assert_eq!(compact_write_document(&reordered)["blockIds"], reordered["blockIds"]);
        let short = json!({"blockIds": ["block-a1", "block-b2"], "blocks": ["block-a1 x"]});
        assert_eq!(compact_write_document(&short)["blockIds"], short["blockIds"]);
        let none = json!({"blockIds": ["block-a1"]});
        assert_eq!(compact_write_document(&none)["blockIds"], none["blockIds"]);
    }

    #[test]
    fn write_document_keeps_the_ids_when_previews_were_omitted_for_size() {
        // The real omission note, which tells the agent to use the ids.
        let full = json!({
            "blockIds": ["block-a1", "block-b2", "block-c3"],
            "blocks": {"omitted": 401, "reason": "more than 400 blocks; use blockIds"}
        });
        assert_eq!(compact_write_document(&full), full);
    }

    #[test]
    fn write_document_keeps_divergent_aliases_and_warnings() {
        let full = json!({
            "success": true, "document_id": "d", "documentId": "d-other",
            "blockIds": ["b1"], "warnings": ["projection failed"], "warning": "catalog entry absent",
            "workspace_entry_durable": false
        });
        let compact = compact_write_document(&full);
        assert_eq!(compact["document_id"], "d");
        assert_eq!(compact["documentId"], "d-other");
        assert_eq!(compact["warnings"], json!(["projection failed"]));
        assert_eq!(compact["warning"], "catalog entry absent");
        assert_eq!(compact["workspace_entry_durable"], false, "no camel twin, so it stays");
        assert_eq!(compact_write_document(&json!(null)), json!(null));
    }
}
