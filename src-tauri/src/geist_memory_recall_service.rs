use crate::app_runtime::AppHandle;
use crate::{
    document_projection_service::document_snippet,
    document_service::DocumentRecord,
    geist_memory_store::{
        memory_json, memory_sort_key, memory_text, read_memory_store, LocalMemoryRecord,
        LocalMemoryStore,
    },
    mcp_utils::{mcp_arg_string, mcp_arg_u64, mcp_arg_usize, mcp_graph_id_or_default},
    paths::existing_graph_dir,
};
use std::collections::BTreeMap;

pub(super) fn mcp_local_recent_document_recall(
    documents: &[DocumentRecord],
    limit: usize,
) -> serde_json::Value {
    let memories = documents
        .iter()
        .take(limit.min(20))
        .enumerate()
        .map(|(index, document)| {
            let preview =
                document_snippet(document).unwrap_or_else(|| "No preview available".to_string());
            serde_json::json!({
                "number": index + 1,
                "text": format!("{}: {}", document.title, preview),
                "document_id": document.document_id.clone(),
                "documentId": document.document_id.clone(),
                "title": document.title.clone(),
                "created_at": document.created_at.clone(),
                "createdAt": document.created_at.clone(),
                "last_active": document.updated_at.clone(),
                "lastActive": document.updated_at.clone(),
                "source": "local-recent-document",
            })
        })
        .collect::<Vec<_>>();
    let count = memories.len();
    serde_json::json!({
        "memories": memories,
        "count": count,
        "source": "local-recent-documents",
        "memory_queue_available": false,
        "memoryQueueAvailable": false,
    })
}

pub(super) fn mcp_local_recall_memories(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    validate_recall_modes(arguments)?;
    let graph_id = mcp_graph_id_or_default(&app, arguments)?;
    let graph_dir = existing_graph_dir(&app, &graph_id)?;
    let store = read_memory_store(&graph_dir, &graph_id)?;
    let query = mcp_arg_string(arguments, &["query"]);
    let limit = mcp_arg_usize(arguments, &["limit"], 5).clamp(0, 100);
    let observer = mcp_arg_string(arguments, &["observer_agent_id", "observerAgentId"])
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

    // Per-observer recall (Variant B): when the caller supplies an observer identity
    // (the agt:Agent IRI the choreograph `withObserver` seam injects), read from THAT
    // witness's perspective graph via the current-head SPARQL path (`recall_for_observer`)
    // — never the flat shared queue, which would leak other agents' memories. Commons
    // recall (no observer) keeps the legacy behavior. Note: `number` lookup is not
    // supported per-observer; an observer-scoped call returns the current-head/query
    // recall from that perspective graph.
    if let Some(observer) = observer.as_deref() {
        let hits = crate::emporium::memory_applier::recall_for_observer(
            &app,
            &graph_id,
            observer,
            query.as_deref().unwrap_or(""),
            limit,
        )?;
        let memories = hits
            .iter()
            .enumerate()
            .map(|(index, hit)| {
                serde_json::json!({
                    "number": index + 1,
                    "text": hit.content.clone(),
                    "content": hit.content.clone(),
                    "subject": hit.subject.clone(),
                    "created_at": hit.created_at,
                    "createdAt": hit.created_at,
                    "observer_agent_id": observer,
                    "observedBy": observer,
                    "source": "per-observer-projection",
                })
            })
            .collect::<Vec<_>>();
        let count = memories.len();
        return Ok(serde_json::json!({
            "memories": memories,
            "count": count,
            "source": "per-observer-projection",
            "observer_agent_id": observer,
        }));
    }

    Ok(shared_memory_recall(&store, arguments))
}

/// `recall` as the dispatcher calls it: the shared recall, plus the caller's
/// last four Diary lines when the connection is an agent with a diary (Geist
/// 2.1: "last-4 ride recall"). Silence when there is nothing: no key at all.
pub(super) fn mcp_local_recall_memories_for(
    app: AppHandle,
    arguments: &serde_json::Value,
    actor: Option<crate::document_chronicle::ChronicleActor>,
) -> Result<serde_json::Value, String> {
    let mut result = mcp_local_recall_memories(app.clone(), arguments)?;
    let diary_limit = mcp_arg_usize(arguments, &["diaryLimit", "diary_limit"], crate::geist_diary_store::DIARY_RECALL_LINES);
    if result.get("observer_agent_id").is_none() {
        if let Ok(graph_id) = mcp_graph_id_or_default(&app, arguments) {
            if let Ok(graph_dir) = existing_graph_dir(&app, &graph_id) {
                let window = arguments.get("diaryWindowTokens").and_then(serde_json::Value::as_u64);
                if let Some(diary) = crate::geist_diary_service::diary_recall_lines(&graph_dir, &graph_id, &actor, diary_limit, window) {
                    result["diary"] = diary;
                }
            }
        }
    }
    Ok(result)
}

fn validate_recall_modes(arguments: &serde_json::Value) -> Result<(), String> {
    if let Some(value) = arguments.get("documentId") {
        if !value.as_str().is_some_and(|id| !id.trim().is_empty()) { return Err("documentId must be a non-empty string".into()); }
        if arguments.get("number").is_some() || mcp_arg_string(arguments, &["observer_agent_id", "observerAgentId"]).is_some() {
            return Err("documentId filtering cannot be combined with number or observer-specific recall".into());
        }
        if arguments.get("limit").and_then(serde_json::Value::as_u64) == Some(0) { return Err("documentId-filtered recall requires limit >= 1".into()); }
    } else if arguments.get("offset").is_some() { return Err("offset requires documentId filtering".into()); }
    match arguments.get("rememberer") {
        None | Some(serde_json::Value::Null) => return Ok(()),
        Some(serde_json::Value::String(value)) if !value.trim().is_empty() => {}
        _ => return Err("rememberer must be a non-empty string".into()),
    }
    if mcp_arg_string(arguments, &["observer_agent_id", "observerAgentId"]).is_some() {
        return Err("rememberer and observer-specific recall are conflicting modes".into());
    }
    Ok(())
}

fn shared_memory_recall(
    store: &LocalMemoryStore,
    arguments: &serde_json::Value,
) -> serde_json::Value {
    let document_id = mcp_arg_string(arguments, &["documentId"]);
    let offset = if document_id.is_some() { mcp_arg_usize(arguments, &["offset"], 0) } else { 0 };
    let number = mcp_arg_u64(arguments, &["number"]);
    let query = mcp_arg_string(arguments, &["query"]);
    let limit = mcp_arg_usize(arguments, &["limit"], 8).clamp(0, 100);
    // Validated at the public entrypoint. This is provenance filtering, not auth;
    // preserve the supplied string exactly rather than case-folding/trimming it.
    let rememberer = arguments
        .get("rememberer")
        .and_then(serde_json::Value::as_str);
    let mut candidates: BTreeMap<u64, (&LocalMemoryRecord, Option<&str>)> = store
        .memories
        .values()
        .map(|memory| (memory.number, (memory, None)))
        .collect();
    if number.is_some() || query.is_some() || document_id.is_some() {
        // Active authority wins before filtering. Archive ID order also makes
        // duplicate historical records independent of archive-vector ordering.
        let mut archives = store.archives.iter().collect::<Vec<_>>();
        archives.sort_by(|left, right| left.archive_doc_id.cmp(&right.archive_doc_id));
        for archive in archives {
            for memory in &archive.memories {
                candidates
                    .entry(memory.number)
                    .or_insert((memory, Some(archive.archive_doc_id.as_str())));
            }
        }
    }
    let query_lower = query.as_ref().map(|value| value.to_ascii_lowercase());
    let mut memories =
        candidates
            .into_values()
            .filter(|(memory, _)| {
                if document_id.as_ref().is_some_and(|id| !memory.anchors.iter().any(|a| a.document_id == *id)) { return false; }
                if let Some(rememberer) = rememberer {
                    if !memory.rememberer.as_ref().is_some_and(|actor| {
                        actor.name == rememberer || actor.label() == rememberer
                    }) {
                        return false;
                    }
                }
                if let Some(number) = number {
                    return memory.number == number;
                }
                query_lower
                    .as_ref()
                    .map(|query| memory_text(memory).to_ascii_lowercase().contains(query))
                    .unwrap_or(true)
            })
            .collect::<Vec<_>>();
    memories.sort_by(|(left, _), (right, _)| {
        memory_sort_key(right)
            .cmp(&memory_sort_key(left))
            .then_with(|| right.number.cmp(&left.number))
    });
    let total = memories.len();
    let start = offset.min(total);
    // Number lookup remains exact and independent of query/limit, as before.
    if number.is_none() {
        memories.drain(..start);
        memories.truncate(limit);
    }
    let rows = memories
        .into_iter()
        .map(|(memory, archive)| {
            let mut row = memory_json(memory);
            if let Some(archive) = archive {
                row["archived"] = serde_json::json!(true);
                row["archive_document_id"] = serde_json::json!(archive);
                row["archiveDocumentId"] = serde_json::json!(archive);
            }
            row
        })
        .collect::<Vec<_>>();
    let count = rows.len();
    let mut output = serde_json::json!({
        "memories": rows,
        "count": count,
        "source": "local-memory-store",
    });
    if document_id.is_some() {
        let end = if number.is_some() { count } else { start + count };
        output["page"] = serde_json::json!({"offset":start,"total":total,"nextOffset":if end < total {Some(end)} else {None},"complete":end>=total});
    }
    if let Some(number) = number.filter(|_| count == 0) {
        output["note"] = serde_json::json!(format!("Memory #{number} not found"));
    }
    output
}

pub(super) fn mcp_local_recall_or_recent_documents(
    app: AppHandle,
    graph_id: &str,
    documents: &[DocumentRecord],
    limit: usize,
) -> serde_json::Value {
    let arguments = serde_json::json!({
        "graphId": graph_id,
        "limit": limit,
    });
    match mcp_local_recall_memories(app, &arguments) {
        Ok(value)
            if value
                .get("count")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0)
                > 0 =>
        {
            value
        }
        _ => mcp_local_recent_document_recall(documents, limit),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geist_memory_store::LocalMemoryArchiveRecord;
    use serde_json::{json, Value};

    #[test]
    fn document_filter_reads_archives_and_pages_without_activity_changes() {
        let mut data = store();
        let anchor = crate::geist_memory_anchors::MemoryAnchor { document_id:"d".into(),block_id:None,title_at_creation:"D".into() };
        data.memories.get_mut("1").unwrap().anchors.push(anchor.clone());
        data.archives[0].memories[0].anchors.push(anchor.clone());
        data.archives[0].memories[2].anchors.push(anchor); // active duplicate 10 is unanchored
        let before = serde_json::to_value(&data).unwrap();
        let first = shared_memory_recall(&data, &json!({"documentId":"d","limit":1}));
        assert_eq!(first["memories"][0]["number"],1);
        assert_eq!(first["page"]["nextOffset"],1);
        let second = shared_memory_recall(&data, &json!({"documentId":"d","limit":1,"offset":1}));
        assert_eq!(second["memories"][0]["number"],20);
        assert_eq!(second["memories"][0]["archived"],true);
        assert_eq!(second["page"]["complete"],true);
        assert_eq!(serde_json::to_value(&data).unwrap(),before);
        assert!(validate_recall_modes(&json!({"documentId":"d","limit":0})).is_err());
        assert!(validate_recall_modes(&json!({"offset":2})).is_err());
    }

    fn memory(number: u64, content: &str, time: &str) -> LocalMemoryRecord {
        LocalMemoryRecord {
            anchors: Vec::new(),
            number,
            block_id: format!("source-{number}"),
            content: content.into(),
            created_at: time.into(),
            last_active: time.into(),
            rememberer: None,
        }
    }

    fn archive(id: &str, memories: Vec<LocalMemoryRecord>) -> LocalMemoryArchiveRecord {
        LocalMemoryArchiveRecord {
            archive_doc_id: id.into(),
            archived_at: "9000".into(),
            kept: 1,
            archived: memories.len(),
            memories,
        }
    }

    fn store() -> LocalMemoryStore {
        LocalMemoryStore {
            schema_version: 1,
            graph_id: "fixture".into(),
            next_number: 30,
            memories: (1..=10)
                .map(|n| (n.to_string(), memory(n, "active match", "1000")))
                .collect(),
            archives: vec![archive(
                "archive-a",
                vec![
                    memory(20, "archived MATCH", ""),
                    memory(21, "archive newest", "9999"),
                    memory(10, "obsolete archived-only", "99999"),
                ],
            )],
        }
    }

    fn numbers(result: &Value) -> Vec<u64> {
        result["memories"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["number"].as_u64().unwrap())
            .collect()
    }

    #[test]
    fn recent_defaults_to_eight_active_only_and_breaks_ties_by_number_descending() {
        let result = shared_memory_recall(&store(), &json!({}));
        assert_eq!(numbers(&result), vec![10, 9, 8, 7, 6, 5, 4, 3]);
        assert_eq!(result["count"], 8);
        assert!(result["memories"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row.get("archived").is_none()));
    }

    #[test]
    fn number_searches_archive_with_stable_identity_and_provenance() {
        let result =
            shared_memory_recall(&store(), &json!({"number":20,"limit":0,"query":"no match"}));
        assert_eq!(numbers(&result), vec![20]);
        let row = &result["memories"][0];
        assert_eq!(row["archived"], true);
        assert_eq!(row["archive_document_id"], "archive-a");
        assert_eq!(row["archiveDocumentId"], "archive-a");
        assert_eq!(row["block_id"], "source-20");
        assert_eq!(row["content"], "archived MATCH");
        assert_eq!(row["created_at"], "");
        assert_eq!(row["last_active"], "");
        assert_eq!(result["source"], "local-memory-store");
    }

    #[test]
    fn query_searches_both_sources_and_active_wins_before_filtering() {
        let result = shared_memory_recall(&store(), &json!({"query":"mAtCh","limit":100}));
        assert_eq!(numbers(&result), vec![10, 9, 8, 7, 6, 5, 4, 3, 2, 1, 20]);
        assert_eq!(result["memories"][10]["archived"], true);
        let result = shared_memory_recall(&store(), &json!({"query":"obsolete"}));
        assert_eq!(result["count"], 0);
        let result = shared_memory_recall(&store(), &json!({"number":10}));
        assert_eq!(result["memories"][0]["content"], "active match");
        assert!(result["memories"][0].get("archived").is_none());
    }

    #[test]
    fn archive_duplicates_are_deterministic_and_unknown_clock_ties_use_number() {
        let mut store = store();
        store.archives.push(archive(
            "archive-z",
            vec![
                memory(20, "later duplicate", "99999"),
                memory(22, "archived MATCH", ""),
            ],
        ));
        let result = shared_memory_recall(&store, &json!({"query":"archived match"}));
        assert_eq!(numbers(&result), vec![22, 20]);
        assert_eq!(result["memories"][1]["archive_document_id"], "archive-a");
        store.archives.reverse();
        assert_eq!(
            shared_memory_recall(&store, &json!({"query":"archived match"})),
            result
        );
    }

    #[test]
    fn query_limits_and_numeric_timestamp_ordering_are_preserved() {
        let mut store = store();
        store.memories.get_mut("1").unwrap().last_active = "2026-09-10T20:00:00+00:00".into();
        let result = shared_memory_recall(&store, &json!({"query":"match","limit":2}));
        assert_eq!(numbers(&result), vec![1, 10]);
        assert_eq!(
            shared_memory_recall(&store, &json!({"query":"match","limit":0}))["count"],
            0
        );
        assert_eq!(
            shared_memory_recall(&store, &json!({"query":"match"}))["count"],
            8
        );
    }

    #[test]
    fn missing_number_and_archive_only_store_do_not_change_recent_semantics() {
        let mut store = store();
        let result = shared_memory_recall(&store, &json!({"number":999}));
        assert_eq!(result["count"], 0);
        assert_eq!(result["note"], "Memory #999 not found");
        store.memories.clear();
        assert_eq!(shared_memory_recall(&store, &json!({}))["count"], 0);
        assert_eq!(
            numbers(&shared_memory_recall(&store, &json!({"number":20}))),
            vec![20]
        );
    }

    #[test]
    fn rememberer_matches_exact_name_or_label_without_inferring_unknowns() {
        let mut store = store();
        let actor = crate::document_chronicle::ChronicleActor::agent("Alpha", None);
        store.memories.get_mut("2").unwrap().rememberer = Some(actor.clone());
        store.archives[0].memories[0].rememberer = Some(actor);
        for filter in ["Alpha", "agent:Alpha"] {
            let result = shared_memory_recall(&store, &json!({"rememberer":filter}));
            assert_eq!(numbers(&result), vec![2]);
            let result =
                shared_memory_recall(&store, &json!({"rememberer":filter,"query":"match"}));
            assert_eq!(numbers(&result), vec![2, 20]);
            assert_eq!(result["memories"][1]["archived"], true);
            let result = shared_memory_recall(&store, &json!({"rememberer":filter,"number":20}));
            assert_eq!(numbers(&result), vec![20]);
        }
        for filter in ["alpha", "agent:alpha", "Alpha ", "unknown", "Beta"] {
            assert_eq!(
                shared_memory_recall(&store, &json!({"rememberer":filter,"query":"match"}))
                    ["count"],
                0
            );
        }
        assert_eq!(
            shared_memory_recall(&store, &json!({"rememberer":"Alpha","number":1}))["count"],
            0
        );
        assert_eq!(
            shared_memory_recall(&store, &json!({"number":1}))["count"],
            1
        );
    }

    #[test]
    fn rememberer_does_not_resurrect_shadowed_archive_records_or_apply_quotas() {
        let mut store = store();
        let actor = crate::document_chronicle::ChronicleActor::agent("Alpha", None);
        store.archives[0].memories[2].rememberer = Some(actor.clone());
        assert_eq!(
            shared_memory_recall(&store, &json!({"rememberer":"Alpha","number":10}))["count"],
            0
        );
        for memory in store.memories.values_mut() {
            memory.rememberer = Some(actor.clone());
        }
        assert_eq!(
            shared_memory_recall(&store, &json!({"rememberer":"Alpha"}))["count"],
            8
        );
        assert_eq!(
            shared_memory_recall(&store, &json!({"rememberer":"Alpha","limit":100}))["count"],
            10
        );
    }

    #[test]
    fn rememberer_and_observer_modes_are_rejected_before_reading_store() {
        for key in ["observer_agent_id", "observerAgentId"] {
            let mut arguments = json!({"rememberer":"Alpha"});
            arguments[key] = json!("agent:Alpha");
            assert!(validate_recall_modes(&arguments)
                .unwrap_err()
                .contains("conflicting modes"));
            arguments.as_object_mut().unwrap().remove("rememberer");
            assert!(validate_recall_modes(&arguments).is_ok());
        }
        for rememberer in [json!(true), json!(42), json!(""), json!("   "), json!({})] {
            assert!(validate_recall_modes(&json!({"rememberer":rememberer})).is_err());
        }
        assert!(validate_recall_modes(&json!({"rememberer":"Alpha"})).is_ok());
        assert!(validate_recall_modes(&json!({"rememberer":null})).is_ok());
    }
}
