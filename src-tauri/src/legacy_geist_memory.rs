//! Cloud-1 geist.py memory authority -> native shared queue, without source edits.
//! Archive paragraphs retain their source block IDs, not the discarded queue IDs.
//! Cloud-1 archives contain no per-memory timestamps; empty strings mean unknown.

use crate::geist_memory_store::{LocalMemoryArchiveRecord, LocalMemoryRecord, LocalMemoryStore};
use chrono::{DateTime, NaiveDateTime, SecondsFormat};
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

const QUEUE: &str = "geist-memory-queue";
const ARCHIVE: &str = "geist-memory-archive-";
const META: &str = "__geist_meta__:";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Metadata {
    next_number: u64,
    #[serde(deserialize_with = "unique_entries")]
    memories: BTreeMap<String, Entry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    b: String,
    c: String,
    a: String,
}

fn unique_entries<'de, D: Deserializer<'de>>(de: D) -> Result<BTreeMap<String, Entry>, D::Error> {
    struct Entries;
    impl<'de> serde::de::Visitor<'de> for Entries {
        type Value = BTreeMap<String, Entry>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("unique numbered memory metadata")
        }
        fn visit_map<M: serde::de::MapAccess<'de>>(
            self,
            mut map: M,
        ) -> Result<Self::Value, M::Error> {
            let mut entries = BTreeMap::new();
            while let Some((key, value)) = map.next_entry::<String, Entry>()? {
                if entries.insert(key, value).is_some() {
                    return Err(serde::de::Error::custom("duplicate memory metadata number"));
                }
            }
            Ok(entries)
        }
    }
    de.deserialize_map(Entries)
}

fn number(text: &str) -> Result<u64, String> {
    let n = text.parse::<u64>().map_err(|_| "invalid memory number")?;
    if n == 0 || n.to_string() != text {
        return Err("memory number must be a canonical positive integer".into());
    }
    Ok(n)
}

fn stamp(text: &str) -> Result<String, String> {
    if !text.is_empty() {
        DateTime::parse_from_rfc3339(text).map_err(|_| "invalid cloud memory timestamp")?;
    }
    Ok(text.to_owned())
}

fn block_id(node: &Value) -> Result<&str, String> {
    node["attrs"]["data-block-id"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| "memory block has no source block ID".into())
}

fn fields(value: &Value, allowed: &[&str]) -> Result<(), String> {
    let object = value.as_object().ok_or("memory node must be an object")?;
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err("unknown memory node field".into());
    }
    Ok(())
}

fn text(node: &Value, kind: &str) -> Result<String, String> {
    fields(node, &["type", "attrs", "content"])?;
    if node["type"] != kind {
        return Err(format!("expected {kind} in memory document"));
    }
    let children = node["content"]
        .as_array()
        .ok_or("memory block lacks inline content")?;
    let mut text = String::new();
    for child in children {
        fields(child, &["type", "text", "marks"])?;
        if child["type"] != "text" || child.get("content").is_some() {
            return Err("unsupported non-text memory inline node".into());
        }
        if let Some(marks) = child.get("marks") {
            for mark in marks.as_array().ok_or("malformed memory text marks")? {
                if !matches!(mark["type"].as_str(), Some("bold" | "strong"))
                    || mark.as_object().is_none_or(|m| m.len() != 1)
                {
                    return Err("unsupported memory text mark".into());
                }
            }
        }
        text.push_str(
            child["text"]
                .as_str()
                .ok_or("memory text is not a string")?,
        );
    }
    Ok(text)
}

fn blocks(document: &Value) -> Result<&Vec<Value>, String> {
    fields(document, &["type", "attrs", "content"])?;
    if document["type"] != "doc" {
        return Err("memory authority must be canonical TipTap doc JSON".into());
    }
    let blocks = document["content"]
        .as_array()
        .ok_or("memory document lacks content")?;
    if blocks.len() < 2 {
        return Err("memory document lacks heading and metadata/summary".into());
    }
    let mut ids = BTreeSet::new();
    for block in blocks {
        if !ids.insert(block_id(block)?) {
            return Err("duplicate memory document block ID".into());
        }
    }
    // Legacy XML writers can leave level as "1"; Yrs also emits numeric 1.0.
    let level = &document["content"][0]["attrs"]["level"];
    if level.as_f64() != Some(1.0) && level.as_str() != Some("1") {
        return Err("memory heading must be level 1".into());
    }
    Ok(blocks)
}

fn numbered(node: &Value) -> Result<(u64, String), String> {
    let raw = text(node, "paragraph")?;
    let (label, content) = raw
        .split_once(". ")
        .ok_or("memory block lacks numbered prefix")?;
    // Strip ONE structural label only. Archive/rewrite already duplicated labels
    // in cloud-1; removing more would silently alter source content.
    Ok((number(label)?, content.to_owned()))
}

pub(crate) fn parse_memory(
    graph_id: &str,
    documents: &BTreeMap<String, Value>,
) -> Result<Option<LocalMemoryStore>, String> {
    let archives = documents
        .iter()
        .filter(|(id, _)| id.starts_with(ARCHIVE))
        .collect::<Vec<_>>();
    let Some(queue) = documents.get(QUEUE) else {
        return if archives.is_empty() {
            Ok(None)
        } else {
            Err("memory archives without queue: next_number authority is absent".into())
        };
    };
    crate::ids::validate_local_id(graph_id, "graph_id")?;
    let nodes = blocks(queue)?;
    if text(&nodes[0], "heading")? != "Memory Queue" {
        return Err("unexpected memory queue heading".into());
    }
    let metadata_text = text(&nodes[1], "paragraph")?;
    let metadata: Metadata = serde_json::from_str(
        metadata_text
            .strip_prefix(META)
            .ok_or("missing __geist_meta__: memory authority")?,
    )
    .map_err(|e| format!("invalid memory metadata: {e}"))?;
    if metadata.next_number == 0 || metadata.next_number == u64::MAX {
        return Err("invalid/overflowing next memory number".into());
    }
    let mut by_id = BTreeMap::new();
    for node in &nodes[2..] {
        by_id.insert(block_id(node)?, node);
    }
    let mut store = LocalMemoryStore {
        schema_version: 1,
        graph_id: graph_id.into(),
        next_number: metadata.next_number,
        memories: BTreeMap::new(),
        archives: Vec::new(),
    };
    let mut numbers = BTreeSet::new();
    for (key, entry) in metadata.memories {
        let n = number(&key)?;
        let node = by_id
            .remove(entry.b.as_str())
            .ok_or("memory metadata references absent/reused block")?;
        let (label, content) = numbered(node)?;
        if label != n {
            return Err("memory metadata number disagrees with block label".into());
        }
        numbers.insert(n);
        store.memories.insert(
            key,
            LocalMemoryRecord {
                anchors: Vec::new(),
                number: n,
                block_id: entry.b,
                content,
                created_at: stamp(&entry.c)?,
                last_active: stamp(&entry.a)?,
                rememberer: None,
            },
        );
    }
    if !by_id.is_empty() {
        return Err("unindexed memory blocks: incomplete metadata transaction".into());
    }
    for (id, document) in archives {
        let label = id.strip_prefix(ARCHIVE).unwrap();
        let archived = NaiveDateTime::parse_from_str(label, "%Y-%m-%d_%H%M%S")
            .map_err(|_| "unknown cloud memory archive ID format")?;
        if archived.format("%Y-%m-%d_%H%M%S").to_string() != label {
            return Err("noncanonical memory archive timestamp label".into());
        }
        let nodes = blocks(document)?;
        if text(&nodes[0], "heading")? != format!("Memory Archive \u{2014} {label}") {
            return Err("memory archive heading/ID mismatch".into());
        }
        let summary = text(&nodes[1], "paragraph")?;
        let (count, kept) = summary
            .strip_prefix("Archived ")
            .and_then(|s| s.strip_suffix(" most recently active."))
            .and_then(|s| s.split_once(" memories. Kept "))
            .ok_or("unknown memory archive summary")?;
        let parse_count = |s: &str| -> Result<usize, String> {
            let n = s.parse::<usize>().map_err(|_| "invalid archive count")?;
            if n.to_string() != s {
                return Err("noncanonical archive count".into());
            }
            Ok(n)
        };
        let count = parse_count(count)?;
        let kept = parse_count(kept)?;
        if count == 0 || kept == 0 || count != nodes.len() - 2 {
            return Err("memory archive count/content mismatch".into());
        }
        let mut memories = Vec::new();
        for node in &nodes[2..] {
            let (n, content) = numbered(node)?;
            if !numbers.insert(n) {
                return Err("memory number appears in multiple active/archive records".into());
            }
            memories.push(LocalMemoryRecord {
                anchors: Vec::new(),
                number: n,
                block_id: block_id(node)?.into(),
                content,
                created_at: String::new(),
                last_active: String::new(),
                rememberer: None,
            });
        }
        store.archives.push(LocalMemoryArchiveRecord {
            archive_doc_id: id.clone(),
            // geist.py constructs the ID with datetime.now(timezone.utc).
            // This is only second-resolution archive time, never memory provenance.
            archived_at: archived
                .and_utc()
                .to_rfc3339_opts(SecondsFormat::Secs, false),
            kept,
            archived: count,
            memories,
        });
    }
    if numbers.last().is_some_and(|n| *n >= store.next_number) {
        return Err("next_number does not exceed every active/archived memory".into());
    }
    Ok(Some(store))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn paragraph(id: &str, text: &str) -> Value {
        json!({"type":"paragraph","attrs":{"data-block-id":id},
            "content":[{"type":"text","text":text}]})
    }
    fn heading(text: &str) -> Value {
        json!({"type":"heading","attrs":{"data-block-id":"heading","level":1},
            "content":[{"type":"text","text":text}]})
    }
    fn fixture() -> BTreeMap<String, Value> {
        let meta = json!({"next_number":9,"memories":{"7":{
            "b":"source-block","c":"2026-08-25T15:45:55.123456+00:00",
            "a":"2026-08-26T10:45:55.987654-07:00"}}});
        BTreeMap::from([(
            QUEUE.into(),
            json!({"type":"doc","content":[
            heading("Memory Queue"), paragraph("meta", &format!("{META}{meta}")),
            {"type":"paragraph","attrs":{"data-block-id":"source-block"},"content":[
                {"type":"text","text":"7.","marks":[{"type":"bold"}]},
                {"type":"text","text":" line one\n<literal> & two  "}]}]}),
        )])
    }
    fn set_meta(docs: &mut BTreeMap<String, Value>, meta: Value) {
        docs.get_mut(QUEUE).unwrap()["content"][1] = paragraph("meta", &format!("{META}{meta}"));
    }
    fn add_archive(docs: &mut BTreeMap<String, Value>) {
        docs.insert(
            format!("{ARCHIVE}2026-08-26_123456"),
            json!({"type":"doc","content":[
            heading("Memory Archive \u{2014} 2026-08-26_123456"),
            paragraph("summary","Archived 1 memories. Kept 1 most recently active."),
            paragraph("archive-block","2. 2. original content")]}),
        );
    }

    #[test]
    fn preserves_queue_and_never_mutates_documents() {
        let docs = fixture();
        let before = docs.clone();
        let store = parse_memory("target", &docs).unwrap().unwrap();
        assert_eq!(store.graph_id, "target");
        assert_eq!(store.next_number, 9);
        let row = &store.memories["7"];
        assert_eq!(row.number, 7);
        assert_eq!(row.block_id, "source-block");
        assert_eq!(row.content, "line one\n<literal> & two  ");
        assert_eq!(row.created_at, "2026-08-25T15:45:55.123456+00:00");
        assert_eq!(row.last_active, "2026-08-26T10:45:55.987654-07:00");
        assert_eq!(docs, before);
    }

    #[test]
    fn accepts_yrs_float_heading_levels_but_not_other_values() {
        let mut docs = fixture();
        add_archive(&mut docs);
        for document in docs.values_mut() {
            document["content"][0]["attrs"]["level"] = json!(1.0);
        }
        assert!(parse_memory("target", &docs).unwrap().is_some());
        for level in [json!(1.5), json!(2), json!("1.0"), json!("2"), Value::Null] {
            docs.get_mut(QUEUE).unwrap()["content"][0]["attrs"]["level"] = level;
            assert!(parse_memory("target", &docs).is_err());
        }
    }

    #[test]
    fn accepts_legacy_xml_string_level_in_february_archive() {
        let mut docs = fixture();
        add_archive(&mut docs);
        let mut archive = docs
            .remove("geist-memory-archive-2026-08-26_123456")
            .unwrap();
        archive["content"][0] = heading("Memory Archive \u{2014} 2026-02-17_213746");
        archive["content"][0]["attrs"]["level"] = json!("1");
        docs.insert("geist-memory-archive-2026-02-17_213746".into(), archive);
        let store = parse_memory("target", &docs).unwrap().unwrap();
        assert_eq!(store.archives[0].archived_at, "2026-02-17T21:37:46+00:00");
        assert_eq!(store.archives[0].memories[0].block_id, "archive-block");
    }

    #[test]
    fn preserves_archives_without_fabricating_memory_provenance() {
        let mut docs = fixture();
        add_archive(&mut docs);
        let store = parse_memory("target", &docs).unwrap().unwrap();
        let archive = &store.archives[0];
        assert_eq!(
            archive.archive_doc_id,
            "geist-memory-archive-2026-08-26_123456"
        );
        assert_eq!(archive.archived_at, "2026-08-26T12:34:56+00:00");
        assert_eq!((archive.kept, archive.archived), (1, 1));
        let row = &archive.memories[0];
        assert_eq!(row.number, 2);
        assert_eq!(row.block_id, "archive-block");
        assert_eq!(row.content, "2. original content");
        assert!(row.created_at.is_empty() && row.last_active.is_empty());
    }

    #[test]
    fn absent_is_none_empty_seed_is_some_archives_alone_are_error() {
        assert!(parse_memory("target", &BTreeMap::new()).unwrap().is_none());
        let mut docs = fixture();
        docs.get_mut(QUEUE).unwrap()["content"]
            .as_array_mut()
            .unwrap()
            .truncate(2);
        set_meta(&mut docs, json!({"next_number":1,"memories":{}}));
        assert!(parse_memory("target", &docs)
            .unwrap()
            .unwrap()
            .memories
            .is_empty());
        docs.clear();
        add_archive(&mut docs);
        assert!(parse_memory("target", &docs).is_err());
    }

    #[test]
    fn rejects_metadata_shape_reference_number_and_timestamp_failures() {
        for meta in [
            json!({}),
            json!({"next_number":9,"memories":{},"unknown":true}),
            json!({"next_number":9,"memories":{}}),
            json!({"next_number":0,"memories":{}}),
            json!({"next_number":9,"memories":{"7":{"b":null,"c":"","a":""}}}),
            json!({"next_number":9,"memories":{"7":{"b":"missing","c":"","a":""}}}),
            json!({"next_number":9,"memories":{"07":{"b":"source-block","c":"","a":""}}}),
            json!({"next_number":7,"memories":{"7":{"b":"source-block","c":"","a":""}}}),
            json!({"next_number":9,"memories":{"8":{"b":"source-block","c":"","a":""}}}),
            json!({"next_number":9,"memories":{"7":{"b":"source-block","c":"bad","a":""}}}),
            json!({"next_number":9,"memories":{"7":{"b":"source-block","c":""}}}),
        ] {
            let mut docs = fixture();
            set_meta(&mut docs, meta);
            assert!(parse_memory("target", &docs).is_err());
        }
    }

    #[test]
    fn rejects_duplicate_metadata_keys_and_rich_or_unidentified_blocks() {
        let mut docs = fixture();
        let entry = r#"{"b":"source-block","c":"","a":""}"#;
        docs.get_mut(QUEUE).unwrap()["content"][1] = paragraph(
            "meta",
            &format!("{META}{{\"next_number\":9,\"memories\":{{\"7\":{entry},\"7\":{entry}}}}}"),
        );
        assert!(parse_memory("target", &docs).is_err());
        for replacement in [
            json!({"type":"image","attrs":{"data-block-id":"source-block"}}),
            json!({"type":"paragraph","content":[{"type":"text","text":"7. lost ID"}]}),
            paragraph("meta", "7. duplicate ID"),
        ] {
            let mut docs = fixture();
            docs.get_mut(QUEUE).unwrap()["content"][2] = replacement;
            assert!(parse_memory("target", &docs).is_err());
        }
    }

    #[test]
    fn rejects_unknown_structure_and_preserves_explicitly_absent_timestamps() {
        let mut docs = fixture();
        docs.get_mut(QUEUE).unwrap()["content"][2]["content"][1]["unknown"] = json!(true);
        assert!(parse_memory("target", &docs).is_err());
        let mut docs = fixture();
        docs.get_mut(QUEUE).unwrap()["content"][2]["content"][1]["marks"] =
            json!([{"type":"link","attrs":{"href":"https://example.com"}}]);
        assert!(parse_memory("target", &docs).is_err());
        let mut docs = fixture();
        set_meta(
            &mut docs,
            json!({"next_number":9,"memories":{
            "7":{"b":"source-block","c":"","a":""}}}),
        );
        let store = parse_memory("target", &docs).unwrap().unwrap();
        assert!(store.memories["7"].created_at.is_empty());
        assert!(store.memories["7"].last_active.is_empty());
    }

    #[test]
    fn rejects_archive_count_duplicate_number_and_counter_conflicts() {
        for replacement in ["7. active duplicate", "9. counter collision"] {
            let mut docs = fixture();
            add_archive(&mut docs);
            docs.get_mut("geist-memory-archive-2026-08-26_123456")
                .unwrap()["content"][2] = paragraph("archive-block", replacement);
            assert!(parse_memory("target", &docs).is_err());
        }
        let mut docs = fixture();
        add_archive(&mut docs);
        docs.get_mut("geist-memory-archive-2026-08-26_123456")
            .unwrap()["content"][1] = paragraph(
            "summary",
            "Archived 2 memories. Kept 1 most recently active.",
        );
        assert!(parse_memory("target", &docs).is_err());
    }
}
