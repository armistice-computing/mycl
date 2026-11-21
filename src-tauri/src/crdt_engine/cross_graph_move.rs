//! Conservative, receipt-backed document transfer. Call only through `apply`:
//! it owns both graph hot-write/lifecycle gates for the entire attempt.
use super::executor::{ApplyOperationError, ApplyOperationResult};
use super::{document_ops, rooms::RoomRegistry, workspace_ops};
use crate::{app_runtime::AppHandle, crdt_queue::CrdtOperation};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::Arc,
};
#[cfg(feature = "desktop")]
use tauri::Manager;
use yrs::{types::ToJson, updates::decoder::Decode};
use yrs::{
    Doc, Map, MapPrelim, Out, ReadTxn, StateVector, Transact, Update, WriteTxn, XmlFragment,
};

pub(super) const KIND: &str = "workspace.moveDocumentsAcrossGraphs";
const MARKER: &str = "gardenCrossGraphMove";
// Base64 source + destination states and JSON history all count toward this.
const MAX_RECEIPT_BYTES: usize = 256 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;

fn bounded_file(path: &Path) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
        return Err(format!(
            "cross-graph move file exceeds 32 MiB or is not regular: {}",
            path.display()
        ));
    }
    Ok(())
}
fn encoded_size(value: &impl Serialize) -> Result<usize, String> {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len());
            if self.0 > MAX_RECEIPT_BYTES {
                return Err(std::io::Error::other("cross-graph receipt exceeds 256 MiB"));
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, value).map_err(|e| e.to_string())?;
    Ok(counter.0)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Request {
    source: String,
    target: String,
    source_incarnation: String,
    target_incarnation: String,
    document_ids: Vec<String>,
    parent_id: Option<String>,
    order: f64,
}

impl Request {
    fn parse(op: &CrdtOperation) -> Result<Self, String> {
        let required = |key: &str| {
            op.payload
                .get(key)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| format!("cross-graph move requires {key}"))
        };
        let target = required("targetGraphId")?;
        for id in [&op.graph_id, &target] {
            crate::ids::validate_local_id(id, "graph_id")?;
        }
        if target == op.graph_id {
            return Err("cross-graph move requires distinct graphs".into());
        }
        let ids = op
            .payload
            .get("documentIds")
            .and_then(Value::as_array)
            .ok_or("cross-graph move requires documentIds array")?;
        if ids.len() > 100 {
            return Err("cross-graph move accepts at most 100 documents".into());
        }
        let mut document_ids = Vec::new();
        let mut seen = BTreeSet::new();
        for id in ids {
            let id = id.as_str().ok_or("documentIds must contain strings")?;
            crate::ids::validate_local_id(id, "document_id")?;
            if !seen.insert(id) {
                return Err(format!("duplicate documentId: {id}"));
            }
            document_ids.push(id.to_string());
        }
        if document_ids.is_empty() {
            return Err("empty cross-graph move".into());
        }
        let parent_id = match op.payload.get("parentId") {
            None | Some(Value::Null) => None,
            Some(Value::String(id)) => {
                crate::ids::validate_local_id(id, "parentId")?;
                Some(id.clone())
            }
            _ => return Err("parentId must be a folder ID or null".into()),
        };
        let order = op
            .payload
            .get("order")
            .and_then(Value::as_f64)
            .filter(|n| n.is_finite())
            .ok_or("cross-graph move requires finite order")?;
        Ok(Self {
            source: op.graph_id.clone(),
            target,
            source_incarnation: required(crate::crdt_queue::GRAPH_INCARNATION_PAYLOAD_KEY)?,
            target_incarnation: required("targetGraphIncarnation")?,
            document_ids,
            parent_id,
            order,
        })
    }
}

#[derive(Serialize, Deserialize)]
struct Receipt {
    version: u32,
    operation_id: String,
    request: Request,
    entries: Vec<Entry>,
    target_purged: bool,
    complete: bool,
}
#[derive(Serialize, Deserialize)]
struct Entry {
    id: String,
    source_state: String,
    target_state: String,
    source_row: Value,
    target_row: Value,
    restore: Value,
    // Source bytes are retained for comparison; target copies rewrite graphId.
    history: BTreeMap<String, Value>,
    history_copied: bool,
    ready: bool,
    deleting: bool,
    deleted: bool,
}

pub(super) async fn apply(app: &AppHandle, op: &CrdtOperation) -> ApplyOperationResult<Value> {
    let request = Request::parse(op)?;
    crate::ids::validate_local_id(&op.operation_id, "operation_id")?;
    let coordinator = app
        .try_state::<super::persistence_coordinator::GraphPersistenceCoordinator>()
        .ok_or_else(|| {
            "cross-graph move requires managed graph persistence coordinator".to_string()
        })?;
    let mut graphs = [&request.source, &request.target];
    graphs.sort();
    let first = coordinator.acquire_hot_write(graphs[0]).await?;
    let second = coordinator.acquire_hot_write(graphs[1]).await?;
    first.set_crdt_kind(KIND);
    second.set_crdt_kind(KIND);
    super::executor::require_current_graph_incarnation(app, op)?;
    let (_, target) = crate::graph_record_store::read_graph_record_no_heal(app, &request.target)
        .map_err(|e| e.to_string())?;
    if target.incarnation_id.as_deref() != Some(&request.target_incarnation) {
        return Err(ApplyOperationError::terminal(
            "stale target graph incarnation",
        ));
    }
    let path = crate::profile_paths::profile_dir(app)?
        .join("cross-graph-moves")
        .join(format!("{}.json", op.operation_id));
    let mut receipt = if path.exists() {
        if std::fs::metadata(&path).map_err(|e| e.to_string())?.len() > MAX_RECEIPT_BYTES as u64 {
            return Err(ApplyOperationError::terminal(
                "cross-graph receipt exceeds 256 MiB",
            ));
        }
        let receipt: Receipt = crate::storage::read_json(&path).map_err(|e| e.to_string())?;
        if receipt.version != 1
            || receipt.operation_id != op.operation_id
            || receipt.request != request
        {
            return Err(ApplyOperationError::terminal(
                "cross-graph receipt identity mismatch",
            ));
        }
        if !receipt.complete
            && receipt.entries.iter().map(|e| &e.id).collect::<Vec<_>>()
                != receipt.request.document_ids.iter().collect::<Vec<_>>()
        {
            return Err(ApplyOperationError::terminal(
                "cross-graph receipt batch mismatch",
            ));
        }
        receipt
    } else {
        let entries = preflight(app, op, &request).await?;
        let receipt = Receipt {
            version: 1,
            operation_id: op.operation_id.clone(),
            request,
            entries,
            target_purged: false,
            complete: false,
        };
        save_new(&path, &receipt)?;
        receipt
    };
    if receipt.complete {
        return Ok(result(&receipt));
    }
    // From here an error retains the receipt and the queued operation. No retry
    // ever blindly replays captured bytes over an existing target authority.
    resume(app, op, &path, &mut receipt).await
        .map_err(|error| ApplyOperationError::retryable_after_hot_commit(format!(
            "cross-graph transfer {} interrupted; receipt retained (source deletion may have progressed): {error}",op.operation_id)))?;
    Ok(result(&receipt))
}

fn result(r: &Receipt) -> Value {
    json!({"operationId":r.operation_id,"moved":r.request.document_ids,
        "movedCount":r.request.document_ids.len(),"missing":[],"parentId":r.request.parent_id,
        "documentIds":r.request.document_ids,
        "sourceGraphId":r.request.source,"targetGraphId":r.request.target,
        "droppedWires":true,"droppedValuations":true,"historyPreserved":true})
}
fn save(path: &Path, r: &Receipt) -> Result<(), String> {
    encoded_size(r)?;
    // Compact encoding keeps the serialized bound the same at read and write.
    crate::storage::write_bytes(path, &serde_json::to_vec(r).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())
}
fn save_new(path: &Path, r: &Receipt) -> Result<(), String> {
    let temporary = path.with_extension(format!("{}.prepared", uuid::Uuid::new_v4()));
    save(&temporary, r)?;
    let installed = std::fs::hard_link(&temporary, path)
        .map_err(|e| format!("install exclusive move receipt: {e}"));
    let _ = std::fs::remove_file(&temporary);
    installed?;
    crate::storage_atomic::sync_parent_dir(path.parent().ok_or("receipt parent missing")?)
}
fn encode(doc: &Doc) -> String {
    STANDARD.encode(
        doc.transact()
            .encode_state_as_update_v1(&StateVector::default()),
    )
}
fn decode(state: &str) -> Result<Update, String> {
    Update::decode_v1(&STANDARD.decode(state).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())
}
fn state_matches(actual: &Doc, expected: &str, id: &str) -> Result<bool, String> {
    let actual_state = encode(actual);
    let a = decode(&actual_state)?;
    let b = decode(expected)?;
    // Empty client entries can differ on hydration. Nonzero clocks and the
    // delete set still fence edits (including an edit followed by an undo).
    let clocks = |update: &Update| {
        update
            .state_vector()
            .iter()
            .filter(|(_, clock)| **clock > 0)
            .map(|(client, clock)| (*client, *clock))
            .collect::<BTreeMap<_, _>>()
    };
    if clocks(&a) != clocks(&b) || a.delete_set() != b.delete_set() {
        return Ok(false);
    }
    let expected_doc = Doc::new();
    expected_doc
        .transact_mut()
        .apply_update(b)
        .map_err(|e| e.to_string())?;
    let marker = |doc: &Doc| {
        let txn = doc.transact();
        txn.get_map(MARKER).map(|map| map.to_json(&txn))
    };
    Ok(super::projection::materialize_ydoc(actual, id).tiptap_json
        == super::projection::materialize_ydoc(&expected_doc, id).tiptap_json
        && document_ops::comments_from_ydoc_base64(&actual_state)?
            == document_ops::comments_from_ydoc_base64(expected)?
        && marker(actual) == marker(&expected_doc))
}
fn prepared_state(source: &str, operation_id: &str, document_id: &str) -> Result<String, String> {
    let doc = Doc::new();
    {
        let mut txn = doc.transact_mut();
        txn.apply_update(decode(source)?)
            .map_err(|e| e.to_string())?;
        txn.get_or_insert_map(MARKER)
            .insert(&mut txn, "operationId", operation_id);
    }
    let original = super::projection::materialize_ydoc(&doc, document_id).tiptap_json;
    let mut sanitized = original.clone();
    strip_wires(&mut sanitized)?;
    if sanitized != original {
        let mut txn = doc.transact_mut();
        let content = txn.get_or_insert_xml_fragment("content");
        let len = content.len(&txn);
        if len > 0 {
            content.remove_range(&mut txn, 0, len);
        }
        super::builder::append_nodes(
            &mut txn,
            &content,
            sanitized["content"]
                .as_array()
                .ok_or("invalid document content")?,
        );
        drop(txn);
        if super::projection::materialize_ydoc(&doc, document_id).tiptap_json != sanitized {
            return Err("cannot remove wire marks without changing document content".into());
        }
    }
    Ok(encode(&doc))
}
async fn room(
    app: &AppHandle,
    graph: &str,
    dir: &Path,
    id: &str,
) -> Result<Arc<super::rooms::Room>, String> {
    app.state::<RoomRegistry>()
        .get_or_create(
            &format!("doc:{graph}:{id}"),
            crate::ydoc_paths::checked_document_ydoc_state_path(dir, id)?,
        )
        .await
}
async fn row(app: &AppHandle, graph: &str, dir: &Path, id: &str) -> Result<Option<Value>, String> {
    let room = workspace_ops::workspace_room(app, graph, dir).await?;
    room.with_doc(|doc| {
        let txn = doc.transact();
        match txn.get_map("documents").and_then(|m| m.get(&txn, id)) {
            Some(Out::YMap(m)) => serde_json::to_value(m.to_json(&txn))
                .map(Some)
                .map_err(|e| e.to_string()),
            Some(_) => Err(format!("unsupported workspace document row: {id}")),
            None => Ok(None),
        }
    })
    .await
}
async fn validate_target_parent(app: &AppHandle, r: &Request, dir: &Path) -> Result<(), String> {
    if let Some(id) = &r.parent_id {
        let room = workspace_ops::workspace_room(app, &r.target, dir).await?;
        let valid = room
            .with_doc(|doc| {
                let txn = doc.transact();
                matches!(
                    txn.get_map("folders").and_then(|m| m.get(&txn, id)),
                    Some(Out::YMap(_))
                )
            })
            .await;
        if !valid {
            return Err(format!("target folder missing: {id}"));
        }
    }
    Ok(())
}
async fn destination_absent(
    app: &AppHandle,
    graph: &str,
    dir: &Path,
    id: &str,
) -> Result<(), String> {
    crate::geist_derived_view::require_editable(dir, id)?;
    crate::document_tombstone_store::require_document_not_tombstoned(dir, id)?;
    let ws = workspace_ops::workspace_room(app, graph, dir).await?;
    let occupied = ws
        .with_doc(|doc| {
            let txn = doc.transact();
            ["documents", "artifacts", "folders"]
                .iter()
                .any(|name| txn.get_map(*name).is_some_and(|m| m.contains_key(&txn, id)))
        })
        .await;
    let snapshot = ws
        .with_doc(|doc| workspace_ops::materialize_workspace_snapshot_json(graph, doc))
        .await?;
    let dangling_wire = snapshot["wires"].as_array().is_some_and(|wires| {
        wires.iter().any(|wire| {
            wire["sourceDocumentId"].as_str() == Some(id)
                || (wire["targetDocumentId"].as_str() == Some(id)
                    && wire["targetGraphId"]
                        .as_str()
                        .is_none_or(|g| g.is_empty() || g == graph))
        })
    });
    if occupied
        || dangling_wire
        || crate::document_paths::document_dir(dir, id)?.exists()
        || crate::ydoc_paths::document_ydoc_dir(dir, id).exists()
        || dir.join("artifacts").join(id).exists()
        || dir.join("images").join(id).exists()
        || app
            .state::<RoomRegistry>()
            .peek(&format!("doc:{graph}:{id}"))
            .await
            .is_some()
    {
        return Err(format!("destination collision: {graph}/{id}"));
    }
    Ok(())
}

async fn preflight(app: &AppHandle, op: &CrdtOperation, r: &Request) -> Result<Vec<Entry>, String> {
    let source = crate::graph_paths::existing_graph_dir(app, &r.source)?;
    let target = crate::graph_paths::existing_graph_dir(app, &r.target)?;
    validate_target_parent(app, r, &target).await?;
    let mut entries = Vec::new();
    for (index, id) in r.document_ids.iter().enumerate() {
        destination_absent(app, &r.target, &target, id).await?;
        crate::geist_derived_view::require_editable(&source, id)?;
        crate::document_tombstone_store::require_document_not_tombstoned(&source, id)?;
        let source_row = row(app, &r.source, &source, id)
            .await?
            .ok_or_else(|| format!("source document missing: {id}"))?;
        reject_assets(&source_row)?;
        let manifest = crate::document_paths::document_dir(&source, id)?.join("document.json");
        bounded_file(&manifest)?;
        let state_path = crate::ydoc_paths::document_ydoc_state_path(&source, id);
        if state_path.exists() {
            bounded_file(&state_path)?;
        }
        let record = crate::document_record_store::read_document_record_cold(&source, &manifest)?;
        if record.document_id != *id || record.graph_id != r.source {
            return Err("source manifest identity mismatch".into());
        }
        let history = capture_history(&source, id)?;
        let source_room = room(app, &r.source, &source, id).await?;
        let source_state = source_room
            .with_doc(|doc| -> Result<String, String> {
                let txn = doc.transact();
                for (name, _) in txn.root_refs() {
                    if !["content", "comments", MARKER].contains(&name) {
                        return Err(format!("unsupported document Y.Doc root: {name}"));
                    }
                }
                drop(txn);
                reject_assets(&super::projection::materialize_ydoc(doc, id).tiptap_json)?;
                Ok(encode(doc))
            })
            .await?;
        // Refuse unreadable comments rather than trusting a lossy projection.
        document_ops::comments_from_ydoc_base64(&source_state)?;
        let mut target_row = source_row.clone();
        target_row["parentId"] = json!(r.parent_id);
        target_row["order"] = json!(r.order + index as f64);
        // Yrs serializes integral JS numbers as JSON integers. Canonicalize
        // before freezing the receipt, not by weakening replay comparison.
        let target_row = serde_json::to_value(super::builder::json_to_any(&target_row))
            .map_err(|e| e.to_string())?;
        let restore = crate::document_chronicle::ChronicleRestore::from_entry(
            &crate::document_chronicle::ChronicleEntry::from_record(&record),
        );
        entries.push(Entry {
            id: id.clone(),
            target_state: prepared_state(&source_state, &op.operation_id, id)?,
            source_state,
            source_row,
            target_row,
            restore: serde_json::to_value(restore).map_err(|e| e.to_string())?,
            history,
            history_copied: false,
            ready: false,
            deleting: false,
            deleted: false,
        });
        encoded_size(&entries)?;
    }
    Ok(entries)
}

fn reject_assets(value: &Value) -> Result<(), String> {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if (key.starts_with("sf_")
                    || ["sourceFile", "storageKey", "assetId", "artifactId"]
                        .contains(&key.as_str()))
                    && !value.is_null()
                    && value != ""
                {
                    return Err(format!(
                        "cross-graph move does not support local assets ({key})"
                    ));
                }
                // Hyperlinks (href/url) are references, not copied assets. They
                // may stop resolving after a move, just as after deletion.
                if key == "src" {
                    if let Some(url) = value.as_str().filter(|s| !s.is_empty()) {
                        if !(url.starts_with("https://")
                            || url.starts_with("http://")
                            || url.starts_with("mailto:")
                            || url.starts_with('#'))
                            || url.contains("/api/")
                            || url.contains("localhost")
                            || url.contains("127.0.0.1")
                        {
                            return Err(format!(
                                "cross-graph move does not support local asset/link: {url}"
                            ));
                        }
                    }
                }
                reject_assets(value)?;
            }
        }
        Value::Array(values) => {
            for value in values {
                reject_assets(value)?;
            }
        }
        _ => (),
    }
    Ok(())
}

fn strip_wires(value: &mut Value) -> Result<(), String> {
    match value {
        Value::Object(map) => {
            for key in ["wireId", "data-wire-id"] {
                map.remove(key);
            }
            if let Some(marks) = map.get_mut("marks").and_then(Value::as_array_mut) {
                marks.retain(|mark| {
                    !matches!(
                        mark.get("type").and_then(Value::as_str),
                        Some("wire" | "wireMark")
                    )
                });
            }
            if let Some(xml) = map.get_mut("tiptapXml") {
                if let Some(text) = xml.as_str() {
                    *xml = json!(strip_wire_xml(text)?);
                }
            }
            for value in map.values_mut() {
                strip_wires(value)?;
            }
        }
        Value::Array(values) => {
            for value in values {
                strip_wires(value)?;
            }
        }
        _ => (),
    }
    Ok(())
}

fn strip_wire_xml(xml: &str) -> Result<String, String> {
    // Parser ranges let us remove markup, not text that happens to mention a
    // wire. Preserve all unrelated XML bytes, including comments and marks.
    let wrapped = format!("<root>{xml}</root>");
    let doc = roxmltree::Document::parse(&wrapped)
        .map_err(|e| format!("unsupported history XML: {e}"))?;
    let mut removals = Vec::new();
    for node in doc.descendants().filter(|n| n.is_element()) {
        for attr in node.attributes().filter(|a| a.name() == "src") {
            reject_assets(&json!({attr.name():attr.value()}))?;
        }
        let range = node.range();
        if matches!(node.tag_name().name(), "wireMark" | "wire") {
            let mut quoted = None;
            let mut end = None;
            for (offset, ch) in wrapped[range.clone()].char_indices() {
                match (quoted, ch) {
                    (Some(q), c) if c == q => quoted = None,
                    (None, '\'' | '"') => quoted = Some(ch),
                    (None, '>') => {
                        end = Some(range.start + offset + 1);
                        break;
                    }
                    _ => (),
                }
            }
            let end = end.ok_or("invalid wire XML tag")?;
            removals.push(range.start..end);
            if !wrapped[range.start..end].ends_with("/>") {
                let close = wrapped[range.clone()]
                    .rfind("</")
                    .ok_or("invalid wire XML closing tag")?;
                removals.push(range.start + close..range.end);
            }
        } else {
            for attr in node
                .attributes()
                .filter(|a| matches!(a.name(), "wireId" | "data-wire-id"))
            {
                removals.push(attr.range());
            }
        }
    }
    removals.sort_by_key(|r| r.start);
    let mut out = String::new();
    let mut cursor = 6;
    for range in removals {
        if range.start >= cursor {
            out.push_str(&wrapped[cursor..range.start]);
        }
        cursor = cursor.max(range.end);
    }
    out.push_str(&wrapped[cursor..wrapped.len() - 7]);
    Ok(out)
}

// Only known document projection files and typed history files may be discarded
// or relocated. Originals, attachments, and unknown sidecars fail before writes.
fn capture_history(dir: &Path, id: &str) -> Result<BTreeMap<String, Value>, String> {
    let root = crate::document_paths::document_dir(dir, id)?;
    let mut files = BTreeMap::new();
    fn walk(root: &Path, path: &Path, files: &mut BTreeMap<String, Value>) -> Result<(), String> {
        for item in std::fs::read_dir(path).map_err(|e| e.to_string())? {
            let item = item.map_err(|e| e.to_string())?;
            let path = item.path();
            let relative = path
                .strip_prefix(root)
                .map_err(|e| e.to_string())?
                .to_str()
                .ok_or("non-UTF8 sidecar")?
                .replace('\\', "/");
            let ty = item.file_type().map_err(|e| e.to_string())?;
            if ty.is_file() && relative == ".incarnation-id" {
                bounded_file(&path)?;
                let identity = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
                uuid::Uuid::parse_str(identity.trim())
                    .map_err(|e| format!("invalid document incarnation: {e}"))?;
                // A new graph-local lifetime is minted at the destination.
                continue;
            }
            if ty.is_dir() && ["history", "history/snapshots"].contains(&relative.as_str()) {
                walk(root, &path, files)?;
            } else if ty.is_file()
                && [
                    "document.json",
                    "tiptap.json",
                    "tiptap.xml",
                    "tree.json",
                    "blocks.json",
                    "history/tail-commit.json",
                ]
                .contains(&relative.as_str())
            {
                continue;
            } else if ty.is_file()
                && (relative == "history/history.json"
                    || (relative.starts_with("history/snapshots/") && relative.ends_with(".json")))
            {
                bounded_file(&path)?;
                files.insert(
                    relative,
                    crate::storage::read_json(&path).map_err(|e| e.to_string())?,
                );
                encoded_size(files)?;
            } else {
                return Err(format!("unsupported document sidecar: {relative}"));
            }
        }
        Ok(())
    }
    walk(&root, &root, &mut files)?;
    for (path, value) in &files {
        if value.get("documentId").and_then(Value::as_str) != Some(id) {
            return Err(format!("history identity mismatch: {path}"));
        }
        if path == "history/history.json" {
            let store: crate::document_history_store::LocalDocumentHistoryStore =
                serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
            for snapshot in store.snapshots {
                crate::ids::validate_local_id(&snapshot.snapshot_id, "snapshot_id")?;
                if snapshot.document_id != id
                    || !files
                        .contains_key(&format!("history/snapshots/{}.json", snapshot.snapshot_id))
                {
                    return Err(format!(
                        "missing or mismatched history snapshot: {}",
                        snapshot.snapshot_id
                    ));
                }
            }
        } else {
            let _: crate::document_history_store::LocalDocumentSnapshotPayload =
                serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
            reject_assets(value)?;
            target_history(value, "preflight")?;
        }
    }
    Ok(files)
}
fn target_history(value: &Value, target: &str) -> Result<Value, String> {
    let mut value = value.clone();
    value["graphId"] = json!(target);
    if let Some(snapshots) = value.get_mut("snapshots").and_then(Value::as_array_mut) {
        for snapshot in snapshots {
            snapshot["graphId"] = json!(target);
        }
    }
    strip_wires(&mut value)?;
    Ok(value)
}

fn copy_history(target: &Path, graph: &str, entry: &Entry) -> Result<(), String> {
    let root = crate::document_paths::document_dir(target, &entry.id)?;
    // Install immutable payloads before the index. Never replace a payload
    // already present: a mismatch is a collision, including on recovery.
    for (relative, value) in &entry.history {
        if relative == "history/history.json" {
            continue;
        }
        let path = root.join(relative);
        let expected = target_history(value, graph)?;
        if path.exists() {
            let actual: Value = crate::storage::read_json(&path).map_err(|e| e.to_string())?;
            if actual != expected {
                return Err(format!("target history collision: {relative}"));
            }
        } else {
            crate::storage::write_json(&path, &expected).map_err(|e| e.to_string())?;
        }
    }
    if let Some(source_index) = entry.history.get("history/history.json") {
        let path = root.join("history/history.json");
        let mut current: Value = crate::storage::read_json(&path).map_err(|e| e.to_string())?;
        let source_index = target_history(source_index, graph)?;
        let snapshots = current
            .get_mut("snapshots")
            .and_then(Value::as_array_mut)
            .ok_or("invalid target history index")?;
        for item in source_index["snapshots"]
            .as_array()
            .ok_or("invalid source history index")?
        {
            match snapshots
                .iter()
                .find(|v| v["snapshotId"] == item["snapshotId"])
            {
                Some(existing) if existing != item => {
                    return Err("target history metadata collision".into())
                }
                Some(_) => (),
                None => snapshots.push(item.clone()),
            }
        }
        // The history service takes the last automatic entry as newest. Keep
        // the imported sequence before the target's new current snapshot;
        // appending old entries would make the next edit roll an old snapshot.
        let source_order: BTreeMap<&str, usize> = source_index["snapshots"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
            .filter_map(|(index, value)| value["snapshotId"].as_str().map(|id| (id, index)))
            .collect();
        snapshots.sort_by_key(|value| {
            source_order
                .get(value["snapshotId"].as_str().unwrap_or(""))
                .copied()
                .unwrap_or(usize::MAX)
        });
        current["totalCount"] = json!(current["totalCount"]
            .as_u64()
            .unwrap_or(0)
            .max(source_index["totalCount"].as_u64().unwrap_or(0))
            .max(current["snapshots"].as_array().unwrap().len() as u64));
        crate::storage::write_json(&path, &current).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn verify_history(target: &Path, graph: &str, entry: &Entry) -> Result<(), String> {
    let root = crate::document_paths::document_dir(target, &entry.id)?;
    for (relative, value) in &entry.history {
        let actual: Value =
            crate::storage::read_json(&root.join(relative)).map_err(|e| e.to_string())?;
        let expected = target_history(value, graph)?;
        if relative == "history/history.json" {
            let snapshots = actual["snapshots"]
                .as_array()
                .ok_or("target history index missing")?;
            if !expected["snapshots"]
                .as_array()
                .ok_or("source history index missing")?
                .iter()
                .all(|v| snapshots.contains(v))
            {
                return Err("copied target history changed".into());
            }
        } else if actual != expected {
            return Err(format!("copied target snapshot changed: {relative}"));
        }
    }
    Ok(())
}

async fn verify_source(
    app: &AppHandle,
    r: &Request,
    source: &Path,
    entry: &Entry,
    op: &str,
) -> Result<(), String> {
    if let Some(tombstone) =
        crate::document_tombstone_store::read_document_tombstone(source, &entry.id)?
    {
        return if entry.deleting && tombstone.deletion_operation_id.as_deref() == Some(op) {
            Ok(())
        } else {
            Err(format!("source deletion boundary changed: {}", entry.id))
        };
    }
    if entry.deleted {
        return Err(format!("source was recreated: {}", entry.id));
    }
    let current_row = row(app, &r.source, source, &entry.id).await?;
    if !(current_row.as_ref() == Some(&entry.source_row)
        || (entry.deleting && current_row.is_none()))
        || !room(app, &r.source, source, &entry.id)
            .await?
            .with_doc(|doc| state_matches(doc, &entry.source_state, &entry.id))
            .await?
        || capture_history(source, &entry.id)? != entry.history
    {
        return Err(format!(
            "source changed since cross-graph preflight: {}",
            entry.id
        ));
    }
    Ok(())
}
async fn verify_target(
    app: &AppHandle,
    r: &Request,
    target: &Path,
    entry: &Entry,
) -> Result<Arc<super::rooms::Room>, String> {
    crate::document_tombstone_store::require_document_not_tombstoned(target, &entry.id)?;
    let state_path = crate::ydoc_paths::document_ydoc_state_path(target, &entry.id);
    if !state_path.is_file() {
        return Err(format!("target authority missing: {}", entry.id));
    }
    let room = room(app, &r.target, target, &entry.id).await?;
    if !room
        .with_doc(|doc| state_matches(doc, &entry.target_state, &entry.id))
        .await?
    {
        return Err(format!(
            "target changed or is not owned by this move: {}",
            entry.id
        ));
    }
    let current_row = row(app, &r.target, target, &entry.id).await?;
    if (entry.ready && current_row.is_none())
        || current_row.as_ref().is_some_and(|v| v != &entry.target_row)
    {
        return Err(format!("target workspace entry changed: {}", entry.id));
    }
    Ok(room)
}

async fn publish_row(
    app: &AppHandle,
    graph: &str,
    dir: &Path,
    entry: &Entry,
    op: &str,
) -> Result<(), String> {
    let workspace = workspace_ops::workspace_room(app, graph, dir).await?;
    workspace
        .update_doc(|_, txn| {
            let documents = txn.get_or_insert_map("documents");
            if documents.contains_key(txn, &entry.id) {
                return Err(format!("target workspace collision: {}", entry.id));
            }
            let child = documents.insert(txn, entry.id.as_str(), MapPrelim::default());
            for (key, value) in entry.target_row.as_object().ok_or("invalid target row")? {
                child.insert(txn, key.as_str(), super::builder::json_to_any(value));
            }
            Ok(())
        })
        .await?;
    workspace_ops::persist_workspace(app, graph, dir, &workspace, op)
        .await
        .map(|_| ())
}

async fn resume(
    app: &AppHandle,
    op: &CrdtOperation,
    path: &Path,
    receipt: &mut Receipt,
) -> Result<(), String> {
    #[cfg(test)]
    tests::fail_if_requested("prepared")?;
    let r = receipt.request.clone();
    let source = crate::graph_paths::existing_graph_dir(app, &r.source)?;
    let target = crate::graph_paths::existing_graph_dir(app, &r.target)?;
    validate_target_parent(app, &r, &target).await?;
    for entry in &receipt.entries {
        verify_source(app, &r, &source, entry, &op.operation_id).await?;
    }
    for entry in &receipt.entries {
        if crate::ydoc_paths::document_ydoc_state_path(&target, &entry.id).exists() {
            verify_target(app, &r, &target, entry).await?;
        } else {
            destination_absent(app, &r.target, &target, &entry.id).await?;
        }
    }
    // Also remove stale attention in the target. Tombstoned target identities
    // are conservatively collisions, but old value records can outlive them.
    if !receipt.target_purged {
        // No document is published before this stage is durably acknowledged.
        crate::salience_value_store::purge_document_values_for_operation(
            app,
            &r.target,
            &r.document_ids,
            &op.operation_id,
        )?;
        receipt.target_purged = true;
        save(path, receipt)?;
    }
    for index in 0..receipt.entries.len() {
        let entry = &receipt.entries[index];
        let state_path = crate::ydoc_paths::document_ydoc_state_path(&target, &entry.id);
        if !state_path.exists() {
            if entry.ready || entry.history_copied {
                return Err("published target disappeared".into());
            }
            destination_absent(app, &r.target, &target, &entry.id).await?;
            let room = room(app, &r.target, &target, &entry.id).await?;
            room.update_doc(|_, txn| {
                txn.apply_update(decode(&entry.target_state)?)
                    .map_err(|e| e.to_string())
            })
            .await?;
        }
        let target_room = verify_target(app, &r, &target, entry).await?;
        if !entry.ready {
            let title = entry
                .target_row
                .get("title")
                .and_then(Value::as_str)
                .ok_or("source title missing")?;
            document_ops::persist_room_document_as(
                app,
                &r.target,
                &entry.id,
                title,
                &target_room,
                &op.operation_id,
                &crate::document_chronicle::WriteHints {
                    restore: Some(entry.restore.clone()),
                    ..Default::default()
                },
            )
            .await?;
            copy_history(&target, &r.target, entry)?;
            verify_history(&target, &r.target, entry)?;
            crate::document_incarnation_store::ensure_document_incarnation_id(&target, &entry.id)?;
            match row(app, &r.target, &target, &entry.id).await? {
                None => publish_row(app, &r.target, &target, entry, &op.operation_id).await?,
                Some(existing) if existing == entry.target_row => {
                    let ws = workspace_ops::workspace_room(app, &r.target, &target).await?;
                    workspace_ops::persist_workspace(
                        app,
                        &r.target,
                        &target,
                        &ws,
                        &op.operation_id,
                    )
                    .await?;
                }
                Some(_) => return Err(format!("target workspace collision: {}", entry.id)),
            }
            #[cfg(test)]
            tests::fail_if_requested("published")?;
            receipt.entries[index].ready = true;
            receipt.entries[index].history_copied = true;
            save(path, receipt)?;
        }
    }
    // Verify the whole destination batch before the first destructive action.
    for entry in &receipt.entries {
        verify_target(app, &r, &target, entry).await?;
        verify_history(&target, &r.target, entry)?;
    }
    for index in 0..receipt.entries.len() {
        if receipt.entries[index].deleted {
            continue;
        }
        // A durable intent is necessary to distinguish our tombstone from a
        // later delete/recreate on recovery. Recheck before recording intent.
        verify_source(app, &r, &source, &receipt.entries[index], &op.operation_id).await?;
        receipt.entries[index].deleting = true;
        save(path, receipt)?;
        #[cfg(test)]
        tests::fail_if_requested("deleting")?;
        let id = receipt.entries[index].id.clone();
        // No recursive enqueue: use the workspace operation under both leases.
        let delete = CrdtOperation {
            operation_id: op.operation_id.clone(),
            kind: "workspace.deleteDocument".into(),
            graph_id: r.source.clone(),
            document_id: Some(id),
            payload: json!({}),
            enqueue_timestamp: op.enqueue_timestamp.clone(),
        };
        workspace_ops::apply_classified(app, &delete)
            .await
            .map_err(|e| e.to_string())?;
        #[cfg(test)]
        tests::fail_if_requested("deleted")?;
        receipt.entries[index].deleted = true;
        save(path, receipt)?;
    }
    workspace_ops::remove_wires_where(app, &r.source, &op.operation_id, |wire| {
        let moved = |key| {
            wire.get(key)
                .and_then(Value::as_str)
                .is_some_and(|id| r.document_ids.iter().any(|m| m == id))
        };
        moved("sourceDocumentId")
            || (moved("targetDocumentId")
                && wire
                    .get("targetGraphId")
                    .and_then(Value::as_str)
                    .is_none_or(|graph| graph.is_empty() || graph == r.source))
    })
    .await?;
    // remove_wires_where is a no-op once its hot delete committed. Its retry
    // still owes a projection flush if the previous attempt lost the tail.
    let source_workspace = workspace_ops::workspace_room(app, &r.source, &source).await?;
    workspace_ops::persist_workspace(app, &r.source, &source, &source_workspace, &op.operation_id)
        .await?;
    crate::salience_value_store::purge_document_values_for_operation(
        app,
        &r.source,
        &r.document_ids,
        &op.operation_id,
    )?;
    receipt.complete = true;
    receipt.entries.clear();
    save(path, receipt)
}

#[cfg(test)]
#[path = "cross_graph_move_tests.rs"]
mod tests;
