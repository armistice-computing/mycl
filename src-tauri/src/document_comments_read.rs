//! Comments for `read_document`, ported from the legacy server, whose
//! read_document returned every comment beside the content. Offline Garden
//! could write comments (edit_comment) but no tool read them back.
use crate::document_types::DocumentRecord;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

/// Every comment on the document, oldest first: its stored fields (text,
/// author, createdAt, resolved, ...) plus `id`, and, when its mark is in the
/// content, `quotedText` (the marked words) and `blockId`. A comment with no
/// mark in the content (never anchored, which edit_comment allows, or its
/// marked text deleted) comes back with `anchored: false`. When the content's
/// structure isn't available to check (tiptapJson absent, as right after a
/// time-travel restore), `anchored` is left out rather than guessed.
///
/// The Y.Doc is decoded only if its update names a "comments" root at all.
/// A comment stored as a non-JSON Y type is listed as `{id, unreadable: true}`
/// instead of failing the whole read.
pub(super) fn document_comments_for_read(document: &DocumentRecord) -> Result<Option<Value>, String> {
    let (stored, unreadable) =
        crate::crdt_engine::document_ops::comments_for_read_from_ydoc_base64(&document.ydoc_update_base64)?;
    if stored.is_empty() && unreadable.is_empty() {
        return Ok(None);
    }
    let structure_known = document.tiptap_json.is_some();
    let anchors = comment_anchors(document.tiptap_json.as_ref());
    let mut comments = stored
        .iter()
        .map(|(id, body)| {
            let mut entry = match body {
                Value::Object(fields) => fields.clone(),
                // Early writers stored a bare string as the comment body.
                other => {
                    let mut fields = Map::new();
                    fields.insert("text".into(), other.clone());
                    fields
                }
            };
            entry.insert("id".into(), json!(id));
            match anchors.get(id.as_str()) {
                Some(anchor) => {
                    entry.insert("quotedText".into(), json!(anchor.text));
                    entry.insert("blockId".into(), json!(anchor.block_id));
                }
                None if structure_known => {
                    entry.insert("anchored".into(), json!(false));
                }
                None => {}
            }
            Value::Object(entry)
        })
        .collect::<Vec<_>>();
    comments.sort_by(|left, right| {
        created_ms(left)
            .partial_cmp(&created_ms(right))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left["id"].as_str().cmp(&right["id"].as_str()))
    });
    comments.extend(unreadable.into_iter().map(|id| json!({"id": id, "unreadable": true})));
    Ok(Some(Value::Array(comments)))
}

fn created_ms(comment: &Value) -> f64 {
    comment
        .get("createdAt")
        .and_then(|value| value.as_f64().or_else(|| value.as_str().and_then(|raw| raw.parse().ok())))
        .unwrap_or(f64::MAX)
}

struct Anchor {
    text: String,
    block_id: Option<String>,
}

/// commentId → the text under its mark and the top-level block holding it.
fn comment_anchors(tiptap_json: Option<&Value>) -> BTreeMap<String, Anchor> {
    let mut anchors = BTreeMap::new();
    let Some(blocks) = tiptap_json.and_then(|doc| doc.get("content")).and_then(Value::as_array) else {
        return anchors;
    };
    for block in blocks {
        let block_id = block
            .pointer("/attrs/data-block-id")
            .or_else(|| block.pointer("/attrs/blockId"))
            .or_else(|| block.pointer("/attrs/id"))
            .and_then(Value::as_str)
            .map(str::to_string);
        collect_anchors(block, &block_id, &mut anchors);
    }
    anchors
}

fn collect_anchors(node: &Value, block_id: &Option<String>, anchors: &mut BTreeMap<String, Anchor>) {
    if let Some(text) = node.get("text").and_then(Value::as_str) {
        for mark in node.get("marks").and_then(Value::as_array).into_iter().flatten() {
            if let Some(id) = mark.pointer("/attrs/commentId").and_then(Value::as_str) {
                let anchor = anchors.entry(id.to_string()).or_insert_with(|| Anchor {
                    text: String::new(),
                    block_id: block_id.clone(),
                });
                anchor.text.push_str(text);
            }
        }
    }
    for child in node.get("content").and_then(Value::as_array).into_iter().flatten() {
        collect_anchors(child, block_id, anchors);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchors_collect_marked_text_per_comment_and_block() {
        let doc = json!({"type": "doc", "content": [
            {"type": "paragraph", "attrs": {"data-block-id": "block-a"}, "content": [
                {"type": "text", "text": "plain "},
                {"type": "text", "text": "marked", "marks": [{"type": "comment", "attrs": {"commentId": "c1"}}]},
                {"type": "text", "text": " words", "marks": [{"type": "bold"}, {"type": "comment", "attrs": {"commentId": "c1"}}]}
            ]},
            {"type": "bulletList", "content": [{"type": "listItem", "attrs": {"data-block-id": "block-b"}, "content": [
                {"type": "paragraph", "content": [
                    {"type": "text", "text": "nested", "marks": [{"type": "comment", "attrs": {"commentId": "c2"}}]}
                ]}
            ]}]}
        ]});
        let anchors = comment_anchors(Some(&doc));
        assert_eq!(anchors["c1"].text, "marked words");
        assert_eq!(anchors["c1"].block_id.as_deref(), Some("block-a"));
        assert_eq!(anchors["c2"].text, "nested");
        assert!(anchors.get("c3").is_none());
    }

    #[test]
    fn comments_come_back_with_bodies_quotes_and_orphans() {
        use base64::Engine;
        use yrs::{Doc, Map as _, ReadTxn, Transact, WriteTxn};
        let ydoc = Doc::new();
        {
            let mut txn = ydoc.transact_mut();
            let comments = txn.get_or_insert_map("comments");
            let later: yrs::Any = serde_json::from_value(json!({"text": "Cut this.", "author": "Eschaton", "createdAt": 2000, "resolved": false})).unwrap();
            let earlier: yrs::Any = serde_json::from_value(json!({"text": "Gone anchor", "author": "Eschaton", "createdAt": 1000, "resolved": true})).unwrap();
            comments.insert(&mut txn, "c1", later);
            comments.insert(&mut txn, "c9", earlier);
        }
        let update = ydoc.transact().encode_state_as_update_v1(&yrs::StateVector::default());
        let mut document: DocumentRecord = serde_json::from_value(json!({
            "documentId": "d", "graphId": "g", "title": "T", "body": "", "origin": "local",
            "providerId": "p", "localPath": "", "rdfSubject": "", "createdAt": "0", "updatedAt": "0",
            "capabilities": []
        })).unwrap();
        document.ydoc_update_base64 = base64::engine::general_purpose::STANDARD.encode(update);
        document.tiptap_json = Some(json!({"type": "doc", "content": [
            {"type": "paragraph", "attrs": {"data-block-id": "block-a"}, "content": [
                {"type": "text", "text": "this line", "marks": [{"type": "comment", "attrs": {"commentId": "c1"}}]}
            ]}
        ]}));
        let comments = document_comments_for_read(&document).unwrap().unwrap();
        let comments = comments.as_array().unwrap();
        assert_eq!(comments.len(), 2);
        assert_eq!(comments[0]["id"], "c9", "oldest first");
        assert_eq!(comments[0]["anchored"], false);
        assert_eq!(comments[0]["resolved"], true);
        assert_eq!(comments[1]["id"], "c1");
        assert_eq!(comments[1]["text"], "Cut this.");
        assert_eq!(comments[1]["quotedText"], "this line");
        assert_eq!(comments[1]["blockId"], "block-a");

        // No marks at all: comments still come back, as unanchored.
        document.tiptap_json = Some(json!({"type": "doc", "content": []}));
        let comments = document_comments_for_read(&document).unwrap().unwrap();
        assert!(comments.as_array().unwrap().iter().all(|c| c["anchored"] == false));

        // Structure unknown (restore clears tiptapJson): no anchored claim either way.
        document.tiptap_json = None;
        let comments = document_comments_for_read(&document).unwrap().unwrap();
        assert!(comments.as_array().unwrap().iter().all(|c| c.get("anchored").is_none()));
    }

    #[test]
    fn a_document_that_never_had_comments_skips_the_decode() {
        use base64::Engine;
        use yrs::{Doc, ReadTxn, Text, Transact, WriteTxn};
        let ydoc = Doc::new();
        {
            let mut txn = ydoc.transact_mut();
            let text = txn.get_or_insert_text("content");
            text.insert(&mut txn, 0, "just words");
        }
        let update = ydoc.transact().encode_state_as_update_v1(&yrs::StateVector::default());
        let encoded = base64::engine::general_purpose::STANDARD.encode(update);
        let (stored, unreadable) =
            crate::crdt_engine::document_ops::comments_for_read_from_ydoc_base64(&encoded).unwrap();
        assert!(stored.is_empty() && unreadable.is_empty());
    }

    #[test]
    fn one_odd_comment_value_does_not_fail_the_read() {
        use base64::Engine;
        use yrs::{Doc, Map as _, MapPrelim, ReadTxn, Transact, WriteTxn};
        let ydoc = Doc::new();
        {
            let mut txn = ydoc.transact_mut();
            let comments = txn.get_or_insert_map("comments");
            let good: yrs::Any = serde_json::from_value(json!({"text": "fine", "createdAt": 1})).unwrap();
            comments.insert(&mut txn, "c-good", good);
            comments.insert(&mut txn, "c-weird", MapPrelim::default());
        }
        let update = ydoc.transact().encode_state_as_update_v1(&yrs::StateVector::default());
        let mut document: DocumentRecord = serde_json::from_value(json!({
            "documentId": "d", "graphId": "g", "title": "T", "body": "", "origin": "local",
            "providerId": "p", "localPath": "", "rdfSubject": "", "createdAt": "0", "updatedAt": "0",
            "capabilities": []
        })).unwrap();
        document.ydoc_update_base64 = base64::engine::general_purpose::STANDARD.encode(update);
        let comments = document_comments_for_read(&document).unwrap().unwrap();
        let comments = comments.as_array().unwrap();
        assert_eq!(comments[0]["id"], "c-good");
        assert_eq!(comments[0]["text"], "fine");
        assert_eq!(comments[1], json!({"id": "c-weird", "unreadable": true}));
        // The strict decoder used for projection comparison still refuses it.
        assert!(crate::crdt_engine::document_ops::comments_from_ydoc_base64(&document.ydoc_update_base64).is_err());
    }
}
