//! Read-only migration evidence. Never use the healing record-store APIs here.

use crate::{crdt_engine::projection, document_types::DocumentRecord};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

pub(crate) fn read_records(
    graph_dir: &Path,
    graph_id: &str,
) -> Result<Vec<DocumentRecord>, String> {
    // These are the literal on-disk names used by document_record_store.
    let directory = graph_dir.join("documents");
    let entries = fs::read_dir(&directory)
        .map_err(|error| format!("read {}: {error}", directory.display()))?;
    let mut records = Vec::new();
    let mut seen = BTreeSet::new();
    for entry in entries {
        let entry = entry.map_err(|error| format!("read document entry: {error}"))?;
        let kind = entry.file_type().map_err(|error| error.to_string())?;
        if kind.is_symlink() {
            return Err(format!(
                "document evidence is a symlink: {}",
                entry.path().display()
            ));
        }
        if !kind.is_dir() {
            continue;
        }
        let id = entry
            .file_name()
            .into_string()
            .map_err(|_| "document directory name is not UTF-8".to_string())?;
        crate::ids::validate_local_id(&id, "document_id")?;
        if crate::document_tombstone_store::document_is_tombstoned(graph_dir, &id)? {
            continue;
        }
        let path = entry.path().join("document.json");
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        if !metadata.file_type().is_file() {
            return Err(format!(
                "document evidence is not a regular file: {}",
                path.display()
            ));
        }
        let bytes = fs::read(&path).map_err(|error| format!("read {}: {error}", path.display()))?;
        let mut record: DocumentRecord = serde_json::from_slice(&bytes)
            .map_err(|error| format!("parse {}: {error}", path.display()))?;
        if record.graph_id != graph_id {
            return Err(format!(
                "graph identity mismatch at {}: expected {graph_id}, found {}",
                path.display(),
                record.graph_id
            ));
        }
        if !seen.insert(record.document_id.clone()) {
            return Err(format!("duplicate document record: {}", record.document_id));
        }
        if record.document_id != id {
            return Err(format!(
                "document identity mismatch at {}: expected {id}, found {}",
                path.display(),
                record.document_id
            ));
        }
        // Authority is read independently from its sidecar; do not retain legacy
        // inline histories across a whole-graph scan.
        record.ydoc_update_base64.clear();
        records.push(record);
    }
    records.sort_by(|a, b| a.document_id.cmp(&b.document_id));
    Ok(records)
}

pub(crate) fn projection_errors(record: &DocumentRecord, doc: &yrs::Doc) -> Vec<String> {
    let projected = projection::materialize_ydoc(doc, &record.document_id);
    let mut errors = Vec::new();
    if record.body != projected.body {
        errors.push("body differs from Y.Doc projection".into());
    }
    match &record.tiptap_json {
        Some(stored)
            if canonical_tiptap(stored)
                == canonical_tiptap(&projection::ydoc_to_tiptap_json(doc)) => {}
        _ => errors.push("TipTap JSON missing or differs from Y.Doc projection".into()),
    }
    match (
        xml_semantics(&record.tiptap_xml),
        xml_semantics(&projection::ydoc_to_tiptap_xml(doc)),
    ) {
        (Ok(stored), Ok(expected)) if stored == expected => {}
        (Err(error), _) | (_, Err(error)) => {
            errors.push(format!("TipTap XML verification unsupported: {error}"))
        }
        _ => errors.push("TipTap XML differs from Y.Doc projection".into()),
    }
    match serde_json::to_value(&record.tree) {
        Ok(stored)
            if !stored.is_null()
                && tree_semantics(&stored) == tree_semantics(&projected.tree_json) => {}
        Ok(_) => errors.push("tree missing or differs from Y.Doc projection".into()),
        Err(error) => errors.push(format!("tree verification failed: {error}")),
    }
    match serde_json::to_value(&record.blocks) {
        // Do not deserialize the projection to BlockSnapshot: that silently drops
        // non-null fields (e.g. wireId) not represented by the persisted type.
        Ok(stored) => {
            if canonical_blocks(stored) != canonical_blocks(projected.blocks_json) {
                errors.push(
                    "blocks differ from Y.Doc projection (including any unrepresented mark attributes)"
                        .into(),
                );
            }
        }
        Err(error) => errors.push(format!("blocks verification failed: {error}")),
    }
    errors
}

fn sort_values(values: &mut [Value]) {
    // serde_json has preserve_order enabled; sort object keys before using
    // serialization as a set-order key, including nested attribute objects.
    values.sort_by_key(|value| canonical_keys(value).to_string());
}

fn canonical_keys(value: &Value) -> Value {
    match value {
        Value::Object(map) => map
            .iter()
            .map(|(key, value)| (key.clone(), canonical_keys(value)))
            .collect::<BTreeMap<_, _>>()
            .into_iter()
            .collect(),
        Value::Array(values) => values.iter().map(canonical_keys).collect(),
        _ => value.clone(),
    }
}

fn canonical_tiptap(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut result: serde_json::Map<String, Value> = map
                .iter()
                .map(|(key, value)| (key.clone(), canonical_tiptap(value)))
                .collect();
            if map.contains_key("type") {
                if let Some(Value::Array(marks)) = result.get_mut("marks") {
                    sort_values(marks);
                }
            }
            Value::Object(result)
        }
        Value::Array(values) => values.iter().map(canonical_tiptap).collect(),
        _ => value.clone(),
    }
}

// Snapshot Option fields serialize as null, while projection JSON sometimes
// omits them. Numeric snapshot order is f64, projection order is an integer.
fn canonical_snapshot(value: &Value) -> Value {
    match value {
        Value::Object(map) => map
            .iter()
            .filter(|(_, value)| !value.is_null())
            .map(|(key, value)| (key.clone(), canonical_snapshot(value)))
            .collect(),
        Value::Array(values) => values.iter().map(canonical_snapshot).collect(),
        Value::Number(number) if number.is_f64() => {
            let n = number.as_f64().unwrap();
            // Only normalize exactly representable integers, never round a
            // large integer snapshot through f64 and conceal a difference.
            if n.fract() == 0.0 && n.abs() <= 9_007_199_254_740_991.0 {
                json!(n as i64)
            } else {
                value.clone()
            }
        }
        _ => value.clone(),
    }
}

fn canonical_blocks(mut value: Value) -> Value {
    if let Some(blocks) = value.as_array_mut() {
        for block in blocks {
            if let Some(marks) = block.get_mut("marks").and_then(Value::as_array_mut) {
                for mark in marks.iter_mut() {
                    if let Some(object) = mark.as_object_mut() {
                        object.remove("id"); // Generated afresh on every projection.
                    }
                    *mark = canonical_snapshot(mark);
                }
                sort_values(marks);
            }
        }
    }
    canonical_snapshot(&value)
}

fn tree_semantics(value: &Value) -> Value {
    json!({"docId": value.get("docId"), "root": tree_nodes(&value["root"], &[])})
}

// Inline wrappers are sets of marks, not ordered structural containers.
// Keep their complete descriptors on each child; block nesting stays ordered.
fn tree_nodes(value: &Value, inherited: &[Value]) -> Vec<Value> {
    let mut descriptor = value.as_object().cloned().unwrap_or_default();
    let children = descriptor.remove("children").unwrap_or_else(|| json!([]));
    let mut marks = inherited.to_vec();
    let is_mark = value["kind"] == "mark";
    if is_mark {
        marks.push(canonical_snapshot(&Value::Object(descriptor.clone())));
    }
    sort_values(&mut marks);
    let children: Vec<Value> = children
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|child| tree_nodes(child, if is_mark { &marks } else { &[] }))
        .collect();
    if is_mark && !children.is_empty() {
        children
    } else {
        vec![
            json!({"node": canonical_snapshot(&Value::Object(descriptor)), "marks": marks, "children": children}),
        ]
    }
}

fn xml_semantics(xml: &str) -> Result<Value, String> {
    let wrapped = format!("<evidence-root>{xml}</evidence-root>");
    let document = roxmltree::Document::parse(&wrapped).map_err(|error| error.to_string())?;
    Ok(Value::Array(xml_nodes(document.root_element(), &[])?))
}

fn xml_nodes(node: roxmltree::Node<'_, '_>, inherited: &[Value]) -> Result<Vec<Value>, String> {
    if node.is_text() {
        return Ok(vec![
            json!({"text": node.text().unwrap_or_default(), "marks": inherited}),
        ]);
    }
    if !node.is_element() {
        return Err("comments or processing instructions are not verified".into());
    }
    let name = node.tag_name();
    let mut attributes: Vec<Value> = node
        .attributes()
        .map(|attr| json!([attr.namespace(), attr.name(), attr.value()]))
        .collect();
    sort_values(&mut attributes);
    let descriptor =
        json!({"namespace": name.namespace(), "tag": name.name(), "attributes": attributes});
    let is_mark = matches!(
        name.name(),
        "strong"
            | "bold"
            | "em"
            | "italic"
            | "s"
            | "strike"
            | "code"
            | "u"
            | "underline"
            | "a"
            | "link"
            | "mark"
            | "highlight"
            | "span"
            | "textStyle"
            | "commentMark"
            | "wireMark"
    );
    let mut marks = inherited.to_vec();
    if is_mark {
        marks.push(descriptor.clone());
    }
    sort_values(&mut marks);
    let mut children = Vec::new();
    for child in node.children() {
        children.extend(xml_nodes(child, if is_mark { &marks } else { &[] })?);
    }
    if is_mark && !children.is_empty() {
        Ok(children)
    } else {
        Ok(vec![
            json!({"node": descriptor, "marks": inherited, "children": children}),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crdt_engine::builder::ydoc_from_tiptap_json;

    fn record(doc: &yrs::Doc) -> DocumentRecord {
        let projection = projection::materialize_ydoc(doc, "document-a");
        serde_json::from_value(json!({
            "documentId": "document-a", "graphId": "graph-a", "title": "Evidence",
            "body": projection.body, "origin": "local", "providerId": "local",
            "localPath": "", "rdfSubject": "", "createdAt": "", "updatedAt": "",
            "capabilities": [], "tiptapXml": projection::ydoc_to_tiptap_xml(doc),
            "tiptapJson": projection.tiptap_json, "tree": projection.tree_json,
            "blocks": projection.blocks_json
        }))
        .unwrap()
    }

    fn fixture() -> yrs::Doc {
        ydoc_from_tiptap_json(&json!({"type": "doc", "content": [{
            "type": "paragraph", "attrs": {"data-block-id": "block-a"},
            "content": [{"type": "text", "text": "Evidence", "marks": [{"type": "bold"}, {"type": "italic"}]}]
        }]}))
    }

    #[test]
    fn missing_directory_is_not_created() {
        let path =
            std::env::temp_dir().join(format!("migration-evidence-{}", uuid::Uuid::new_v4()));
        assert!(read_records(&path, "graph-a").is_err());
        assert!(!path.exists());
        fs::create_dir(&path).unwrap();
        assert!(read_records(&path, "graph-a").is_err());
        assert_eq!(fs::read_dir(&path).unwrap().count(), 0);
        fs::remove_dir(path).unwrap();
    }

    #[test]
    fn misplaced_records_are_rejected_without_repair() {
        let path =
            std::env::temp_dir().join(format!("migration-evidence-{}", uuid::Uuid::new_v4()));
        let directory = path.join("documents/document-a");
        fs::create_dir_all(&directory).unwrap();
        let mut record = record(&fixture());
        let manifest = directory.join("document.json");
        let original = serde_json::to_vec(&record).unwrap();
        fs::write(&manifest, &original).unwrap();
        assert!(read_records(&path, "graph-b")
            .unwrap_err()
            .contains("graph identity mismatch"));
        assert_eq!(fs::read(&manifest).unwrap(), original);
        record.document_id = "document-b".into();
        fs::write(&manifest, serde_json::to_vec(&record).unwrap()).unwrap();
        assert!(read_records(&path, "graph-a")
            .unwrap_err()
            .contains("document identity mismatch"));
        assert!(!path.join("ydocs").exists());
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn projections_ignore_mark_order_but_detect_content_and_attribute_drift() {
        let doc = fixture();
        let stored = record(&doc);
        for _ in 0..5 {
            let independent = ydoc_from_tiptap_json(stored.tiptap_json.as_ref().unwrap());
            assert!(
                projection_errors(&stored, &independent).is_empty(),
                "{:?}",
                projection_errors(&stored, &independent)
            );
        }
        let mut changed = stored.clone();
        changed.body.push('!');
        changed.blocks[0].content.push('!');
        changed.tiptap_xml = changed.tiptap_xml.replace("Evidence", "Different");
        let errors = projection_errors(&changed, &doc);
        assert!(errors.iter().any(|error| error.starts_with("body")));
        assert!(errors.iter().any(|error| error.starts_with("blocks")));
        assert!(errors.iter().any(|error| error.starts_with("TipTap XML")));
        let mut changed_tree = stored.clone();
        changed_tree.tree.as_mut().unwrap().root.children[0]
            .attributes
            .extra
            .insert("unexpected".into(), "value".into());
        assert!(projection_errors(&changed_tree, &doc)
            .iter()
            .any(|error| error.starts_with("tree")));
        let mut changed_json = stored.clone();
        changed_json.tiptap_json.as_mut().unwrap()["content"][0]["attrs"]["extra"] = json!(true);
        assert!(projection_errors(&changed_json, &doc)
            .iter()
            .any(|error| error.starts_with("TipTap JSON")));
    }

    #[test]
    fn tombstones_skip_leftovers_and_invalid_tombstones_fail_closed() {
        let path =
            std::env::temp_dir().join(format!("migration-evidence-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(path.join("documents/document-a")).unwrap();
        let tombstones = path.join("ydocs/document-tombstones");
        fs::create_dir_all(&tombstones).unwrap();
        let tombstone = tombstones.join("document-a.json");
        let bytes = serde_json::to_vec(&json!({
            "schemaVersion": 1, "documentId": "document-a",
            "deletionId": uuid::Uuid::new_v4().to_string(), "deletedAt": ""
        }))
        .unwrap();
        fs::write(&tombstone, &bytes).unwrap();
        assert!(read_records(&path, "graph-a").unwrap().is_empty());
        assert_eq!(fs::read(&tombstone).unwrap(), bytes);
        assert!(!path.join("documents/document-a/document.json").exists());
        fs::write(&tombstone, b"invalid").unwrap();
        assert!(read_records(&path, "graph-a").is_err());
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn snapshot_mark_sets_preserve_unknown_non_null_fields() {
        let a = json!([{"id": "block-a", "order": 0.0, "marks": [
            {"id": "random-a", "type": "bold", "start": 0, "end": 1, "href": null},
            {"id": "random-b", "type": "italic", "start": 0, "end": 1}
        ]}]);
        let b = json!([{"id": "block-a", "order": 0, "marks": [
            {"end": 1, "start": 0, "type": "italic", "id": "different-a"},
            {"end": 1, "start": 0, "type": "bold", "id": "different-b"}
        ]}]);
        assert_eq!(canonical_blocks(a.clone()), canonical_blocks(b.clone()));
        let mut changed = b;
        changed[0]["marks"][0]["wireId"] = json!("unrepresented");
        assert_ne!(canonical_blocks(a), canonical_blocks(changed));
    }

    #[test]
    fn xml_mark_wrapper_order_is_semantic_but_attributes_and_text_are_not_ignored() {
        let a = "<paragraph><strong><em>x</em></strong></paragraph>";
        let b = "<paragraph><em><strong>x</strong></em></paragraph>";
        assert_eq!(xml_semantics(a).unwrap(), xml_semantics(b).unwrap());
        assert_ne!(
            xml_semantics(a).unwrap(),
            xml_semantics(&b.replace("<em>", "<em title=\"changed\"> ")).unwrap()
        );
        assert_ne!(
            xml_semantics(a).unwrap(),
            xml_semantics(&b.replace('x', "y")).unwrap()
        );
    }
}
