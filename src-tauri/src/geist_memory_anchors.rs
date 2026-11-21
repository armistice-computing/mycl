//! Deliberate, memory-owned record links. These are not workspace wires and
//! must not feed judgment topology or refresh attention on read.
use crate::{
    app_runtime::AppHandle,
    geist_memory_store::{memory_sort_key, read_memory_store, LocalMemoryRecord, LocalMemoryStore},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

pub(crate) const MAX_ANCHORS: usize = 16;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MemoryAnchor {
    pub(crate) document_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) block_id: Option<String>,
    pub(crate) title_at_creation: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AnchorInput {
    document_id: String,
    #[serde(default)]
    block_id: Option<String>,
}

fn contains_block(node: &Value, id: &str) -> bool {
    let attrs = &node["attrs"];
    let identity = match attrs.get("data-block-id") {
        None | Some(Value::Null) => attrs.get("blockId"),
        value => value,
    };
    if identity.and_then(Value::as_str) == Some(id) {
        return true;
    }
    node["content"]
        .as_array()
        .is_some_and(|children| children.iter().any(|child| contains_block(child, id)))
}

pub(crate) fn validate_anchors(
    app: &AppHandle,
    graph_id: &str,
    value: Option<&Value>,
) -> Result<Vec<MemoryAnchor>, String> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let list = value.as_array().ok_or("anchors must be an array")?;
    if list.len() > MAX_ANCHORS {
        return Err(format!("anchors accepts at most {MAX_ANCHORS} references"));
    }
    let graph_dir = crate::paths::existing_graph_dir(app, graph_id)?;
    let mut result = Vec::new();
    let mut seen = BTreeSet::new();
    for (index, raw) in list.iter().enumerate() {
        let input: AnchorInput = serde_json::from_value(raw.clone())
            .map_err(|error| format!("anchors[{index}]: {error}"))?;
        if input.document_id.trim().is_empty()
            || input
                .block_id
                .as_ref()
                .is_some_and(|id| id.trim().is_empty())
        {
            return Err(format!("anchors[{index}]: target IDs must not be empty"));
        }
        if !seen.insert((input.document_id.clone(), input.block_id.clone())) {
            continue;
        }
        // Never invoke missing-document self-heal while validating a reference.
        let path = crate::document_paths::document_dir(&graph_dir, &input.document_id)?
            .join("document.json");
        if !path.is_file() {
            return Err(format!(
                "anchors[{index}]: document not found: {}",
                input.document_id
            ));
        }
        crate::document_tombstone_store::require_document_not_tombstoned(
            &graph_dir,
            &input.document_id,
        )?;
        let document = crate::document_record_store::read_document_record_cold(&graph_dir, &path)?;
        if let Some(id) = &input.block_id {
            let exists = document
                .tiptap_json
                .as_ref()
                .map(|tree| contains_block(tree, id))
                .unwrap_or_else(|| document.blocks.iter().any(|block| block.id == *id));
            if !exists {
                return Err(format!(
                    "anchors[{index}]: block {id} not found in committed document {}",
                    input.document_id
                ));
            }
        }
        result.push(MemoryAnchor {
            document_id: input.document_id,
            block_id: input.block_id,
            title_at_creation: document.title.chars().take(512).collect(),
        });
    }
    Ok(result)
}

pub(crate) fn add_reverse_context(
    app: &AppHandle,
    arguments: &Value,
    result: &mut Value,
) -> Result<(), String> {
    if arguments.get("includeMemories").and_then(Value::as_bool) != Some(true) {
        return Ok(());
    }
    let graph_id = crate::mcp_utils::mcp_required_graph_id(arguments)?;
    let document_id = crate::mcp_utils::mcp_required_document_id(arguments)?;
    let dir = crate::paths::existing_graph_dir(app, &graph_id)?;
    result["recallingMemories"] = reverse_context(&dir, &graph_id, &document_id);
    Ok(())
}

/// Active records win over historical duplicates, just as numbered recall does.
pub(crate) fn memories_for_document<'a>(
    store: &'a LocalMemoryStore,
    document_id: &str,
) -> Vec<&'a LocalMemoryRecord> {
    let mut records = BTreeMap::new();
    let mut archives = store.archives.iter().collect::<Vec<_>>();
    archives.sort_by(|a, b| a.archive_doc_id.cmp(&b.archive_doc_id));
    for archive in archives {
        for memory in &archive.memories {
            records.entry(memory.number).or_insert(memory);
        }
    }
    for memory in store.memories.values() {
        records.insert(memory.number, memory);
    }
    let mut result = records
        .into_values()
        .filter(|memory| {
            memory
                .anchors
                .iter()
                .any(|anchor| anchor.document_id == document_id)
        })
        .collect::<Vec<_>>();
    result.sort_by(|a, b| {
        memory_sort_key(b)
            .cmp(&memory_sort_key(a))
            .then_with(|| b.number.cmp(&a.number))
    });
    result
}

/// Reverse context is opt-in so UI metadata reads do not scan the memory store.
/// Failure to read derived context must not turn a readable document into an error.
pub(crate) fn reverse_context(graph_dir: &Path, graph_id: &str, document_id: &str) -> Value {
    match read_memory_store(graph_dir, graph_id) {
        Err(error) => json!({"available":false,"warning":error}),
        Ok(store) => {
            let memories = memories_for_document(&store, document_id);
            let total = memories.len();
            let rows = memories.into_iter().take(5).map(|memory| json!({
                "number":memory.number, "preview":memory.content.chars().take(240).collect::<String>(),
                "createdAt":memory.created_at, "rememberer":memory.rememberer,
                "anchors":memory.anchors.iter().filter(|a| a.document_id == document_id).collect::<Vec<_>>()
            })).collect::<Vec<_>>();
            json!({"available":true,"memories":rows,"total":total,"truncated":total>5,"continuation":{"tool":"recall","graphId":graph_id,"documentId":document_id}})
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nested_blocks_are_addressable_but_text_is_not_an_id() {
        let tree = json!({"content":[{"attrs":{"data-block-id":"outer"},"content":[{"attrs":{"blockId":"inner"}}]}]});
        assert!(contains_block(&tree, "inner"));
        assert!(!contains_block(&tree, "missing"));
        assert!(!contains_block(
            &json!({"attrs":{"data-block-id":"real","blockId":"shadow"}}),
            "shadow"
        ));
        assert!(
            serde_json::from_value::<AnchorInput>(json!({"documentId":"d","graphId":"other"}))
                .is_err()
        );
    }

    #[test]
    fn old_memories_need_no_migration_and_active_records_win() {
        let old = json!({"number":1,"blockId":"memory-1","content":"old","createdAt":"1","lastActive":"1"});
        let record: LocalMemoryRecord = serde_json::from_value(old.clone()).unwrap();
        assert!(record.anchors.is_empty());
        assert!(serde_json::to_value(record)
            .unwrap()
            .get("anchors")
            .is_none());
        let mut linked = old.clone();
        linked["anchors"] = json!([{"documentId":"d","titleAtCreation":"D"}]);
        let store: LocalMemoryStore = serde_json::from_value(json!({"schemaVersion":1,"graphId":"g","memories":{"1":old},"archives":[{"archiveDocId":"a","archivedAt":"2","kept":0,"archived":1,"memories":[linked]}]})).unwrap();
        assert!(
            memories_for_document(&store, "d").is_empty(),
            "active copy overrides archived duplicate even when only archived copy linked"
        );
        let mut archived_only = store.clone();
        archived_only.memories.clear();
        assert_eq!(memories_for_document(&archived_only, "d").len(), 1);
    }
}
