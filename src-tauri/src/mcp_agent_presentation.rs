//! Presentation only: never authorizes, rereads state, or changes a tool outcome.
use serde_json::{json, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ResponseView {
    Machine,
    AgentV1,
}

impl ResponseView {
    pub(super) fn from_params(params: &Value) -> Result<Self, String> {
        let Some(meta) = params.get("_meta") else {
            return Ok(Self::Machine);
        };
        let meta = meta
            .as_object()
            .ok_or("tools/call _meta must be an object")?;
        match meta.get("garden/responseView") {
            None => Ok(Self::Machine),
            Some(Value::String(view)) if view == "agent-v1" => Ok(Self::AgentV1),
            Some(Value::String(view)) if view == "machine" => Ok(Self::Machine),
            _ => Err("unsupported garden/responseView; expected agent-v1 or machine".into()),
        }
    }

    pub(super) fn envelope(self, name: &str, value: Value) -> Value {
        let value = match self {
            Self::Machine => value,
            Self::AgentV1 => {
                let mut result = compact_result(name, &value);
                crate::mcp_agent_timestamps::format_timestamps(name, &mut result);
                result
            }
        };
        // Keep both MCP representations compatible; do not assume which one a
        // harness consumes. Both contain the SAME compact view for agents.
        let text = match self {
            Self::Machine => serde_json::to_string_pretty(&value),
            Self::AgentV1 => serde_json::to_string(&value),
        }
        .expect("JSON Value serializes");
        json!({"content":[{"type":"text","text":text}], "structuredContent":value})
    }
}

// Remove a redundant alias only when its value agrees. Divergence is evidence,
// not something presentation should conceal. Never recurse into user content.
fn aliases(value: &mut Value, pairs: &[(&str, &str)]) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    for (redundant, canonical) in pairs {
        if object.get(*canonical).is_some() && object.get(*redundant) == object.get(*canonical) {
            object.remove(*redundant);
        }
    }
}

fn memory(value: &mut Value) {
    aliases(
        value,
        &[
            ("block_id", "blockId"),
            ("created_at", "createdAt"),
            ("last_active", "lastActive"),
        ],
    );
    if let (Some(content), Some(text), Some(number)) = (
        value["content"].as_str(),
        value["text"].as_str(),
        value["number"].as_u64(),
    ) {
        if text == content || text == format!("{number}. {content}") {
            value.as_object_mut().unwrap().remove("text");
        }
    }
}

// These audited envelope fields are currently emitted without camelCase twins.
// An existing canonical value (including null) is never overwritten.
fn envelope_aliases(value: &mut Value, pairs: &[(&str, &str)]) {
    if let Some(object) = value.as_object_mut() {
        for (alias, canonical) in pairs {
            if !object.contains_key(*canonical) {
                if let Some(value) = object.remove(*alias) {
                    object.insert((*canonical).to_string(), value);
                }
            }
        }
    }
    aliases(value, pairs);
}

fn read_aliases(value: &mut Value) {
    envelope_aliases(
        value,
        &[("graph_id", "graphId"), ("document_id", "documentId")],
    );
    aliases(
        value,
        &[
            ("updated_at", "updatedAt"),
            ("created_at", "createdAt"),
            ("updated_by", "updatedBy"),
            ("created_by", "createdBy"),
        ],
    );
}

pub(super) fn compact_result(name: &str, value: &Value) -> Value {
    let mut result = value.clone();
    match name {
        "read_document" => {
            read_aliases(&mut result);
            envelope_aliases(
                &mut result,
                &[("block_count", "blockCount"), ("block_ids", "blockIds")],
            );
        }
        "query_blocks" => {
            read_aliases(&mut result);
            aliases(&mut result, &[("total_matches", "totalMatches")]);
            if let Some(groups) = result.get_mut("results").and_then(Value::as_array_mut) {
                for group in groups {
                    aliases(group, &[("query_index", "queryIndex")]);
                    if let Some(blocks) = group.get_mut("blocks").and_then(Value::as_array_mut) {
                        for block in blocks {
                            aliases(
                                block,
                                &[
                                    ("block_id", "blockId"),
                                    ("block_type", "type"),
                                    ("blockType", "type"),
                                    ("text_content", "content"),
                                    ("textContent", "content"),
                                    ("parent_id", "parentId"),
                                    ("text_length", "textLength"),
                                ],
                            );
                        }
                    }
                }
            }
        }
        "search_blocks" | "search_documents" => {
            envelope_aliases(
                &mut result,
                &[
                    ("graph_id", "graphId"),
                    ("semantic_error", "semanticError"),
                    ("salience_error", "salienceError"),
                ],
            );
            if let Some(hits) = result.get_mut("results").and_then(Value::as_array_mut) {
                for hit in hits {
                    aliases(
                        hit,
                        &[
                            ("document_id", "documentId"),
                            ("document_title", "documentTitle"),
                            ("block_id", "blockId"),
                            ("block_type", "blockType"),
                            ("match_source", "matchSource"),
                            ("lexical_score", "lexicalScore"),
                            ("semantic_score", "semanticScore"),
                            ("composite_score", "compositeScore"),
                            ("relevance_score", "relevanceScore"),
                            ("salience_boost", "salienceBoost"),
                            ("updated_at", "updatedAt"),
                            ("block_count", "blockCount"),
                        ],
                    );
                }
            }
        }
        "get_workspace" => {
            return crate::mcp_agent_workspace_presentation::compact_workspace(value)
        }
        "write_document" => {
            return crate::mcp_agent_workspace_presentation::compact_write_document(value)
        }
        "recall" => {
            if let Some(memories) = result.get_mut("memories").and_then(Value::as_array_mut) {
                for item in memories {
                    memory(item);
                }
            }
        }
        "remember" => memory(&mut result),
        "music" | "sing" => aliases(
            &mut result,
            &[
                ("verse_count", "verseCount"),
                ("active_verse_count", "activeVerseCount"),
                ("counterpoint_parts", "counterpointParts"),
            ],
        ),
        "get_important_blocks" => {
            aliases(
                &mut result,
                &[("value_store_available", "valueStoreAvailable")],
            );
            if let Some(blocks) = result.get_mut("blocks").and_then(Value::as_array_mut) {
                for block in blocks {
                    aliases(
                        block,
                        &[
                            ("text", "content"),
                            ("block_id", "blockId"),
                            ("doc_id", "documentId"),
                            ("docId", "documentId"),
                            ("document_id", "documentId"),
                            ("block_wires", "blockWires"),
                            ("doc_wires", "docWires"),
                        ],
                    );
                }
            }
        }
        "insert_blocks" => {
            if let Some(object) = result.as_object_mut() {
                crate::mcp_agent_workspace_presentation::drop_ids_named_by_previews(
                    object,
                    &["block_ids", "blockIds"],
                );
            }
        }
        "update_blocks" => {
            // block_update names its id list "updated" (block_ops.rs).
            if let Some(object) = result.as_object_mut() {
                crate::mcp_agent_workspace_presentation::drop_ids_named_by_previews(
                    object,
                    &["updated"],
                );
            }
        }
        _ => {} // Unsupported/new shapes stay lossless until explicitly audited.
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_aliases_are_bounded_and_canonical_once() {
        let payload = json!({"block_id":"user", "blockId":"user", "text_content":"keep", "textContent":"keep"});
        let value = json!({"graph_id":"g", "document_id":"d", "total_matches":1, "totalMatches":1,
        "results":[{"query_index":0,"queryIndex":0,"blocks":[{
            "block_id":"b","blockId":"b","type":"paragraph","block_type":"paragraph","blockType":"paragraph",
            "content":"body","text_content":"body","textContent":"body",
            "parent_id":null,"parentId":null,"text_length":4,"textLength":4,
            "attrs":payload,"metadata":payload,"tiptapJson":payload,"unknown":payload
        }]}]});
        let compact = compact_result("query_blocks", &value);
        assert_eq!(
            compact,
            json!({"graphId":"g","documentId":"d","totalMatches":1,
            "results":[{"queryIndex":0,"blocks":[{"blockId":"b","type":"paragraph","content":"body",
                "parentId":null,"textLength":4,"attrs":payload,"metadata":payload,"tiptapJson":payload,"unknown":payload}]}]})
        );
        assert_eq!(compact_result("query_blocks", &compact), compact);
        let divergent = json!({"graph_id":"g","graphId":null,"document_id":"d","documentId":"other",
            "results":[{"query_index":0,"queryIndex":1,"blocks":[{"content":"body","text_content":"other","textContent":null,
                "block_id":"a","blockId":"b","type":"paragraph","block_type":"heading","blockType":"heading"}]}]});
        assert_eq!(compact_result("query_blocks", &divergent), divergent);
    }

    #[test]
    fn search_aliases_keep_scores_conflicts_and_unknown_payloads() {
        let pairs = [
            ("document_id", "documentId"),
            ("document_title", "documentTitle"),
            ("block_id", "blockId"),
            ("block_type", "blockType"),
            ("match_source", "matchSource"),
            ("lexical_score", "lexicalScore"),
            ("semantic_score", "semanticScore"),
            ("composite_score", "compositeScore"),
            ("relevance_score", "relevanceScore"),
            ("salience_boost", "salienceBoost"),
            ("updated_at", "updatedAt"),
            ("block_count", "blockCount"),
        ];
        for name in ["search_blocks", "search_documents"] {
            for (alias, canonical) in [
                ("semantic_error", "semanticError"),
                ("salience_error", "salienceError"),
            ] {
                let mut value = json!({});
                value[alias] = json!({"message":"unavailable", "unknown":true});
                let compact = compact_result(name, &value);
                assert_eq!(compact[canonical], value[alias]);
                assert!(compact.get(alias).is_none());
                value[canonical] = value[alias].clone();
                assert_eq!(compact_result(name, &value), compact);
                value[canonical] = Value::Null;
                assert_eq!(compact_result(name, &value), value);
            }
            for (alias, canonical) in pairs {
                let mut hit = json!({"score":0.7,"metadata":{"graph_id":"g","graphId":"g"}});
                hit[alias] = json!("same");
                hit[canonical] = json!("same");
                let value = json!({"graph_id":"g","graphId":"g","results":[hit]});
                let compact = compact_result(name, &value);
                assert!(compact["results"][0].get(alias).is_none());
                assert_eq!(compact["results"][0][canonical], "same");
                assert_eq!(compact["results"][0]["score"], 0.7);
                assert_eq!(
                    compact["results"][0]["metadata"],
                    value["results"][0]["metadata"]
                );
                assert_eq!(compact_result(name, &compact), compact);
                let mut divergent = value;
                divergent["graphId"] = json!("different");
                divergent["results"][0][canonical] = Value::Null;
                assert_eq!(compact_result(name, &divergent), divergent);
            }
        }
    }

    #[test]
    fn block_mutation_acks_name_each_block_once() {
        // Shapes as block_insert and block_update actually return them.
        let insert = json!({"success": true, "block_ids": ["block-a1"], "blocks": ["block-a1 Hello"],
            "blocks_inserted": 1, "blocks_existing": 0, "existing_block_ids": [], "block_count": 3});
        let compact = compact_result("insert_blocks", &insert);
        assert!(compact.get("block_ids").is_none());
        assert_eq!(compact["blocks"], insert["blocks"]);
        assert_eq!(compact["existing_block_ids"], json!([]));

        let update = json!({"success": true, "updated": ["block-a1", "block-b2"],
            "blocks": ["block-a1 Hello worl", "block-b2 Second lin"], "missing": []});
        let compact = compact_result("update_blocks", &update);
        assert!(compact.get("updated").is_none());
        assert_eq!(compact["blocks"], update["blocks"]);
        assert_eq!(compact["missing"], json!([]));

        let mismatch = json!({"updated": ["block-a1"], "blocks": ["block-zz Other"], "missing": []});
        assert_eq!(compact_result("update_blocks", &mismatch), mismatch, "a mismatch stays visible");
        let omitted = json!({"block_ids": ["block-a1"], "blocks": {"omitted": 401, "reason": "use block_ids"}});
        assert_eq!(compact_result("insert_blocks", &omitted), omitted);
    }

    #[test]
    fn document_aliases_preserve_body_chronicle_access_and_pagination() {
        let payload =
            json!({"created_at":"raw","createdAt":"raw","attrs":{"block_id":"b","blockId":"b"}});
        let value = json!({"graph_id":"g","document_id":"d","block_count":1,"block_ids":["b"],
            "created_at":"raw","createdAt":"raw","updated_by":null,"updatedBy":null,
            "readOnly":true,"derived":true,"readOnlyOwner":"geist","revision":4,
            "content":payload,"chronicle":payload,"metadata":payload,"page":{"nextCursor":"1"},"unknown":payload});
        let mut expected = value.clone();
        let object = expected.as_object_mut().unwrap();
        for (alias, canonical) in [
            ("graph_id", "graphId"),
            ("document_id", "documentId"),
            ("block_count", "blockCount"),
            ("block_ids", "blockIds"),
        ] {
            let value = object.remove(alias).unwrap();
            object.insert(canonical.into(), value);
        }
        object.remove("created_at");
        object.remove("updated_by");
        assert_eq!(compact_result("read_document", &value), expected);
        let divergent = json!({"graph_id":"g","graphId":"other","block_count":1,"blockCount":2,
            "created_at":"a","createdAt":"b","updated_by":"a","updatedBy":null});
        assert_eq!(compact_result("read_document", &divergent), divergent);
    }

    #[test]
    fn new_compactors_do_not_change_machine_envelopes_or_malformed_shapes() {
        for name in [
            "query_blocks",
            "search_blocks",
            "search_documents",
            "read_document",
        ] {
            let value =
                json!({"graph_id":"g","results":[{"block_id":"b","blockId":"b"}],"content":"body"});
            let envelope = ResponseView::Machine.envelope(name, value.clone());
            assert_eq!(envelope["structuredContent"], value);
            assert_eq!(
                envelope["content"][0]["text"],
                serde_json::to_string_pretty(&value).unwrap()
            );
            for malformed in [
                Value::Null,
                json!([{"graph_id":"g"}]),
                json!({"results":[null,1,"raw"],"unknown":true}),
            ] {
                assert_eq!(compact_result(name, &malformed), malformed);
            }
        }
    }

    #[test]
    fn default_machine_response_is_unchanged() {
        let value = json!({"content":"body","text":"body","warnings":["keep"],"revision":3});
        let result = ResponseView::from_params(&json!({}))
            .unwrap()
            .envelope("remember", value.clone());
        assert_eq!(result["structuredContent"], value);
        assert_eq!(
            result["content"][0]["text"],
            serde_json::to_string_pretty(&value).unwrap()
        );
    }

    #[test]
    fn views_are_explicit_and_invalid_versions_fail_before_execution() {
        assert_eq!(
            ResponseView::from_params(&json!({"_meta":{"garden/responseView":"agent-v1"}}))
                .unwrap(),
            ResponseView::AgentV1
        );
        assert!(
            ResponseView::from_params(&json!({"_meta":{"garden/responseView":"agent-v2"}}))
                .is_err()
        );
        assert!(ResponseView::from_params(&json!({"_meta":null})).is_err());
        assert_eq!(
            ResponseView::from_params(&json!({"_meta":{"other":"keep"}})).unwrap(),
            ResponseView::Machine
        );
    }

    #[test]
    fn memory_body_once_without_losing_provenance_or_warnings() {
        let value = json!({"number":8,"text":"8. a memory","content":"a memory","created_at":"1","createdAt":"1","rememberer":{"name":"epsilon","via":"credential"},"warnings":["projection failed"],"success":true});
        let result = compact_result("remember", &value);
        assert!(result.get("text").is_none());
        assert!(result.get("created_at").is_none());
        for key in ["content", "number", "rememberer", "warnings", "success"] {
            assert_eq!(result[key], value[key]);
        }
        assert!(value.get("text").is_some());
        let envelope = ResponseView::AgentV1.envelope("remember", value);
        assert_eq!(
            serde_json::from_str::<Value>(envelope["content"][0]["text"].as_str().unwrap())
                .unwrap(),
            envelope["structuredContent"]
        );
    }

    #[test]
    fn conflicting_fields_unknown_shapes_and_pagination_are_preserved() {
        let value = json!({"number":1,"text":"different","content":"body","created_at":"1","createdAt":"2","page":{"nextCursor":"x"}});
        assert_eq!(compact_result("remember", &value), value);
        assert_eq!(compact_result("new_tool", &value), value);
    }

    #[test]
    fn attunement_compaction_is_idempotent_and_keeps_meaning() {
        let body = "A substantial memory. ".repeat(100);
        let recall = json!({"memories":[{"number":1,"text":format!("1. {body}"),"content":body,"rememberer":{"name":"epsilon"}}],"count":1,"source":"local-memory-store"});
        let compact = compact_result("recall", &recall);
        assert!(compact.to_string().len() * 100 < recall.to_string().len() * 55);
        assert_eq!(compact_result("recall", &compact), compact);
        let blocks = json!({"blocks":[{"text":"meaning","content":"meaning","block_id":"b","blockId":"b","doc_id":"d","docId":"d","document_id":"d","documentId":"d","importance":4,"valence":-2,"score":0.5}],"value_store_available":false,"valueStoreAvailable":false,"warning":"unavailable"});
        let compact = compact_result("get_important_blocks", &blocks);
        assert_eq!(
            compact["blocks"][0],
            json!({"content":"meaning","blockId":"b","documentId":"d","importance":4,"valence":-2,"score":0.5})
        );
        assert_eq!(compact["warning"], "unavailable");
        assert_eq!(compact["valueStoreAvailable"], false);
        let song =
            json!({"verses":["first","second"],"verse_count":2,"verseCount":2,"coda":"keep"});
        let compact = compact_result("music", &song);
        assert_eq!(compact["verses"], song["verses"]);
        assert_eq!(compact["coda"], "keep");
        assert!(compact.get("verse_count").is_none());
    }
}
