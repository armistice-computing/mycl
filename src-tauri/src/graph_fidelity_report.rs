//! Fidelity report — how faithfully a graph archive (a cloud-1.0 export) made
//! it into a local graph, and optionally how faithfully the live cloud graph
//! made it into that archive in the first place.
//!
//! Three sides, two stages:
//!
//! ```text
//!   cloud (live prod, optional snapshot) --export--> archive --import--> local
//! ```
//!
//! Every exception names its `side` (`cloud` | `archive` | `local`) and, where
//! a loss can be placed, its `stage` (`export` | `import`), so "did the
//! migration work" becomes a measured number with named exceptions on the
//! side that is actually inconsistent — the ledger's demand since the maiden
//! import (2026-08-29), when catalog ghosts, dangling wires, and clipped
//! titles were each discovered by hand.
//!
//! Legacy cloud observations carry aggregate fingerprints, useful only for triage:
//!
//! - `block_count`   — number of distinct block ids in the document
//! - `block_id_hash` — sha256 of the sorted distinct block ids joined by `,`
//! - `text_length`   — sum of text-node lengths in Unicode code points
//!
//! Archive/local verification additionally hashes canonical TipTap content and
//! comments, checks workspace meaning, chronology and authority inventories.
//! Later cloud observations never certify historical export completeness.

use crate::app_runtime::AppHandle;
use crate::crdt_engine::import_archive_ops::{
    filter_reserved_projection_quads, parse_graph_archive, rewrite_graph_archive_rdf,
};
use crate::crdt_engine::{projection, workspace_ops};
use crate::graph_paths::existing_graph_dir_no_heal;
use crate::rdf_authority::is_reserved_rdf_graph_iri;
use crate::storage::read_bytes;
use crate::ydoc_paths::{document_ydoc_dir, document_ydoc_state_path, workspace_ydoc_state_path};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use yrs::updates::decoder::Decode;
use yrs::{Doc, ReadTxn, Transact, Update};

/// All archive documents must match. Independent coverage gates apply as well.
pub(crate) const FIDELITY_READY_PERCENT: f64 = 100.0;

const GEIST_ROOTS: [&str; 3] = ["memory", "narrative", "values"];
const ORIGINAL_ROOTS: [&str; 3] = ["artifacts", "images", "documents"];
const EXCEPTION_ID_PREVIEW: usize = 25;

// ─────────────────────────────────────────────────────────────────────────────
// Snapshot schema (also the on-disk JSON schema for a cloud snapshot)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DocFingerprint {
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) title: Option<String>,
    #[serde(default)]
    pub(crate) block_count: usize,
    #[serde(default)]
    pub(crate) block_id_hash: Option<String>,
    #[serde(default)]
    pub(crate) text_length: u64,
    /// RFC 3339 or epoch-millis string; only the cloud side carries it.
    #[serde(default)]
    pub(crate) updated_at: Option<String>,
    /// Canonical visible TipTap content, including order, marks and attributes.
    #[serde(default)]
    pub(crate) content_sha256: Option<String>,
    #[serde(default)]
    pub(crate) comments_sha256: Option<String>,
    #[serde(default)]
    pub(crate) unverified_roots: Vec<String>,
}

impl DocFingerprint {
    fn exact_content(&self, other: &DocFingerprint) -> bool {
        self.content_sha256.is_some()
            && self.content_sha256 == other.content_sha256
    }

    fn same_content(&self, other: &DocFingerprint) -> bool {
        self.block_count == other.block_count
            && self.block_id_hash == other.block_id_hash
            && self.text_length == other.text_length
    }

    fn brief(&self) -> String {
        format!(
            "{} blocks, {} chars, ids {}, content {}",
            self.block_count,
            self.text_length,
            self.block_id_hash
                .as_deref()
                .map(|hash| hash.chars().take(12).collect::<String>())
                .unwrap_or_else(|| "-".to_string()),
            self.content_sha256.as_deref().map(|s| s.chars().take(12).collect::<String>()).unwrap_or_else(|| "unavailable".into())
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CatalogEntry {
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WireEntry {
    pub(crate) id: String,
    pub(crate) source_document: String,
    pub(crate) target_document: String,
    #[serde(default)]
    pub(crate) predicate: Option<String>,
    /// `None` when the wire targets its own graph. Anything else is a
    /// cross-graph anomaly (there are no legitimate cross-graph wires; the
    /// rule is that they must fail gracefully, not that they work).
    #[serde(default)]
    pub(crate) target_graph: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FolderEntry {
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) name: Option<String>,
    #[serde(default)]
    pub(crate) parent: Option<String>,
}

/// One side of the comparison. The cloud snapshot file deserializes into
/// this directly (`side = "cloud"`; its `catalog` may be empty, in which case
/// the document list doubles as the catalog).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct SideSnapshot {
    #[serde(default)]
    pub(crate) side: String,
    #[serde(default)]
    pub(crate) graph_id: String,
    #[serde(default)]
    pub(crate) captured_at: Option<String>,
    /// Workspace catalog entries (what the sidebar would list).
    #[serde(default)]
    pub(crate) catalog: Vec<CatalogEntry>,
    /// Authority fingerprints (one per Y.Doc, or per cloud document).
    #[serde(default)]
    pub(crate) documents: Vec<DocFingerprint>,
    /// Local only: fingerprints of the materialized `document.json` records.
    #[serde(default)]
    pub(crate) projection: Vec<DocFingerprint>,
    #[serde(default)]
    pub(crate) wires: Vec<WireEntry>,
    #[serde(default)]
    pub(crate) folders: Vec<FolderEntry>,
    #[serde(default)]
    pub(crate) artifact_count: usize,
    #[serde(default)]
    pub(crate) originals: Vec<String>,
    #[serde(default)]
    pub(crate) geist: Vec<String>,
    #[serde(default)]
    pub(crate) rdf_user_quads: Option<usize>,
    #[serde(default)]
    pub(crate) rdf_reserved_quads: Option<usize>,
    #[serde(default)]
    pub(crate) warnings: Vec<String>,
    /// Only the archive/local collectors can attest complete authority coverage.
    #[serde(default)]
    pub(crate) evidence: Option<ImportEvidence>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct ImportEvidence {
    pub(crate) workspace: Value,
    pub(crate) chronicle: BTreeMap<String, Value>,
    pub(crate) geist_hashes: BTreeMap<String, String>,
    #[serde(default)]
    pub(crate) legacy_geist_hashes: BTreeMap<String, String>,
    #[serde(default)]
    pub(crate) legacy_valuation_records: usize,
    #[serde(default)]
    pub(crate) legacy_values: BTreeMap<String, Value>,
    #[serde(default)]
    pub(crate) source_valuation_hashes: BTreeMap<String, String>,
    #[serde(default)]
    pub(crate) source_value_keys: BTreeMap<String, String>,
    #[serde(default)]
    pub(crate) quarantined_valuation_hashes: BTreeMap<String, String>,
    #[serde(default)]
    pub(crate) legacy_clocks: BTreeMap<String, Value>,
}

/// Sort object keys recursively, preserving array order and all scalar values.
fn canonical_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => map.iter().map(|(k, v)| (k.clone(), canonical_value(v)))
            .collect::<BTreeMap<_, _>>().into_iter().collect::<serde_json::Map<_, _>>().into(),
        Value::Array(items) => Value::Array(items.iter().map(canonical_value).collect()),
        other => other.clone(),
    }
}

fn value_hash(value: &Value) -> String {
    hex_digest(&serde_json::to_vec(&canonical_value(value)).expect("JSON value serializes"))
}

fn canonical_tiptap(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut object: serde_json::Map<String, Value> = object.iter()
                .map(|(key, value)| (key.clone(), canonical_tiptap(value))).collect();
            // TipTap marks are a set; Yrs attribute iteration order is unstable.
            // Child/content arrays remain ordered. Preserve every mark and attribute.
            if object.contains_key("type") {
                if let Some(Value::Array(marks)) = object.get_mut("marks") {
                    marks.sort_by_key(|mark| canonical_value(mark).to_string());
                }
            }
            canonical_value(&Value::Object(object))
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical_tiptap).collect()),
        other => other.clone(),
    }
}

impl SideSnapshot {
    fn catalog_or_documents(&self) -> Vec<CatalogEntry> {
        if self.catalog.is_empty() {
            self.documents
                .iter()
                .map(|doc| CatalogEntry {
                    id: doc.id.clone(),
                    title: doc.title.clone(),
                })
                .collect()
        } else {
            self.catalog.clone()
        }
    }

    fn document_map(&self) -> BTreeMap<&str, &DocFingerprint> {
        self.documents
            .iter()
            .map(|doc| (doc.id.as_str(), doc))
            .collect()
    }

    fn counts(&self) -> Value {
        json!({
            "catalog": self.catalog_or_documents().len(),
            "documents": self.documents.len(),
            "projection": self.projection.len(),
            "wires": self.wires.len(),
            "folders": self.folders.len(),
            "artifacts": self.artifact_count,
            "originals": self.originals.len(),
            "geist": self.geist.len(),
            "rdf_user_quads": self.rdf_user_quads,
            "rdf_reserved_quads": self.rdf_reserved_quads,
            "warnings": self.warnings.len(),
        })
    }
}

/// What the archive says about itself, carried alongside its snapshot.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ArchiveIdentity {
    pub(crate) source_graph_id: String,
    pub(crate) source_user_id: String,
    pub(crate) source_graph_title: Option<String>,
    pub(crate) exported_at: Option<String>,
    pub(crate) includes_artifacts: bool,
    pub(crate) archive_size_bytes: usize,
}

// ─────────────────────────────────────────────────────────────────────────────
// Fingerprints
// ─────────────────────────────────────────────────────────────────────────────

/// Distinct block ids in canonical TipTap XML (`data-block-id="…"`).
pub(crate) fn block_ids_from_tiptap_xml(xml: &str) -> BTreeSet<String> {
    const NEEDLE: &str = "data-block-id=\"";
    let mut ids = BTreeSet::new();
    let mut rest = xml;
    while let Some(start) = rest.find(NEEDLE) {
        let after = &rest[start + NEEDLE.len()..];
        let Some(end) = after.find('"') else { break };
        let id = &after[..end];
        if !id.is_empty() {
            ids.insert(id.to_string());
        }
        rest = &after[end + 1..];
    }
    ids
}

/// Sum of `text` node lengths in code points, matching SPARQL `STRLEN`.
pub(crate) fn text_length_from_tiptap_json(node: &Value) -> u64 {
    match node {
        Value::Array(items) => items.iter().map(text_length_from_tiptap_json).sum(),
        Value::Object(map) => {
            let own = if map.get("type").and_then(Value::as_str) == Some("text") {
                map.get("text")
                    .and_then(Value::as_str)
                    .map(|text| text.chars().count() as u64)
                    .unwrap_or(0)
            } else {
                0
            };
            own + map
                .get("content")
                .map(text_length_from_tiptap_json)
                .unwrap_or(0)
        }
        _ => 0,
    }
}

fn hash_block_ids(ids: &BTreeSet<String>) -> Option<String> {
    if ids.is_empty() {
        return None;
    }
    let joined = ids.iter().cloned().collect::<Vec<_>>().join(",");
    Some(hex_digest(joined.as_bytes()))
}

fn hex_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub(crate) fn fingerprint_from_projection(
    id: &str,
    title: Option<&str>,
    tiptap_xml: &str,
    tiptap_json: &Value,
) -> DocFingerprint {
    let ids = block_ids_from_tiptap_xml(tiptap_xml);
    DocFingerprint {
        id: id.to_string(),
        title: title.map(str::to_string),
        block_count: ids.len(),
        block_id_hash: hash_block_ids(&ids),
        text_length: text_length_from_tiptap_json(tiptap_json),
        updated_at: None,
        content_sha256: (!tiptap_json.is_null()).then(|| value_hash(&canonical_tiptap(tiptap_json))),
        comments_sha256: None,
        unverified_roots: Vec::new(),
    }
}

fn fingerprint_from_ydoc(id: &str, title: Option<&str>, doc: &Doc) -> DocFingerprint {
    let xml = projection::ydoc_to_tiptap_xml(doc);
    let tiptap_json = projection::ydoc_to_tiptap_json(doc);
    let mut fingerprint = fingerprint_from_projection(id, title, &xml, &tiptap_json);
    use yrs::{ReadTxn, types::ToJson};
    let txn = doc.transact();
    let comments = txn.get_map("comments")
        .map(|map| serde_json::to_value(map.to_json(&txn)).expect("comments serialize"))
        .unwrap_or_else(|| json!({}));
    fingerprint.comments_sha256 = Some(value_hash(&comments));
    fingerprint.unverified_roots = txn.root_refs().map(|(name, _)| name.to_string())
        .filter(|name| name != "content" && name != "comments").collect();
    fingerprint.unverified_roots.sort();
    fingerprint
}

fn doc_from_update(bytes: &[u8], what: &str) -> Result<Doc, String> {
    let update =
        Update::decode_v1(bytes).map_err(|error| format!("decode {what} Y.Doc update: {error}"))?;
    let doc = Doc::new();
    {
        let mut txn = doc.transact_mut();
        txn.apply_update(update)
            .map_err(|error| format!("apply {what} Y.Doc update: {error}"))?;
        if txn.has_missing_updates() {
            return Err(format!("{what} contains unresolved Y.Doc dependencies"));
        }
    }
    Ok(doc)
}

// ─────────────────────────────────────────────────────────────────────────────
// Workspace snapshot → catalog / folders / wires / artifacts
// ─────────────────────────────────────────────────────────────────────────────

fn str_field(entry: &Value, key: &str) -> Option<String> {
    entry
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn fill_side_from_workspace_snapshot(side: &mut SideSnapshot, snapshot: &Value, self_graph_id: &str) {
    side.evidence.get_or_insert_with(ImportEvidence::default).workspace =
        workspace_evidence(snapshot, self_graph_id);
    let list = |key: &str| {
        snapshot
            .get(key)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    side.catalog = list("documents")
        .iter()
        .filter_map(|entry| {
            Some(CatalogEntry {
                id: str_field(entry, "id")?,
                title: str_field(entry, "title"),
            })
        })
        .collect();
    side.folders = list("folders")
        .iter()
        .filter_map(|entry| {
            Some(FolderEntry {
                id: str_field(entry, "id")?,
                name: str_field(entry, "name"),
                parent: str_field(entry, "parentId"),
            })
        })
        .collect();
    side.wires = list("wires")
        .iter()
        .filter_map(|entry| {
            let target_graph = str_field(entry, "targetGraphId")
                .filter(|graph_id| graph_id != self_graph_id);
            Some(WireEntry {
                id: str_field(entry, "id")?,
                source_document: str_field(entry, "sourceDocumentId").unwrap_or_default(),
                target_document: str_field(entry, "targetDocumentId").unwrap_or_default(),
                predicate: str_field(entry, "predicate"),
                target_graph,
            })
        })
        .collect();
    side.artifact_count = list("artifacts").len();
}

fn workspace_evidence(snapshot: &Value, graph_id: &str) -> Value {
    let mut result = serde_json::Map::new();
    for (kind, fields) in [
        ("documents", &["id", "title", "parentId", "order", "section", "readOnly"][..]),
        ("folders", &["id", "name", "parentId", "order", "section"][..]),
        ("wires", &["id", "sourceDocumentId", "targetDocumentId", "targetArtifactId", "sourceBlockId", "targetBlockId", "sourceMarkId", "targetMarkId", "predicate", "targetGraphId", "sceneGraphId", "bidirectional", "inverseOf", "deletedAt", "tombstonedAt"][..]),
    ] {
        let mut entries = BTreeMap::new();
        for entry in snapshot[kind].as_array().into_iter().flatten() {
            let Some(id) = entry["id"].as_str() else { continue };
            let mut selected = serde_json::Map::new();
            for field in fields {
                let mut value = entry.get(*field).cloned().unwrap_or(Value::Null);
                if matches!(*field, "targetGraphId" | "sceneGraphId") && value.as_str() == Some(graph_id) {
                    value = Value::Null;
                }
                selected.insert((*field).to_string(), value);
            }
            entries.insert(id.to_string(), Value::Object(selected));
        }
        result.insert(kind.to_string(), json!(entries));
    }
    Value::Object(result)
}

fn is_geist_path(path: &str) -> bool {
    GEIST_ROOTS
        .iter()
        .any(|root| path.starts_with(&format!("{root}/")))
}

// ─────────────────────────────────────────────────────────────────────────────
// Archive side
// ─────────────────────────────────────────────────────────────────────────────

/// Read the archive into a side snapshot. `target_graph_id` is the local
/// graph the archive was (or will be) imported into: the RDF identity rewrite
/// and the reserved-graph filter are applied exactly as the importer applies
/// them, so the quad count is "what the importer would load".
pub(crate) fn archive_side(
    archive_bytes: &[u8],
    target_graph_id: &str,
) -> Result<(SideSnapshot, ArchiveIdentity), String> {
    let parsed = parse_graph_archive(archive_bytes)?;
    let manifest = &parsed.manifest;
    let mut side = SideSnapshot {
        side: "archive".to_string(),
        graph_id: manifest.source_graph_id.clone(),
        captured_at: manifest.exported_at.clone(),
        warnings: parsed.warnings.clone(),
        ..SideSnapshot::default()
    };
    validate_archive_inventory(archive_bytes, &parsed, &mut side.warnings)?;

    if let Some(workspace_bytes) = &parsed.workspace_bytes {
        let workspace = doc_from_update(workspace_bytes, "archive workspace")?;
        let snapshot =
            workspace_ops::materialize_workspace_snapshot_json(&manifest.source_graph_id, &workspace)?;
        fill_side_from_workspace_snapshot(&mut side, &snapshot, &manifest.source_graph_id);
        if let Some(evidence) = &mut side.evidence {
            for entry in snapshot["documents"].as_array().into_iter().flatten() {
                let Some(id) = entry["id"].as_str() else { continue };
                let Some(created) = entry["createdAt"].as_f64().filter(|n| n.is_finite() && *n > 0.0) else { continue };
                let updated = entry["updatedAt"].as_f64().filter(|n| n.is_finite() && *n >= created).unwrap_or(created);
                evidence.legacy_clocks.insert(id.into(), json!({"created_at": (created as u128).to_string(), "updated_at":(updated as u128).to_string()}));
            }
        }
    }
    if let Some(evidence) = &mut side.evidence {
        let geist = crate::legacy_geist_import::plan(&parsed, target_graph_id)?;
        evidence.legacy_geist_hashes = geist.authority_evidence()?.into_iter()
            .map(|(path, value)| (path, value_hash(&value))).collect();
    }
    let titles: BTreeMap<&str, Option<&str>> = side
        .catalog
        .iter()
        .map(|entry| (entry.id.as_str(), entry.title.as_deref()))
        .collect();

    for (document_id, bytes) in &parsed.documents {
        let doc = doc_from_update(bytes, &format!("archive document {document_id}"))?;
        let title = titles.get(document_id.as_str()).copied().flatten();
        side.documents
            .push(fingerprint_from_ydoc(document_id, title, &doc));
    }

    for (path, _) in &parsed.originals {
        if is_geist_path(path) {
            side.geist.push(path.clone());
        } else {
            side.originals.push(path.clone());
        }
    }
    side.originals.sort();
    side.geist.sort();
    if let Some(evidence) = &mut side.evidence {
        for (path, bytes) in &parsed.originals {
            if is_geist_path(path) {
                evidence.geist_hashes.insert(path.clone(), hex_digest(bytes));
            }
        }
        for (id, entry) in &parsed.chronicle {
            if entry.document_id != *id {
                side.warnings.push(format!("chronicle identity mismatch: {id}"));
            }
            let mut value = serde_json::to_value(entry).expect("chronicle serializes");
            if value["createdAtSource"] != "import" {
                value["createdAtSource"] = json!("archive-chronicle");
            }
            evidence.chronicle.insert(id.clone(), value);
        }
    }

    if !parsed.rdf_n_quads.trim().is_empty() {
        // Independently inventory source valuation authorities BEFORE the importer
        // discards projection graphs. Sharing its filter would hide this loss.
        use oxigraph::io::{RdfFormat, RdfParser};
        let mut valuation_subjects = BTreeSet::new();
        let source_root = format!("urn:mnemosyne:user:{}:graph:{}", manifest.source_user_id, manifest.source_graph_id);
        let mut raw_values: BTreeMap<String, BTreeMap<String, oxigraph::model::Term>> = BTreeMap::new();
        let mut raw_quads: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut conflicting_subjects = BTreeSet::new();
        for quad in RdfParser::from_format(RdfFormat::NQuads).for_slice(parsed.rdf_n_quads.as_bytes()) {
            let quad = quad.map_err(|error| format!("invalid source RDF: {error}"))?;
            if quad.graph_name.to_string() != format!("<{source_root}>")
                || !quad.subject.to_string().starts_with(&format!("<{source_root}:valuation:")) { continue; }
            raw_quads.entry(quad.subject.to_string()).or_default().insert(quad.to_string());
            if quad.predicate.as_str() == "http://mnemosyne.dev/doc#blockRef" {
                valuation_subjects.insert(quad.subject.to_string());
            }
            let fields = raw_values.entry(quad.subject.to_string()).or_default();
            let key = quad.predicate.as_str().to_string();
            if fields.get(&key).is_some_and(|previous| previous != &quad.object) {
                conflicting_subjects.insert(quad.subject.to_string());
            }
            fields.insert(key, quad.object);
        }
        if let Some(evidence) = &mut side.evidence {
            evidence.legacy_valuation_records = raw_values.len();
            let mut duplicate_keys = BTreeSet::new();
            for (subject, fields) in &raw_values {
                if conflicting_subjects.contains(subject) { continue; }
                let one = BTreeMap::from([(subject.clone(), fields.clone())]);
                if let Ok(records) = source_value_records(&source_root, &one) {
                    for (key, value) in records {
                        evidence.source_value_keys.insert(subject.clone(), key.clone());
                        if duplicate_keys.contains(&key) { continue; }
                        if evidence.legacy_values.insert(key.clone(), value).is_some() {
                            duplicate_keys.insert(key.clone());
                            evidence.legacy_values.remove(&key);
                        }
                    }
                }
            }
            evidence.source_valuation_hashes = raw_quads.into_iter().map(|(subject, quads)| (subject, value_hash(&json!(quads)))).collect();
        }
        let rewritten = rewrite_graph_archive_rdf(&parsed.rdf_n_quads, manifest, target_graph_id);
        let (kept, dropped) = filter_reserved_projection_quads(&rewritten, target_graph_id);
        side.rdf_user_quads = Some(kept.lines().filter(|line| !line.trim().is_empty()).count());
        side.rdf_reserved_quads = Some(dropped);
    }

    let identity = ArchiveIdentity {
        source_graph_id: manifest.source_graph_id.clone(),
        source_user_id: manifest.source_user_id.clone(),
        source_graph_title: manifest.source_graph_title.clone(),
        exported_at: manifest.exported_at.clone(),
        includes_artifacts: manifest.includes_artifacts,
        archive_size_bytes: archive_bytes.len(),
    };
    Ok((side, identity))
}

/// Independent tar enumeration catches entries the production parser skipped.
fn validate_archive_inventory(bytes: &[u8], parsed: &crate::crdt_engine::import_archive_ops::ParsedGraphArchive, warnings: &mut Vec<String>) -> Result<(), String> {
    let parsed_docs = &parsed.documents;
    use std::io::Read;
    let decoder = flate2::read::GzDecoder::new(bytes);
    let mut tar = tar::Archive::new(decoder);
    let mut names = BTreeSet::new();
    let mut manifest = Value::Null;
    for entry in tar.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        let name = entry.path().map_err(|e| e.to_string())?.to_string_lossy().into_owned();
        if !entry.header().entry_type().is_file() { continue; }
        if !names.insert(name.clone()) { warnings.push(format!("duplicate archive member: {name}")); }
        if name == "manifest.json" {
            let mut data = Vec::new();
            entry.read_to_end(&mut data).map_err(|e| e.to_string())?;
            manifest = serde_json::from_slice(&data).map_err(|e| e.to_string())?;
        }
    }
    for key in ["workspace", "rdf"] {
        match manifest["files"][key].as_str() {
            Some(path) if names.contains(path) => {},
            _ => warnings.push(format!("missing declared archive {key} payload")),
        }
    }
    if let Some(count) = manifest["document_count"].as_u64() {
        if count != parsed_docs.len() as u64 { warnings.push(format!("manifest document_count={count}, decoded entries={}", parsed_docs.len())); }
    }
    let prefix = manifest["files"]["documents_dir"].as_str().unwrap_or("crdt/documents/");
    let members: BTreeSet<_> = names.iter().filter(|name| name.starts_with(prefix) && name.ends_with(".yjs")).cloned().collect();
    let decoded: BTreeSet<_> = parsed_docs.iter().map(|(id, _)| format!("{}{id}.yjs", if prefix.ends_with('/') { prefix.to_string() } else { format!("{prefix}/") })).collect();
    if members != decoded { warnings.push("tar document member inventory differs from the importer's decoded inventory".into()); }
    let geist_members: BTreeSet<_> = names.iter().filter(|name| is_geist_path(name)).cloned().collect();
    let decoded_geist: BTreeSet<_> = parsed.originals.iter().filter(|(path, _)| is_geist_path(path)).map(|(path, _)| path.clone()).collect();
    if geist_members != decoded_geist { warnings.push("tar Geist inventory differs from decoded authority files".into()); }
    let prefix = manifest["files"]["chronicle_dir"].as_str().unwrap_or("chronicle/documents/");
    let prefix = format!("{}/", prefix.trim_end_matches('/'));
    let entries: BTreeSet<_> = names.iter().filter(|name| name.starts_with(&prefix) && name.ends_with(".json")).cloned().collect();
    let decoded: BTreeSet<_> = parsed.chronicle.iter().map(|(id, _)| format!("{prefix}{id}.json")).collect();
    if entries != decoded { warnings.push("tar Chronicle inventory differs from decoded entries".into()); }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Local side
// ─────────────────────────────────────────────────────────────────────────────

fn walk_relative_files(root: &Path, base: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(root) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_relative_files(&path, base, out);
        } else if let Ok(relative) = path.strip_prefix(base) {
            out.push(relative.to_string_lossy().replace('\\', "/"));
        }
    }
}

fn local_originals(graph_dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for root in ORIGINAL_ROOTS {
        let root_dir = graph_dir.join(root);
        let Ok(entries) = std::fs::read_dir(&root_dir) else { continue };
        for entry in entries.flatten() {
            let original = entry.path().join("original");
            if original.is_dir() {
                walk_relative_files(&original, graph_dir, &mut out);
            }
        }
    }
    out.sort();
    out
}

fn local_geist(graph_dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for root in GEIST_ROOTS {
        let root_dir = graph_dir.join(root);
        if root_dir.is_dir() {
            walk_relative_files(&root_dir, graph_dir, &mut out);
        }
    }
    out.sort();
    out
}

/// Graph term of one N-Quads line: `Ok(None)` for the default graph,
/// `Ok(Some(iri))` for a named graph, `Err` when the line does not parse.
pub(crate) fn nquad_graph_iri(line: &str) -> Result<Option<String>, String> {
    let trimmed = line.trim();
    let body = trimmed
        .strip_suffix('.')
        .ok_or_else(|| "quad does not end with '.'".to_string())?
        .trim_end();
    let mut rest = body;
    let mut take_term = |allow_literal: bool| -> Result<(), String> {
        rest = rest.trim_start();
        if rest.starts_with('<') {
            let end = rest.find('>').ok_or("unterminated IRI")?;
            rest = &rest[end + 1..];
        } else if rest.starts_with("_:") {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            rest = &rest[end..];
        } else if allow_literal && rest.starts_with('"') {
            let bytes = rest.as_bytes();
            let mut index = 1;
            let mut escaped = false;
            loop {
                let byte = *bytes.get(index).ok_or("unterminated literal")?;
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'"' {
                    break;
                }
                index += 1;
            }
            rest = &rest[index + 1..];
            if rest.starts_with("^^<") {
                let end = rest.find('>').ok_or("unterminated datatype IRI")?;
                rest = &rest[end + 1..];
            } else if rest.starts_with('@') {
                let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
                rest = &rest[end..];
            }
        } else {
            return Err(format!("unexpected term start: {}", rest.chars().take(20).collect::<String>()));
        }
        Ok(())
    };
    take_term(false)?;
    take_term(false)?;
    take_term(true)?;
    let graph = rest.trim();
    if graph.is_empty() {
        return Ok(None);
    }
    let iri = graph
        .strip_prefix('<')
        .and_then(|value| value.strip_suffix('>'))
        .ok_or_else(|| format!("unexpected graph term: {graph}"))?;
    Ok(Some(iri.to_string()))
}

fn count_local_rdf(app: &AppHandle, graph_id: &str, side: &mut SideSnapshot) {
    let dump = crate::rdf_service::dump_rdf(
        app.clone(),
        crate::rdf_service::RdfDumpInput {
            graph_id: graph_id.to_string(),
            format: "application/n-quads".to_string(),
            source_graph_iri: None,
        },
    );
    let dump = match dump {
        Ok(dump) => dump,
        Err(error) => {
            side.warnings.push(format!("local RDF dump failed: {error}"));
            return;
        }
    };
    let mut user = 0usize;
    let mut reserved = 0usize;
    let mut unparsed = 0usize;
    for line in dump.data.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match nquad_graph_iri(line) {
            Ok(None) => user += 1,
            Ok(Some(iri)) => {
                if iri.contains(":projection:") || is_reserved_rdf_graph_iri(graph_id, &iri) {
                    reserved += 1;
                } else {
                    user += 1;
                }
            }
            Err(_) => unparsed += 1,
        }
    }
    if unparsed > 0 {
        side.warnings
            .push(format!("{unparsed} local RDF quad line(s) did not parse and were not counted"));
    }
    side.rdf_user_quads = Some(user);
    side.rdf_reserved_quads = Some(reserved);
}

/// Read the local graph into a side snapshot: workspace authority for the
/// catalog, per-document Y.Doc authority for fingerprints, `document.json`
/// records for the projection, plus RDF/originals/geist inventories.
pub(crate) fn local_side(app: &AppHandle, graph_id: &str) -> Result<SideSnapshot, String> {
    let graph_dir = existing_graph_dir_no_heal(app, graph_id)?;
    local_side_from_dir(&graph_dir, graph_id)
}

fn local_side_from_dir(graph_dir: &Path, graph_id: &str) -> Result<SideSnapshot, String> {
    let mut side = SideSnapshot {
        side: "local".to_string(),
        graph_id: graph_id.to_string(),
        captured_at: Some(crate::clock::timestamp()),
        ..SideSnapshot::default()
    };
    let workspace_state = workspace_ydoc_state_path(&graph_dir);
    if workspace_state.is_file() {
        let bytes = read_bytes(&workspace_state)?;
        let workspace = doc_from_update(&bytes, "local workspace")?;
        let snapshot = workspace_ops::materialize_workspace_snapshot_json(graph_id, &workspace)?;
        fill_side_from_workspace_snapshot(&mut side, &snapshot, graph_id);
    } else {
        side.warnings
            .push("local graph has no workspace Y.Doc state file".to_string());
    }
    let titles: BTreeMap<String, Option<String>> = side
        .catalog
        .iter()
        .map(|entry| (entry.id.clone(), entry.title.clone()))
        .collect();

    // Projection: the materialized records.
    let records = crate::migration_document_evidence::read_records(graph_dir, graph_id)?;
    let mut record_titles: BTreeMap<String, String> = BTreeMap::new();
    for record in &records {
        if let Some(Some(title)) = titles.get(&record.document_id) {
            if title != &record.title {
                side.warnings.push(format!("document {}: record title differs from workspace title", record.document_id));
            }
        }
        if let Some(evidence) = &mut side.evidence {
            let value = serde_json::to_value(crate::document_chronicle::ChronicleEntry::from_record(record))
                .expect("chronicle serializes");
            evidence.chronicle.insert(record.document_id.clone(), value);
            evidence.legacy_clocks.insert(record.document_id.clone(), json!({"created_at":record.created_at,"updated_at":record.updated_at}));
        }
        record_titles.insert(record.document_id.clone(), record.title.clone());
        let tiptap_json = record.tiptap_json.clone().unwrap_or(Value::Null);
        if record.tiptap_json.is_none() {
            side.warnings.push(format!(
                "document {} record has no tiptapJson; its projection text length reads as 0",
                record.document_id
            ));
        }
        side.projection.push(fingerprint_from_projection(
            &record.document_id,
            Some(record.title.as_str()),
            &record.tiptap_xml,
            &tiptap_json,
        ));
    }

    // Authority: every persisted document Y.Doc, whether or not a record or
    // catalog entry exists for it.
    let ydocs_root = document_ydoc_dir(&graph_dir, "probe")
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "resolve document Y.Doc root".to_string())?;
    let mut ydoc_ids: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&ydocs_root) {
        for entry in entries.flatten() {
            let Ok(name) = entry.file_name().into_string() else { continue };
            if document_ydoc_state_path(&graph_dir, &name).is_file() {
                ydoc_ids.push(name);
            }
        }
    }
    ydoc_ids.sort();
    for document_id in ydoc_ids {
        let bytes = read_bytes(&document_ydoc_state_path(&graph_dir, &document_id))?;
        let doc = doc_from_update(&bytes, &format!("local document {document_id}"))?;
        if let Some(record) = records.iter().find(|record| record.document_id == document_id) {
            for error in crate::migration_document_evidence::projection_errors(record, &doc) {
                side.warnings.push(format!("document {document_id}: {error}"));
            }
        }
        let title = record_titles
            .get(&document_id)
            .cloned()
            .or_else(|| titles.get(&document_id).cloned().flatten());
        side.documents
            .push(fingerprint_from_ydoc(&document_id, title.as_deref(), &doc));
    }

    side.originals = local_originals(&graph_dir);
    side.geist = local_geist(&graph_dir);
    if let Some(evidence) = &mut side.evidence {
        for relative in ["narrative/song.json", "memory/memory-queue.json", "memory/legacy-geist-source.json", "values/block-values.json#config"] {
            let file = relative.split('#').next().unwrap();
            let path = graph_dir.join(file);
            if path.is_file() {
                let mut value: Value = serde_json::from_slice(&read_bytes(&path)?).map_err(|e|e.to_string())?;
                if relative.ends_with("#config") { value = value["config"].clone(); }
                evidence.legacy_geist_hashes.insert(relative.into(), value_hash(&value));
            }
        }
        for path in &side.geist {
            evidence.geist_hashes.insert(path.clone(), hex_digest(&read_bytes(&graph_dir.join(path))?));
        }
        let path = graph_dir.join("values/block-values.json");
        if path.is_file() {
            let store: Value = serde_json::from_slice(&read_bytes(&path)?).map_err(|e| e.to_string())?;
            if store["graphId"] != graph_id || store["observer"].as_str().unwrap_or("") != "" {
                return Err("shared valuation store identity mismatch".into());
            }
            let blocks = store["blocks"].as_object().ok_or("shared valuation store has no blocks map")?;
            evidence.legacy_values = blocks.iter().map(|(key, value)| (key.clone(), value.clone())).collect();
        }
        let path = graph_dir.join("values/legacy-valuation-quarantine.json");
        if path.is_file() {
            use oxigraph::io::{RdfFormat, RdfParser};
            let quarantine: Value = serde_json::from_slice(&read_bytes(&path)?).map_err(|e| e.to_string())?;
            for (subject, record) in quarantine["records"].as_object().ok_or("quarantine records missing")? {
                let mut quads = BTreeSet::new();
                for quad in RdfParser::from_format(RdfFormat::NQuads).for_slice(record["rdf"].as_str().ok_or("quarantine RDF missing")?.as_bytes()) {
                    quads.insert(quad.map_err(|e| e.to_string())?.to_string());
                }
                evidence.quarantined_valuation_hashes.insert(format!("<{}>", subject.trim_start_matches('<').trim_end_matches('>')), value_hash(&json!(quads)));
            }
        }
    }
    Ok(side)
}

// ─────────────────────────────────────────────────────────────────────────────
// Comparison
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
struct Exception {
    kind: &'static str,
    side: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    stage: Option<&'static str>,
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    detail: String,
}

struct Exceptions(Vec<Exception>);

impl Exceptions {
    fn push(
        &mut self,
        kind: &'static str,
        side: &'static str,
        stage: Option<&'static str>,
        id: impl Into<String>,
        title: Option<String>,
        detail: impl Into<String>,
    ) {
        self.0.push(Exception {
            kind,
            side,
            stage,
            id: id.into(),
            title,
            detail: detail.into(),
        });
    }

    fn count(&self, kind: &str) -> usize {
        self.0.iter().filter(|item| item.kind == kind).count()
    }

    fn ids(&self, kind: &str) -> Vec<String> {
        self.0
            .iter()
            .filter(|item| item.kind == kind)
            .map(|item| item.id.clone())
            .collect()
    }
}

fn parse_instant_millis(value: &str) -> Option<i64> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.chars().all(|c| c.is_ascii_digit()) {
        return trimmed.parse::<i64>().ok();
    }
    chrono::DateTime::parse_from_rfc3339(trimmed)
        .ok()
        .map(|instant| instant.timestamp_millis())
}

/// `Some(true)` when `value` is strictly after `cutoff`; `None` when either
/// side is missing or unparseable (the caller then treats it as "unknown").
fn is_after(value: Option<&str>, cutoff: Option<&str>) -> Option<bool> {
    let value = parse_instant_millis(value?)?;
    let cutoff = parse_instant_millis(cutoff?)?;
    Some(value > cutoff)
}

fn folder_key(folder: &FolderEntry) -> (String, Option<String>, Option<String>) {
    (folder.id.clone(), folder.name.clone(), folder.parent.clone())
}

fn compare_import_evidence(archive: &SideSnapshot, local: &SideSnapshot, exceptions: &mut Exceptions) {
    let (Some(expected), Some(actual)) = (&archive.evidence, &local.evidence) else {
        exceptions.push("incomplete_evidence", "local", Some("import"), "workspace", None,
            "complete archive and local workspace evidence is required");
        return;
    };
    for kind in ["documents", "folders", "wires"] {
        let (Some(left), Some(right)) = (expected.workspace[kind].as_object(), actual.workspace[kind].as_object()) else {
            exceptions.push("incomplete_evidence", "local", Some("import"), kind, None, "workspace collection missing");
            continue;
        };
        for id in left.keys().chain(right.keys()).collect::<BTreeSet<_>>() {
            if left.get(id) != right.get(id) {
                exceptions.push("workspace_drift", "local", Some("import"), id.clone(), None,
                    format!("{kind} metadata differs (title, placement, order, or wire meaning/anchor)"));
            }
        }
    }
    for (id, chronicle) in &expected.chronicle {
        if actual.chronicle.get(id) != Some(chronicle) {
            exceptions.push("chronicle_drift", "local", Some("import"), id.clone(), None,
                "archive clock, revision or authorship did not survive restoration");
        }
    }
    for (id, clocks) in &expected.legacy_clocks {
        if !expected.chronicle.contains_key(id) && actual.legacy_clocks.get(id) != Some(clocks) {
            exceptions.push("clock_drift", "local", Some("import"), id.clone(), None,
                "cloud archive catalog clock differs from local document record");
        }
    }
    for (path, hash) in &expected.geist_hashes {
        if actual.geist_hashes.get(path) != Some(hash) {
            exceptions.push("geist_content_drift", "local", Some("import"), path.clone(), None,
                "native Geist authority bytes are missing or changed");
        }
    }
    for (path, hash) in &expected.legacy_geist_hashes {
        if actual.legacy_geist_hashes.get(path) != Some(hash) {
            exceptions.push("geist_translation_unverified", "local", Some("import"), path.clone(), None,
                "cloud Geist does not match its translated native authority or immutable source backup");
        }
    }
    for (key, expected_value) in &expected.legacy_values {
        let equal = actual.legacy_values.get(key).is_some_and(|actual_value| stored_value_matches(expected_value, actual_value));
        if !equal {
            exceptions.push("valuation_translation_unverified", "local", Some("import"), key.clone(), None,
                "source valuation fields differ or are absent in native shared store");
        }
    }
    if expected.legacy_valuation_records != expected.legacy_values.len() {
        exceptions.push("valuation_translation_unverified", "local", Some("import"), "valuations", None,
            "source includes valuation rows without usable block references; retained quarantine requires review");
    }
    for (subject, hash) in &actual.quarantined_valuation_hashes {
        if expected.legacy_valuation_records == 0 { continue; } // Native archives verify this file by its archived bytes.
        if expected.source_valuation_hashes.get(subject) != Some(hash) {
            exceptions.push("valuation_quarantine_drift", "local", Some("import"), subject.clone(), None,
                "quarantined source valuation RDF differs from the archive");
        }
    }
    for (subject, hash) in &expected.source_valuation_hashes {
        let mapped = expected.source_value_keys.get(subject).is_some_and(|key| {
            expected.legacy_values.get(key).zip(actual.legacy_values.get(key))
                .is_some_and(|(expected, actual)| stored_value_matches(expected, actual))
        });
        if !mapped && actual.quarantined_valuation_hashes.get(subject) != Some(hash) {
            exceptions.push("valuation_quarantine_missing", "local", Some("import"), subject.clone(), None,
                "source valuation has neither a matching native record nor lossless quarantine RDF");
        }
    }
    if archive.rdf_user_quads.is_some_and(|count| count > 0) {
        exceptions.push("user_rdf_content_unverified", "local", Some("import"), "user-rdf", None,
            "user RDF requires a semantic content comparison; matching quad counts cannot certify preservation");
    }
}

fn stored_value_matches(expected: &Value, actual: &Value) -> bool {
    expected.as_object().is_some_and(|fields| fields.iter().all(|(field, expected)| {
        if field.ends_with("Count") {
            return expected.as_u64().is_some() && expected.as_u64() == actual[field].as_u64();
        }
        match (expected.as_f64(), actual[field].as_f64()) {
            (Some(left), Some(right)) => left == right,
            _ => actual.get(field) == Some(expected),
        }
    }))
}

// Independent verifier oracle: map source predicates to expected stored fields,
// without calling the importer's translator or value-store backfill helpers.
fn source_value_records(
    source_root: &str,
    subjects: &BTreeMap<String, BTreeMap<String, oxigraph::model::Term>>,
) -> Result<BTreeMap<String, Value>, String> {
    use oxigraph::model::Term;
    let ns = "http://mnemosyne.dev/doc#";
    let mut result = BTreeMap::new();
    for fields in subjects.values() {
        for (predicate, term) in fields {
            if predicate == "http://www.w3.org/1999/02/22-rdf-syntax-ns#type" {
                if !matches!(term, Term::NamedNode(node) if node.as_str() == format!("{ns}BlockValuation")) {
                    return Err("unrepresentable valuation type".into());
                }
                continue;
            }
            let name = predicate.strip_prefix(ns).ok_or("unrepresentable valuation predicate")?;
            if !matches!(name, "blockRef" | "rawImportanceSum" | "rawValenceSum" | "cumulativeImportance" | "cumulativeValence"
                | "importanceCount" | "valenceCount" | "userImportance" | "userValence" | "lastValuatedAt") {
                return Err(format!("unrepresentable valuation field {name}"));
            }
            if name == "blockRef" { continue; }
            let Term::Literal(literal) = term else { return Err("valuation field is not a literal".into()); };
            if literal.language().is_some() { return Err("language-tagged valuation field".into()); }
            let datatype = literal.datatype();
            let kind = datatype.as_str().strip_prefix("http://www.w3.org/2001/XMLSchema#").unwrap_or("");
            if name == "lastValuatedAt" {
                if !matches!(kind, "string" | "dateTime" | "dateTimeStamp") { return Err("unrepresentable time datatype".into()); }
                if !literal.value().is_empty() && chrono::DateTime::parse_from_rfc3339(literal.value()).is_err() { return Err("unrepresentable valuation time".into()); }
            } else if !matches!(kind, "string" | "float" | "double" | "decimal" | "integer" | "long" | "int" | "short" | "byte"
                | "nonNegativeInteger" | "positiveInteger" | "nonPositiveInteger" | "negativeInteger" | "unsignedLong" | "unsignedInt" | "unsignedShort" | "unsignedByte") {
                return Err("unrepresentable numeric datatype".into());
            }
        }
        for names in [["rawImportanceSum", "importanceCount", "cumulativeImportance"], ["rawValenceSum", "valenceCount", "cumulativeValence"]] {
            let present = names.iter().filter(|name| fields.contains_key(&format!("{ns}{name}"))).count();
            if present != 0 && present != 3 { return Err("incomplete source aggregate requires quarantine".into()); }
        }
        let Some(Term::NamedNode(reference)) = fields.get(&format!("{ns}blockRef")) else {
            continue; // Unlinked source rows remain counted and block certification.
        };
        let reference = reference.as_str().strip_prefix(&format!("{source_root}:doc:"))
            .ok_or("valuation points outside source graph")?;
        let (document_id, block_id) = reference.split_once("#block-").ok_or("invalid valuation block reference")?;
        crate::ids::validate_local_id(document_id, "valuation document")?;
        crate::ids::validate_local_id(block_id, "valuation block")?;
        let mut value = json!({"documentId":document_id,"blockId":block_id,"tags":[],
            "userImportance":null,"userValence":null,"lastValuatedAt":""});
        for name in ["rawImportanceSum", "rawValenceSum", "cumulativeImportance", "cumulativeValence",
            "importanceCount", "valenceCount", "userImportance", "userValence", "lastValuatedAt"] {
            let term = fields.get(&format!("{ns}{name}"));
            match term {
                Some(Term::Literal(literal)) if name == "lastValuatedAt" => value[name] = json!(literal.value()),
                Some(Term::Literal(literal)) if name.ends_with("Count") => {
                    let count: u64 = literal.value().parse().map_err(|_| format!("invalid source count {name}"))?;
                    value[name] = json!(count);
                }
                Some(Term::Literal(literal)) => {
                    let number: f64 = literal.value().parse().map_err(|_| format!("non-numeric source {name}"))?;
                    if !number.is_finite() { return Err(format!("non-finite source {name}")); }
                    value[name] = json!(number);
                }
                None if name.ends_with("Count") => value[name] = json!(0),
                None if !matches!(name, "lastValuatedAt" | "userImportance" | "userValence") => value[name] = json!(0.0),
                None => {},
                _ => return Err(format!("source {name} is not a literal")),
            }
        }
        value["valuationCount"] = json!(value["importanceCount"].as_u64().unwrap()
            .checked_add(value["valenceCount"].as_u64().unwrap()).ok_or("source valuation count overflow")?);
        if value["rawImportanceSum"].as_f64().unwrap() < 0.0 || value["cumulativeImportance"].as_f64().unwrap() < 0.0
            || value["userImportance"].as_f64().is_some_and(|n| !(0.0..=5.0).contains(&n))
            || value["userValence"].as_f64().is_some_and(|n| !(-5.0..=5.0).contains(&n)) {
            return Err("source valuation outside supported range".into());
        }
        for (raw, count, cumulative) in [("rawImportanceSum", "importanceCount", "cumulativeImportance"), ("rawValenceSum", "valenceCount", "cumulativeValence")] {
            if value[cumulative].as_f64().unwrap() != 0.0 && (value[raw].as_f64().unwrap() == 0.0 || value[count].as_u64().unwrap() == 0) {
                return Err("native backfill would change source statistics".into());
            }
        }
        let key = format!("{document_id}:{block_id}");
        if result.insert(key.clone(), value).is_some() { return Err(format!("duplicate source valuation key {key}")); }
    }
    Ok(result)
}

fn catalog_consistency(side: &SideSnapshot, label: &'static str, exceptions: &mut Exceptions) {
    let catalog = side.catalog_or_documents();
    let catalog_ids: BTreeSet<&str> = catalog.iter().map(|entry| entry.id.as_str()).collect();
    let document_ids: BTreeSet<&str> = side.documents.iter().map(|doc| doc.id.as_str()).collect();
    if label == "cloud" {
        for doc in &side.documents {
            if doc.block_count == 0 {
                exceptions.push(
                    "cloud_ghost",
                    "cloud",
                    None,
                    doc.id.clone(),
                    doc.title.clone(),
                    "cloud lists the document but it has no blocks in RDF (residue or empty doc)",
                );
            }
        }
        return;
    }
    for entry in &catalog {
        if !document_ids.contains(entry.id.as_str()) {
            exceptions.push(
                "catalog_ghost",
                label,
                None,
                entry.id.clone(),
                entry.title.clone(),
                format!("{label} catalog lists the document but no {label} Y.Doc exists for it"),
            );
        }
    }
    for doc in &side.documents {
        if !catalog_ids.contains(doc.id.as_str()) {
            exceptions.push(
                "catalog_orphan",
                label,
                None,
                doc.id.clone(),
                doc.title.clone(),
                format!("{label} Y.Doc exists but the {label} catalog never lists it (invisible in the sidebar)"),
            );
        }
    }
}

fn compare_wires(
    from: &SideSnapshot,
    to: &SideSnapshot,
    missing_kind: &'static str,
    extra_kind: &'static str,
    to_side: &'static str,
    stage: Option<&'static str>,
    exceptions: &mut Exceptions,
) {
    let from_ids: BTreeMap<&str, &WireEntry> = from.wires.iter().map(|w| (w.id.as_str(), w)).collect();
    let to_ids: BTreeMap<&str, &WireEntry> = to.wires.iter().map(|w| (w.id.as_str(), w)).collect();
    for (id, wire) in &from_ids {
        if !to_ids.contains_key(id) {
            exceptions.push(
                missing_kind,
                to_side,
                stage,
                (*id).to_string(),
                None,
                format!("{} → {} ({})", wire.source_document, wire.target_document, wire.predicate.as_deref().unwrap_or("?")),
            );
        }
    }
    for (id, wire) in &to_ids {
        if !from_ids.contains_key(id) {
            exceptions.push(
                extra_kind,
                to_side,
                None,
                (*id).to_string(),
                None,
                format!("{} → {} ({})", wire.source_document, wire.target_document, wire.predicate.as_deref().unwrap_or("?")),
            );
        }
    }
}

fn wire_hygiene(side: &SideSnapshot, label: &'static str, exceptions: &mut Exceptions) {
    let document_ids: BTreeSet<&str> = side.documents.iter().map(|doc| doc.id.as_str()).collect();
    for wire in &side.wires {
        if let Some(graph) = &wire.target_graph {
            exceptions.push(
                "wire_cross_graph",
                label,
                None,
                wire.id.clone(),
                None,
                format!("targets graph {graph} — an anomaly; must fail gracefully, never expected to resolve"),
            );
            continue;
        }
        let mut dangling = Vec::new();
        if !document_ids.contains(wire.source_document.as_str()) {
            dangling.push(format!("source {}", wire.source_document));
        }
        if !document_ids.contains(wire.target_document.as_str()) {
            dangling.push(format!("target {}", wire.target_document));
        }
        if !dangling.is_empty() {
            exceptions.push(
                "wire_dangling",
                label,
                None,
                wire.id.clone(),
                None,
                format!("endpoint document missing on the {label} side: {}", dangling.join(", ")),
            );
        }
    }
}

fn compare_folders(
    from: &SideSnapshot,
    to: &SideSnapshot,
    missing_kind: &'static str,
    extra_kind: &'static str,
    changed_kind: &'static str,
    to_side: &'static str,
    stage: Option<&'static str>,
    exceptions: &mut Exceptions,
) {
    let from_map: BTreeMap<&str, &FolderEntry> = from.folders.iter().map(|f| (f.id.as_str(), f)).collect();
    let to_map: BTreeMap<&str, &FolderEntry> = to.folders.iter().map(|f| (f.id.as_str(), f)).collect();
    for (id, folder) in &from_map {
        match to_map.get(id) {
            None => exceptions.push(missing_kind, to_side, stage, (*id).to_string(), folder.name.clone(), "folder absent"),
            Some(other) if folder_key(folder) != folder_key(other) => exceptions.push(
                changed_kind,
                to_side,
                stage,
                (*id).to_string(),
                folder.name.clone(),
                format!(
                    "name/parent differ: {:?}/{:?} vs {:?}/{:?}",
                    folder.name, folder.parent, other.name, other.parent
                ),
            ),
            Some(_) => {}
        }
    }
    for (id, folder) in &to_map {
        if !from_map.contains_key(id) {
            exceptions.push(extra_kind, to_side, None, (*id).to_string(), folder.name.clone(), "folder only on this side");
        }
    }
}

/// The pure comparison. `exported_at` (from the archive manifest) is the cut
/// that separates cloud drift after the export from loss at export time.
pub(crate) fn compare(
    archive: &SideSnapshot,
    local: &SideSnapshot,
    cloud: Option<&SideSnapshot>,
    exported_at: Option<&str>,
) -> Value {
    let mut exceptions = Exceptions(Vec::new());
    compare_import_evidence(archive, local, &mut exceptions);

    // ── Catalog consistency on each side ──────────────────────────────────
    catalog_consistency(archive, "archive", &mut exceptions);
    catalog_consistency(local, "local", &mut exceptions);
    if let Some(cloud) = cloud {
        catalog_consistency(cloud, "cloud", &mut exceptions);
    }

    // ── Documents: archive → local (import stage) ─────────────────────────
    let archive_docs = archive.document_map();
    let local_docs = local.document_map();
    let local_projection: BTreeMap<&str, &DocFingerprint> =
        local.projection.iter().map(|doc| (doc.id.as_str(), doc)).collect();
    let local_catalog_titles: BTreeMap<&str, Option<&str>> = local
        .catalog
        .iter()
        .map(|entry| (entry.id.as_str(), entry.title.as_deref()))
        .collect();

    let mut exact = 0usize;
    for (id, archive_doc) in &archive_docs {
        if !archive_doc.unverified_roots.is_empty() {
            exceptions.push("unverified_ydoc_roots", "archive", Some("import"), (*id).to_string(), None,
                format!("unmapped document roots: {:?}", archive_doc.unverified_roots));
        }
        let Some(local_doc) = local_docs.get(id) else {
            exceptions.push(
                "missing_locally",
                "local",
                Some("import"),
                (*id).to_string(),
                archive_doc.title.clone(),
                format!("archive Y.Doc ({}) has no local Y.Doc", archive_doc.brief()),
            );
            continue;
        };
        let mut is_exact = true;
        if !local_doc.unverified_roots.is_empty() {
            exceptions.push("unverified_ydoc_roots", "local", Some("import"), (*id).to_string(), None,
                format!("unmapped local document roots: {:?}", local_doc.unverified_roots));
        }
        if !archive_doc.exact_content(local_doc)
            || archive_doc.comments_sha256.is_none()
            || archive_doc.comments_sha256 != local_doc.comments_sha256 {
            is_exact = false;
            exceptions.push(
                "authority_drift",
                "local",
                Some("import"),
                (*id).to_string(),
                archive_doc.title.clone().or_else(|| local_doc.title.clone()),
                format!("archive {} vs local {}", archive_doc.brief(), local_doc.brief()),
            );
        }
        match local_projection.get(id) {
            Some(projection_doc) if !projection_doc.exact_content(local_doc) => {
                is_exact = false;
                exceptions.push(
                    "projection_drift",
                    "local",
                    None,
                    (*id).to_string(),
                    local_doc.title.clone(),
                    format!("local Y.Doc {} vs document.json {}", local_doc.brief(), projection_doc.brief()),
                );
            }
            Some(_) => {}
            None => {
                is_exact = false;
                exceptions.push(
                    "projection_missing",
                    "local",
                    Some("import"),
                    (*id).to_string(),
                    local_doc.title.clone(),
                    "local Y.Doc exists but no document.json record was materialized",
                );
            }
        }
        if let (Some(archive_title), Some(Some(local_title))) =
            (archive_doc.title.as_deref(), local_catalog_titles.get(id))
        {
            if archive_title != *local_title {
                exceptions.push(
                    "title_delta",
                    "local",
                    Some("import"),
                    (*id).to_string(),
                    Some(archive_title.to_string()),
                    format!(
                        "archive title ({} chars) became {:?} locally ({} chars)",
                        archive_title.chars().count(),
                        local_title,
                        local_title.chars().count()
                    ),
                );
            }
        }
        if is_exact {
            exact += 1;
        }
    }
    for (id, local_doc) in &local_docs {
        if !archive_docs.contains_key(id) {
            exceptions.push(
                "extra_locally",
                "local",
                None,
                (*id).to_string(),
                local_doc.title.clone(),
                "local Y.Doc not present in the archive (created after import, or import-side artifact)",
            );
        }
    }

    // ── Wires, folders, artifacts, originals, geist: archive → local ──────
    compare_wires(archive, local, "wire_missing_locally", "wire_extra_locally", "local", Some("import"), &mut exceptions);
    wire_hygiene(archive, "archive", &mut exceptions);
    wire_hygiene(local, "local", &mut exceptions);
    compare_folders(
        archive,
        local,
        "folder_missing_locally",
        "folder_extra_locally",
        "folder_changed_locally",
        "local",
        Some("import"),
        &mut exceptions,
    );
    if archive.artifact_count != local.artifact_count {
        exceptions.push(
            "artifact_count_mismatch",
            "local",
            Some("import"),
            "artifacts",
            None,
            format!("archive catalog {} vs local catalog {}", archive.artifact_count, local.artifact_count),
        );
    }
    let local_originals: BTreeSet<&str> = local.originals.iter().map(String::as_str).collect();
    for path in &archive.originals {
        if !local_originals.contains(path.as_str()) {
            exceptions.push("original_missing_locally", "local", Some("import"), path.clone(), None, "archived original file absent locally");
        }
    }
    let local_geist: BTreeSet<&str> = local.geist.iter().map(String::as_str).collect();
    for path in &archive.geist {
        if !local_geist.contains(path.as_str()) {
            exceptions.push("geist_missing_locally", "local", Some("import"), path.clone(), None, "archived geist authority file absent locally");
        }
    }
    if let (Some(archive_quads), Some(local_quads)) = (archive.rdf_user_quads, local.rdf_user_quads) {
        if archive_quads != local_quads {
            exceptions.push(
                "rdf_user_quads_mismatch",
                "local",
                None,
                "rdf",
                None,
                format!(
                    "archive would load {archive_quads} user quads; local store holds {local_quads} (derived graphs regenerate locally, so small deltas are expected)"
                ),
            );
        }
    }

    // ── Cloud: cloud → archive (export stage) ─────────────────────────────
    let mut cloud_section = Value::Null;
    if let Some(cloud) = cloud {
        let cloud_docs = cloud.document_map();
        let mut export_exact = 0usize;
        let mut end_to_end_exact = 0usize;
        for (id, cloud_doc) in &cloud_docs {
            let drifted = is_after(cloud_doc.updated_at.as_deref(), exported_at);
            match archive_docs.get(id) {
                None => {
                    if cloud_doc.block_count == 0 {
                        // already reported as cloud_ghost
                        continue;
                    }
                    match drifted {
                        Some(true) => exceptions.push(
                            "cloud_drift_new",
                            "cloud",
                            None,
                            (*id).to_string(),
                            cloud_doc.title.clone(),
                            format!("created/updated in cloud after export ({})", cloud_doc.updated_at.as_deref().unwrap_or("?")),
                        ),
                        Some(false) => exceptions.push(
                            "cloud_only_before_export",
                            "archive",
                            Some("export"),
                            (*id).to_string(),
                            cloud_doc.title.clone(),
                            format!("cloud-only observation dated before export ({}, {}); timestamp/RDF alone cannot establish loss at export", cloud_doc.updated_at.as_deref().unwrap_or("?"), cloud_doc.brief()),
                        ),
                        None => exceptions.push(
                            "cloud_only_undated",
                            "archive",
                            Some("export"),
                            (*id).to_string(),
                            cloud_doc.title.clone(),
                            format!("in cloud, not in archive, no usable timestamp to place it ({})", cloud_doc.brief()),
                        ),
                    }
                }
                Some(archive_doc) => {
                    if cloud_doc.same_content(archive_doc) {
                        export_exact += 1;
                        if local_docs.get(id).is_some_and(|local_doc| local_doc.same_content(cloud_doc)) {
                            end_to_end_exact += 1;
                        }
                    } else if drifted == Some(true) {
                        exceptions.push(
                            "cloud_drift_edited",
                            "cloud",
                            None,
                            (*id).to_string(),
                            cloud_doc.title.clone(),
                            format!("edited in cloud after export: cloud {} vs archive {}", cloud_doc.brief(), archive_doc.brief()),
                        );
                    } else {
                        // The cloud fingerprint comes from cloud RDF, which is a
                        // projection of the cloud Y.Doc; the archive IS the cloud
                        // Y.Doc at export time. A mismatch here with no recorded
                        // edit is, in practice, either a cloud-side write that did
                        // not bump `updatedAt` (MCP/Geist writes) or a cloud RDF
                        // projection that lagged its own authority — verified on
                        // the first real run (2026-09-05), where all 15 resolved
                        // that way through document history. Reported for review,
                        // not gating; `export_loss` (document wholly absent) gates.
                        exceptions.push(
                            "export_fidelity_unresolved",
                            "archive",
                            Some("export"),
                            (*id).to_string(),
                            cloud_doc.title.clone(),
                            format!(
                                "cloud RDF {} vs archive Y.Doc {}; cloud updatedAt {} predates the export — check document history (MCP writes do not bump updatedAt; cloud RDF can lag its Y.Doc)",
                                cloud_doc.brief(),
                                archive_doc.brief(),
                                cloud_doc.updated_at.as_deref().unwrap_or("?")
                            ),
                        );
                    }
                }
            }
        }
        for (id, archive_doc) in &archive_docs {
            if !cloud_docs.contains_key(id) {
                exceptions.push(
                    "archive_not_in_cloud",
                    "cloud",
                    None,
                    (*id).to_string(),
                    archive_doc.title.clone(),
                    "archive Y.Doc no longer exists in cloud (deleted since export, or cloud RDF never held it)",
                );
            }
        }
        compare_wires(cloud, archive, "wire_export_loss", "wire_archive_not_in_cloud", "archive", Some("export"), &mut exceptions);
        wire_hygiene(cloud, "cloud", &mut exceptions);
        compare_folders(
            cloud,
            archive,
            "folder_export_loss",
            "folder_archive_not_in_cloud",
            "folder_changed_at_export",
            "archive",
            Some("export"),
            &mut exceptions,
        );
        let cloud_doc_total = cloud_docs.values().filter(|doc| doc.block_count > 0).count();
        cloud_section = json!({
            "documents_in_cloud": cloud.documents.len(),
            "documents_in_cloud_with_blocks": cloud_doc_total,
            "archive_fingerprint_matches": export_exact,
            "three_side_fingerprint_matches": end_to_end_exact,
            "cloud_only_before_export": exceptions.count("cloud_only_before_export"),
            "export_fidelity_unresolved": exceptions.count("export_fidelity_unresolved"),
            "cloud_drift_new": exceptions.count("cloud_drift_new"),
            "cloud_drift_edited": exceptions.count("cloud_drift_edited"),
            "cloud_only_undated": exceptions.count("cloud_only_undated"),
            "cloud_ghosts": exceptions.count("cloud_ghost"),
            "archive_not_in_cloud": exceptions.count("archive_not_in_cloud"),
            "wire_export_loss": exceptions.count("wire_export_loss"),
            "wire_archive_not_in_cloud": exceptions.count("wire_archive_not_in_cloud"),
            "folder_export_loss": exceptions.count("folder_export_loss"),
            "captured_at": cloud.captured_at,
        });
    }

    // ── Summary ───────────────────────────────────────────────────────────
    let documents_in_archive = archive.documents.len();
    let fidelity_percent = if documents_in_archive == 0 {
        100.0
    } else {
        (exact as f64) * 100.0 / (documents_in_archive as f64)
    };
    let missing_locally = exceptions.count("missing_locally");
    let export_loss = exceptions.count("export_loss");
    let needs_review = exceptions.count("export_fidelity_unresolved")
        + exceptions.count("cloud_only_before_export")
        + exceptions.count("cloud_only_undated");
    let mut blocking_reasons: Vec<String> = Vec::new();
    if fidelity_percent < FIDELITY_READY_PERCENT {
        blocking_reasons.push(format!(
            "{fidelity_percent:.2}% of archive documents restored exactly (threshold {FIDELITY_READY_PERCENT}%)"
        ));
    }
    if missing_locally > 0 {
        blocking_reasons.push(format!("{missing_locally} archive document(s) missing locally"));
    }
    // A READY percentage must never swallow a named loss or incomplete evidence.
    let import_blockers = exceptions.0.iter().filter(|item| {
        item.stage == Some("import") || (item.side == "local"
            && !matches!(item.kind, "original_missing_locally" | "artifact_count_mismatch" | "rdf_user_quads_mismatch"))
    }).filter(|item| !matches!(item.kind, "original_missing_locally" | "artifact_count_mismatch"))
        .map(|item| format!("{}: {}", item.kind, item.id)).collect::<Vec<_>>();
    blocking_reasons.extend(import_blockers);
    if archive.documents.is_empty() { blocking_reasons.push("archive has no document authorities; cannot certify this migration".into()); }
    if !archive.warnings.is_empty() || !local.warnings.is_empty() {
        blocking_reasons.push("collection warnings require review before certification".into());
    }
    // Cloud evidence is deliberately independent of the exact archive/local check.
    let import_verified = fidelity_percent == 100.0
        && missing_locally == 0
        && !exceptions.0.iter().any(|item| item.stage == Some("import")
            && !matches!(item.kind, "original_missing_locally" | "artifact_count_mismatch"))
        && !exceptions.0.iter().any(|item| item.side == "local"
            && !matches!(item.kind, "original_missing_locally" | "artifact_count_mismatch" | "rdf_user_quads_mismatch"))
        && archive.evidence.is_some() && local.evidence.is_some()
        && !archive.documents.is_empty() && archive.warnings.is_empty() && local.warnings.is_empty();
    let ready = import_verified;

    let mut kinds: BTreeMap<&str, usize> = BTreeMap::new();
    for item in &exceptions.0 {
        *kinds.entry(item.kind).or_default() += 1;
    }
    let exception_counts: Value = kinds
        .iter()
        .map(|(kind, count)| ((*kind).to_string(), json!(count)))
        .collect::<serde_json::Map<String, Value>>()
        .into();

    let mut ids_by_kind = serde_json::Map::new();
    for kind in kinds.keys() {
        ids_by_kind.insert((*kind).to_string(), json!(exceptions.ids(kind)));
    }
    let exception_values: Vec<Value> = exceptions.0.iter().map(|item| {
        let mut value = serde_json::to_value(item).expect("exception serializes");
        let collection = match item.kind {
            "wire_dangling" | "wire_cross_graph" => Some("wires"),
            "catalog_ghost" => Some("documents"),
            _ => None,
        };
        let preserved = item.side == "local" && collection.is_some_and(|kind| {
            exceptions.0.iter().any(|source| source.side == "archive" && source.kind == item.kind && source.id == item.id)
                && archive.evidence.as_ref().zip(local.evidence.as_ref()).is_some_and(|(left, right)| {
                    let a = &left.workspace[kind][&item.id];
                    !a.is_null() && a == &right.workspace[kind][&item.id]
                })
        });
        value["preserved_source_issue"] = json!(preserved);
        value
    }).collect();

    json!({
        "verification_version": 2,
        "coverage": {
            "documents": "canonical TipTap JSON SHA-256 (text, order, marks, attributes) plus comments map",
            "workspace": "catalog placement/order, folders, wire endpoints/anchors/predicates",
            "geist": "cloud-1 Geist documents and source backup preserved; translated native Song, memory and scoring config compared; runtime behavior requires separate rehearsal",
            "originals": "presence advisory only; content verification deferred by migration scope",
            "cloud": "later RDF observations are diagnostic, never proof of export completeness",
            "limits": ["user RDF content and cloud valuation projection not certified", "unknown document Y.Doc roots are not certified", "requires a stable offline baseline"]
        },
        "summary": {
            "import_verified": import_verified,
            "export_verified": false,
            "cutover_verified": false,
            "verdict": if import_verified { "IMPORT_VERIFIED" } else { "BLOCKED" },
            "source_valuation_records": archive.evidence.as_ref().map(|e| e.legacy_valuation_records),
            "documents_in_archive": documents_in_archive,
            "documents_local": local.documents.len(),
            "documents_exact": exact,
            "fidelity_percent": (fidelity_percent * 100.0).round() / 100.0,
            "ready_threshold_percent": FIDELITY_READY_PERCENT,
            "ready": ready,
            "blocking_reasons": blocking_reasons,
            "missing_locally": missing_locally,
            "export_loss": export_loss,
            "needs_review": needs_review,
            "exception_total": exceptions.0.len(),
            "exception_counts": exception_counts,
        },
        "cloud": cloud_section,
        "sides": {
            "archive": archive.counts(),
            "local": local.counts(),
            "cloud": cloud.map(SideSnapshot::counts),
        },
        "exception_ids": Value::Object(ids_by_kind),
        "exceptions": exception_values,
        "warnings": {
            "archive": archive.warnings,
            "local": local.warnings,
            "cloud": cloud.map(|side| side.warnings.clone()).unwrap_or_default(),
        },
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Markdown
// ─────────────────────────────────────────────────────────────────────────────

fn markdown_ids(ids: &[Value]) -> String {
    let shown: Vec<String> = ids
        .iter()
        .take(EXCEPTION_ID_PREVIEW)
        .filter_map(Value::as_str)
        .map(|id| format!("`{id}`"))
        .collect();
    let mut line = shown.join(", ");
    if ids.len() > EXCEPTION_ID_PREVIEW {
        line.push_str(&format!(" … and {} more", ids.len() - EXCEPTION_ID_PREVIEW));
    }
    line
}

pub(crate) fn render_markdown(report: &Value, graph_id: &str, identity: &ArchiveIdentity) -> String {
    let summary = &report["summary"];
    let mut out = String::new();
    out.push_str(&format!("# Fidelity report — `{graph_id}`\n\n"));
    out.push_str(&format!(
        "Archive: `{}` (user `{}`), exported {}; {} bytes.\n\n",
        identity.source_graph_id,
        identity.source_user_id,
        identity.exported_at.as_deref().unwrap_or("unknown"),
        identity.archive_size_bytes
    ));
    let verdict = if summary["import_verified"].as_bool().unwrap_or(false) { "IMPORT_VERIFIED" } else { "BLOCKED" };
    out.push_str(&format!(
        "## Verdict: {verdict} — {}% document-content matches ({} of {} archive documents; threshold {}%)\n\n",
        summary["fidelity_percent"], summary["documents_exact"], summary["documents_in_archive"], summary["ready_threshold_percent"]
    ));
    if let Some(reasons) = summary["blocking_reasons"].as_array().filter(|list| !list.is_empty()) {
        for reason in reasons.iter().filter_map(Value::as_str) {
            out.push_str(&format!("- BLOCKING: {reason}\n"));
        }
        out.push('\n');
    }
    if summary["needs_review"].as_u64().unwrap_or(0) > 0 {
        out.push_str(&format!(
            "{} cloud observation(s) remain unresolved. Export completeness is not certified by this report.\n\n",
            summary["needs_review"]
        ));
    }
    out.push_str("This verdict covers archive-to-local preservation only. It does not authorize cutover; cloud export completeness, runtime Geist behavior, and excluded data require separate evidence.\n\n");
    out.push_str("| side | catalog | Y.Docs | wires | folders | artifacts | originals | geist | RDF user quads |\n|---|---|---|---|---|---|---|---|---|\n");
    for side in ["cloud", "archive", "local"] {
        let counts = &report["sides"][side];
        if counts.is_null() {
            continue;
        }
        out.push_str(&format!(
            "| {side} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
            counts["catalog"], counts["documents"], counts["wires"], counts["folders"], counts["artifacts"], counts["originals"], counts["geist"], counts["rdf_user_quads"]
        ));
    }
    out.push('\n');
    if !report["cloud"].is_null() {
        let cloud = &report["cloud"];
        out.push_str(&format!(
            "## Later cloud observation (diagnostic only)\n\n{} cloud documents carry blocks; {} share the archive's aggregate fingerprint; {} share all three aggregate fingerprints. These are not exact-content proofs.\nCloud-only with pre-export timestamps (unresolved): {} · unresolved fingerprint differences: {} · post-export timestamp hints: {} cloud-only, {} changed · undated cloud-only: {} · no blocks in cloud RDF: {} · archive docs absent from cloud observation: {} · cloud-only wires: {} · cloud-only folders: {}\n\n",
            cloud["documents_in_cloud_with_blocks"], cloud["archive_fingerprint_matches"], cloud["three_side_fingerprint_matches"], cloud["cloud_only_before_export"], cloud["export_fidelity_unresolved"], cloud["cloud_drift_new"], cloud["cloud_drift_edited"], cloud["cloud_only_undated"], cloud["cloud_ghosts"], cloud["archive_not_in_cloud"], cloud["wire_export_loss"], cloud["folder_export_loss"]
        ));
    }
    out.push_str("## Exceptions by kind\n\n");
    let counts = summary["exception_counts"].as_object().cloned().unwrap_or_default();
    if counts.is_empty() {
        out.push_str("None. Every archive document restored exactly and every structure matched.\n\n");
    }
    for (kind, count) in counts {
        let ids = report["exception_ids"][&kind].as_array().cloned().unwrap_or_default();
        let sample = report["exceptions"]
            .as_array()
            .and_then(|items| items.iter().find(|item| item["kind"] == kind));
        let side = sample.and_then(|item| item["side"].as_str()).unwrap_or("?");
        let stage = sample
            .and_then(|item| item["stage"].as_str())
            .map(|stage| format!(", stage {stage}"))
            .unwrap_or_default();
        out.push_str(&format!("### {kind} — {count} (side {side}{stage})\n\n{}\n\n", markdown_ids(&ids)));
    }
    let warnings = &report["warnings"];
    for side in ["archive", "local", "cloud"] {
        if let Some(list) = warnings[side].as_array().filter(|list| !list.is_empty()) {
            out.push_str(&format!("## {side} warnings\n\n"));
            for warning in list.iter().filter_map(Value::as_str) {
                out.push_str(&format!("- {warning}\n"));
            }
            out.push('\n');
        }
    }
    out
}

// ─────────────────────────────────────────────────────────────────────────────
// Entry point
// ─────────────────────────────────────────────────────────────────────────────

/// Build the full report for `graph_id` against `archive_bytes`, with an
/// optional cloud snapshot (see `docs/fidelity-report.md` for its schema).
pub(crate) fn build_fidelity_report(
    app: &AppHandle,
    graph_id: &str,
    archive_bytes: &[u8],
    cloud: Option<SideSnapshot>,
) -> Result<Value, String> {
    let (archive, identity) = archive_side(archive_bytes, graph_id)?;
    if let Some(snapshot) = &cloud {
        if snapshot.graph_id != identity.source_graph_id {
            return Err("cloud evidence graph_id does not match archive source identity".into());
        }
    }
    let mut local = local_side(app, graph_id)?;
    let again = local_side(app, graph_id)?;
    if stable_side_hash(&local) != stable_side_hash(&again) {
        local.warnings.push("local evidence changed during collection; repeat against a stable copy".into());
    }
    let mut report = compare(&archive, &local, cloud.as_ref(), identity.exported_at.as_deref());
    let markdown = render_markdown(&report, graph_id, &identity);
    if let Some(object) = report.as_object_mut() {
        object.insert("graph_id".to_string(), json!(graph_id));
        object.insert("archive_sha256".to_string(), json!(hex_digest(archive_bytes)));
        object.insert("local_evidence_sha256".to_string(), json!(stable_side_hash(&local)));
        object.insert("generated_at".to_string(), json!(crate::clock::timestamp()));
        object.insert(
            "archive".to_string(),
            serde_json::to_value(&identity).map_err(|error| format!("serialize archive identity: {error}"))?,
        );
        object.insert("markdown".to_string(), json!(markdown));
    }
    Ok(report)
}

fn stable_side_hash(side: &SideSnapshot) -> String {
    let mut value = serde_json::to_value(side).expect("snapshot serializes");
    value.as_object_mut().unwrap().remove("captured_at");
    value_hash(&value)
}

pub fn migration_archive_evidence(archive_path: &Path) -> Result<Value, String> {
    let bytes = read_bytes(archive_path)?;
    let (snapshot, identity) = archive_side(&bytes, "verification-target")?;
    Ok(json!({"verification_version":2,"observation_only":true,
        "archive_sha256":hex_digest(&bytes),"archive":identity,"snapshot":snapshot}))
}

/// Disk-only verification: no app bootstrap, flush, hydration, or RDF self-heal.
pub fn verify_migration_files(graph_dir: &Path, graph_id: &str, archive_path: &Path) -> Result<Value, String> {
    let bytes = read_bytes(archive_path)?;
    let (archive, identity) = archive_side(&bytes, graph_id)?;
    let mut local = local_side_from_dir(graph_dir, graph_id)?;
    let again = local_side_from_dir(graph_dir, graph_id)?;
    if stable_side_hash(&local) != stable_side_hash(&again) {
        local.warnings.push("local evidence changed during collection; repeat against a stable copy".into());
    }
    let mut report = compare(&archive, &local, None, identity.exported_at.as_deref());
    report["archive_sha256"] = json!(hex_digest(&bytes));
    report["archive_observation"] = json!({"verification_version":2,"observation_only":true,
        "archive_sha256":hex_digest(&bytes),"archive":identity,"snapshot":archive});
    report["archive"] = serde_json::to_value(&identity).expect("identity serializes");
    report["generated_at"] = json!(crate::clock::timestamp());
    report["graph_id"] = json!(graph_id);
    report["local_evidence_sha256"] = json!(stable_side_hash(&local));
    report["markdown"] = json!(render_markdown(&report, graph_id, &identity));
    Ok(report)
}

pub(crate) fn load_cloud_snapshot(path: &Path) -> Result<SideSnapshot, String> {
    let bytes = read_bytes(path)?;
    let mut snapshot: SideSnapshot = serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse cloud snapshot {}: {error}", path.display()))?;
    if snapshot.side.is_empty() {
        snapshot.side = "cloud".to_string();
    }
    Ok(snapshot)
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn fp(id: &str, blocks: usize, hash: Option<&str>, text: u64, updated: Option<&str>) -> DocFingerprint {
        DocFingerprint {
            id: id.to_string(),
            title: Some(format!("Title {id}")),
            block_count: blocks,
            block_id_hash: hash.map(str::to_string),
            text_length: text,
            updated_at: updated.map(str::to_string),
            content_sha256: Some(format!("{blocks}:{hash:?}:{text}")),
            comments_sha256: Some("empty-comments".into()),
            unverified_roots: Vec::new(),
        }
    }

    fn side(label: &str, docs: Vec<DocFingerprint>) -> SideSnapshot {
        SideSnapshot {
            side: label.to_string(),
            graph_id: "g".to_string(),
            catalog: docs
                .iter()
                .map(|doc| CatalogEntry {
                    id: doc.id.clone(),
                    title: doc.title.clone(),
                })
                .collect(),
            projection: if label == "local" { docs.clone() } else { Vec::new() },
            documents: docs,
            evidence: Some(ImportEvidence { workspace: json!({"documents":{},"folders":{},"wires":{}}), ..Default::default() }),
            ..SideSnapshot::default()
        }
    }

    fn kinds(report: &Value) -> BTreeMap<String, usize> {
        report["summary"]["exception_counts"]
            .as_object()
            .map(|map| {
                map.iter()
                    .map(|(k, v)| (k.clone(), v.as_u64().unwrap_or(0) as usize))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn fingerprint_uses_sorted_distinct_block_ids_and_code_point_text_length() {
        let xml = r#"<paragraph data-block-id="b">x</paragraph><paragraph data-block-id="a">y</paragraph><paragraph data-block-id="a">z</paragraph>"#;
        let json = json!({"type":"doc","content":[{"type":"paragraph","content":[{"type":"text","text":"héllo"},{"type":"text","text":"wörld"}]}]});
        let fingerprint = fingerprint_from_projection("d", Some("T"), xml, &json);
        assert_eq!(fingerprint.block_count, 2);
        assert_eq!(fingerprint.block_id_hash.as_deref(), Some(hex_digest(b"a,b").as_str()));
        assert_eq!(fingerprint.text_length, 10);
        let empty = fingerprint_from_projection("e", None, "", &Value::Null);
        assert_eq!(empty.block_count, 0);
        assert!(empty.block_id_hash.is_none());
    }

    #[test]
    fn nquad_graph_term_parses_literals_with_spaces_and_default_graph() {
        assert_eq!(
            nquad_graph_iri(r#"<urn:s> <urn:p> "a . literal with <angle> and \" quote"@en <urn:g> ."#).unwrap(),
            Some("urn:g".to_string())
        );
        assert_eq!(
            nquad_graph_iri(r#"<urn:s> <urn:p> "typed"^^<http://www.w3.org/2001/XMLSchema#string> ."#).unwrap(),
            None
        );
        assert_eq!(nquad_graph_iri("_:b0 <urn:p> <urn:o> <urn:g> .").unwrap(), Some("urn:g".to_string()));
        assert!(nquad_graph_iri("garbage").is_err());
    }

    #[test]
    fn exact_restoration_is_ready_with_no_exceptions() {
        let docs = vec![fp("a", 3, Some("h1"), 10, None), fp("b", 1, Some("h2"), 4, None)];
        let archive = side("archive", docs.clone());
        let local = side("local", docs);
        let report = compare(&archive, &local, None, None);
        assert_eq!(report["summary"]["fidelity_percent"], 100.0);
        assert_eq!(report["summary"]["ready"], true);
        assert_eq!(report["summary"]["exception_total"], 0);
    }

    #[test]
    fn ghosts_and_orphans_are_attributed_to_their_side() {
        let mut archive = side("archive", vec![fp("a", 1, Some("h"), 1, None)]);
        archive.catalog.push(CatalogEntry { id: "ghost".into(), title: Some("Ghost".into()) });
        let mut local = side("local", vec![fp("a", 1, Some("h"), 1, None), fp("orphan", 2, Some("o"), 2, None)]);
        local.catalog.retain(|entry| entry.id != "orphan");
        let report = compare(&archive, &local, None, None);
        let items = report["exceptions"].as_array().unwrap();
        let ghost = items.iter().find(|item| item["kind"] == "catalog_ghost").unwrap();
        assert_eq!(ghost["side"], "archive");
        assert_eq!(ghost["id"], "ghost");
        let orphan = items.iter().find(|item| item["kind"] == "catalog_orphan").unwrap();
        assert_eq!(orphan["side"], "local");
        assert_eq!(orphan["id"], "orphan");
        // The orphan is also "extra locally" relative to the archive.
        assert_eq!(kinds(&report)["extra_locally"], 1);
        assert_eq!(report["summary"]["ready"], false, "local catalog inconsistency blocks certification");
    }

    #[test]
    fn drift_missing_and_title_clips_block_readiness_and_name_the_import_stage() {
        let archive = side(
            "archive",
            vec![fp("a", 3, Some("h1"), 10, None), fp("b", 1, Some("h2"), 4, None), fp("c", 1, Some("h3"), 4, None)],
        );
        let mut local = side("local", vec![fp("a", 3, Some("h1"), 10, None), fp("b", 2, Some("h2x"), 9, None)]);
        // Projection drift on "a": document.json disagrees with the Y.Doc.
        local.projection[0].content_sha256 = Some("changed".into());
        // Title clip on "b".
        local.catalog[1].title = Some("Title b clipped".into());
        let report = compare(&archive, &local, None, None);
        let counts = kinds(&report);
        assert_eq!(counts["authority_drift"], 1);
        assert_eq!(counts["projection_drift"], 1);
        assert_eq!(counts["missing_locally"], 1);
        assert_eq!(counts["title_delta"], 1);
        assert_eq!(report["summary"]["documents_exact"], 0);
        assert_eq!(report["summary"]["ready"], false);
        let missing = report["exceptions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["kind"] == "missing_locally")
            .unwrap();
        assert_eq!(missing["stage"], "import");
        assert_eq!(missing["id"], "c");
    }

    #[test]
    fn cloud_side_separates_export_loss_from_drift_after_export() {
        let exported_at = "2026-08-25T15:45:55+00:00";
        let archive = side("archive", vec![fp("kept", 2, Some("k"), 5, None), fp("edited", 2, Some("e1"), 5, None)]);
        let local = side("local", vec![fp("kept", 2, Some("k"), 5, None), fp("edited", 2, Some("e1"), 5, None)]);
        let cloud = SideSnapshot {
            side: "cloud".into(),
            graph_id: "default".into(),
            documents: vec![
                fp("kept", 2, Some("k"), 5, Some("2026-08-01T00:00:00Z")),
                fp("edited", 3, Some("e2"), 9, Some("2026-09-01T00:00:00Z")),
                fp("lost", 4, Some("l"), 7, Some("2026-07-01T00:00:00Z")),
                fp("new", 1, Some("n"), 1, Some("2026-09-02T00:00:00Z")),
                fp("ghost", 0, None, 0, None),
                fp("undated", 1, Some("u"), 1, None),
            ],
            ..SideSnapshot::default()
        };
        let report = compare(&archive, &local, Some(&cloud), Some(exported_at));
        let counts = kinds(&report);
        assert_eq!(counts["cloud_only_before_export"], 1, "{counts:?}");
        assert_eq!(counts["cloud_drift_edited"], 1);
        assert_eq!(counts["cloud_drift_new"], 1);
        assert_eq!(counts["cloud_ghost"], 1);
        assert_eq!(counts["cloud_only_undated"], 1);
        assert!(!counts.contains_key("export_fidelity_unresolved"));
        assert_eq!(report["cloud"]["archive_fingerprint_matches"], 1);
        assert_eq!(report["cloud"]["three_side_fingerprint_matches"], 1);
        assert_eq!(report["summary"]["import_verified"], true, "import verification is separate from export evidence");
        assert_eq!(report["summary"]["cutover_verified"], false);
        assert_eq!(report["summary"]["needs_review"], 2);
        assert_eq!(report["summary"]["blocking_reasons"].as_array().unwrap().len(), 0);

        // A fingerprint mismatch with a pre-export timestamp is review, not a gate.
        let cloud = SideSnapshot {
            side: "cloud".into(),
            graph_id: "default".into(),
            documents: vec![
                fp("kept", 9, Some("stale-rdf"), 5, Some("2026-08-01T00:00:00Z")),
                fp("edited", 2, Some("e1"), 5, Some("2026-08-01T00:00:00Z")),
            ],
            ..SideSnapshot::default()
        };
        let review = compare(&archive, &local, Some(&cloud), Some(exported_at));
        let review_counts = kinds(&review);
        assert_eq!(review_counts["export_fidelity_unresolved"], 1);
        assert_eq!(review["summary"]["ready"], true, "{}", review["summary"]);
        assert_eq!(review["summary"]["needs_review"], 1);
        let loss = report["exceptions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["kind"] == "cloud_only_before_export")
            .unwrap();
        assert_eq!(loss["side"], "archive");
        assert_eq!(loss["stage"], "export");
        assert_eq!(loss["id"], "lost");
    }

    #[test]
    fn wires_report_cross_graph_anomalies_dangling_endpoints_and_import_loss() {
        let wire = |id: &str, src: &str, tgt: &str, graph: Option<&str>| WireEntry {
            id: id.into(),
            source_document: src.into(),
            target_document: tgt.into(),
            predicate: Some("relatedTo".into()),
            target_graph: graph.map(str::to_string),
        };
        let mut archive = side("archive", vec![fp("a", 1, Some("h"), 1, None), fp("b", 1, Some("h"), 1, None)]);
        archive.wires = vec![wire("w1", "a", "b", None), wire("w2", "a", "b", None), wire("w3", "a", "b", Some("other"))];
        let mut local = side("local", vec![fp("a", 1, Some("h"), 1, None), fp("b", 1, Some("h"), 1, None)]);
        local.wires = vec![wire("w1", "a", "b", None), wire("w3", "a", "b", Some("other")), wire("w4", "a", "gone", None)];
        let report = compare(&archive, &local, None, None);
        let counts = kinds(&report);
        assert_eq!(counts["wire_missing_locally"], 1);
        assert_eq!(counts["wire_extra_locally"], 1);
        assert_eq!(counts["wire_cross_graph"], 2, "one per side");
        assert_eq!(counts["wire_dangling"], 1);
        assert_eq!(report["summary"]["ready"], false, "wire loss blocks certification");
    }

    #[test]
    fn markdown_carries_verdict_and_kinds() {
        let archive = side("archive", vec![fp("a", 1, Some("h"), 1, None)]);
        let local = side("local", Vec::new());
        let report = compare(&archive, &local, None, None);
        let identity = ArchiveIdentity {
            source_graph_id: "default".into(),
            source_user_id: "u".into(),
            source_graph_title: None,
            exported_at: Some("2026-08-25T15:45:55+00:00".into()),
            includes_artifacts: false,
            archive_size_bytes: 1,
        };
        let markdown = render_markdown(&report, "g", &identity);
        assert!(markdown.contains("BLOCKED"));
        assert!(markdown.contains("### missing_locally — 1 (side local, stage import)"));
        assert!(markdown.contains("`a`"));
    }

    #[test]
    fn equal_length_changes_order_and_marks_are_not_exact() {
        let original = json!({"type":"doc","content":[
            {"type":"paragraph","attrs":{"id":"a"},"content":[{"type":"text","text":"love"}]},
            {"type":"paragraph","attrs":{"id":"b"},"content":[{"type":"text","text":"hold"}]}
        ]});
        let xml = r#"<paragraph data-block-id="a"/><paragraph data-block-id="b"/>"#;
        let fingerprint = fingerprint_from_projection("doc", None, xml, &original);
        let mut changed = original.clone();
        changed["content"][0]["content"][0]["text"] = json!("hate");
        let mut reordered = original.clone();
        reordered["content"].as_array_mut().unwrap().reverse();
        let mut marked = original.clone();
        marked["content"][0]["content"][0]["marks"] = json!([{"type":"bold"}]);
        for mutation in [changed, reordered, marked] {
            let other = fingerprint_from_projection("doc", None, xml, &mutation);
            assert!(fingerprint.same_content(&other), "legacy heuristic misses this mutation");
            assert!(!fingerprint.exact_content(&other));
        }
    }

    #[test]
    fn mark_set_order_does_not_create_false_drift() {
        let left = json!({"type":"text","text":"same","marks":[{"type":"bold"},{"type":"italic"}]});
        let right = json!({"marks":[{"type":"italic"},{"type":"bold"}],"text":"same","type":"text"});
        assert_eq!(canonical_tiptap(&left), canonical_tiptap(&right));
        let mut missing = right;
        missing["marks"].as_array_mut().unwrap().pop();
        assert_ne!(canonical_tiptap(&left), canonical_tiptap(&missing));
    }

    #[test]
    fn one_bad_document_in_a_hundred_blocks_verification() {
        let docs = (0..100).map(|n| fp(&n.to_string(), 1, Some("id"), 4, None)).collect::<Vec<_>>();
        let archive = side("archive", docs.clone());
        let mut local = side("local", docs);
        local.documents[0].content_sha256 = Some("changed".into());
        let report = compare(&archive, &local, None, None);
        assert_eq!(report["summary"]["fidelity_percent"], 99.0);
        assert_eq!(report["summary"]["import_verified"], false);
    }

    #[test]
    fn rewired_existing_id_and_lost_placement_are_detected() {
        let mut archive = side("archive", vec![fp("a", 1, Some("h"), 4, None)]);
        archive.evidence.as_mut().unwrap().workspace = workspace_evidence(&json!({
            "documents":[{"id":"a","title":"A","parentId":"folder-a"}],
            "folders":[],
            "wires":[{"id":"w","sourceDocumentId":"a","targetDocumentId":"a","sourceBlockId":"b","predicate":"supports","targetGraphId":"g"}]
        }), "g");
        let mut local = archive.clone();
        local.side = "local".into();
        local.projection = local.documents.clone();
        for field in ["sourceBlockId", "predicate", "targetDocumentId"] {
            let mut changed = local.clone();
            changed.evidence.as_mut().unwrap().workspace["wires"]["w"][field] = json!("other");
            assert_eq!(compare(&archive, &changed, None, None)["summary"]["import_verified"], false);
        }
        local.evidence.as_mut().unwrap().workspace["documents"]["a"]["parentId"] = Value::Null;
        assert_eq!(compare(&archive, &local, None, None)["summary"]["import_verified"], false);
    }

    #[test]
    fn incomplete_evidence_valuations_and_comments_fail_closed() {
        let docs = vec![fp("a", 1, Some("h"), 4, None)];
        let archive = side("archive", docs.clone());
        let local = side("local", docs);
        let mut incomplete = local.clone();
        incomplete.evidence = None;
        assert_eq!(compare(&archive, &incomplete, None, None)["summary"]["import_verified"], false);
        let mut comments = local.clone();
        comments.documents[0].comments_sha256 = Some("different".into());
        assert_eq!(compare(&archive, &comments, None, None)["summary"]["import_verified"], false);
        let mut valuations = archive.clone();
        valuations.evidence.as_mut().unwrap().legacy_valuation_records = 2;
        let report = compare(&valuations, &local, None, None);
        assert_eq!(report["summary"]["source_valuation_records"], 2);
        assert_eq!(report["summary"]["import_verified"], false);
        assert_eq!(compare(&side("archive", vec![]), &side("local", vec![]), None, None)["summary"]["import_verified"], false);
    }

    #[test]
    fn valuation_oracle_checks_exact_counts_and_manual_zero() {
        let mut archive = side("archive", vec![fp("a", 1, Some("h"), 4, None)]);
        let mut local = side("local", archive.documents.clone());
        let fields = json!({"importanceCount":9_007_199_254_740_993_u64,"userImportance":0.0});
        archive.evidence.as_mut().unwrap().legacy_valuation_records = 1;
        archive.evidence.as_mut().unwrap().legacy_values.insert("a:b".into(), fields.clone());
        local.evidence.as_mut().unwrap().legacy_values.insert("a:b".into(), fields);
        assert_eq!(compare(&archive, &local, None, None)["summary"]["import_verified"], true);
        local.evidence.as_mut().unwrap().legacy_values.get_mut("a:b").unwrap()["importanceCount"] = json!(9_007_199_254_740_992_u64);
        assert_eq!(compare(&archive, &local, None, None)["summary"]["import_verified"], false);
        local.evidence.as_mut().unwrap().legacy_values = archive.evidence.as_ref().unwrap().legacy_values.clone();
        local.evidence.as_mut().unwrap().legacy_values.get_mut("a:b").unwrap()["userImportance"] = Value::Null;
        assert_eq!(compare(&archive, &local, None, None)["summary"]["import_verified"], false);
    }

    #[test]
    fn unresolved_source_rows_must_still_survive_in_quarantine() {
        let mut archive = side("archive", vec![fp("a", 1, Some("h"), 4, None)]);
        let mut local = side("local", archive.documents.clone());
        archive.evidence.as_mut().unwrap().legacy_valuation_records = 1;
        archive.evidence.as_mut().unwrap().source_valuation_hashes.insert("<urn:unlinked>".into(), "source-quads".into());
        local.evidence.as_mut().unwrap().quarantined_valuation_hashes.insert("<urn:unlinked>".into(), "source-quads".into());
        let preserved = compare(&archive, &local, None, None);
        assert_eq!(preserved["summary"]["import_verified"], false);
        assert!(!kinds(&preserved).contains_key("valuation_quarantine_missing"));
        local.evidence.as_mut().unwrap().quarantined_valuation_hashes.clear();
        assert_eq!(kinds(&compare(&archive, &local, None, None))["valuation_quarantine_missing"], 1);
    }

    #[test]
    fn oracle_refuses_lossy_partial_representations() {
        use oxigraph::model::{Literal, NamedNode, Term};
        let root = "urn:mnemosyne:user:u:graph:g";
        let mut fields = BTreeMap::from([("http://mnemosyne.dev/doc#blockRef".into(),
            Term::NamedNode(NamedNode::new(format!("{root}:doc:d#block-b")).unwrap()))]);
        let subject = format!("<{root}:valuation:d:b>");
        assert!(source_value_records(root, &BTreeMap::from([(subject.clone(), fields.clone())])).is_ok());
        fields.insert("http://mnemosyne.dev/doc#extraEvidence".into(), Term::Literal(Literal::new_simple_literal("keep")));
        assert!(source_value_records(root, &BTreeMap::from([(subject.clone(), fields.clone())])).is_err());
        fields.remove("http://mnemosyne.dev/doc#extraEvidence");
        fields.insert("http://mnemosyne.dev/doc#rawImportanceSum".into(), Term::Literal(Literal::new_simple_literal("3")));
        assert!(source_value_records(root, &BTreeMap::from([(subject, fields)])).is_err());
    }

    #[test]
    fn preserved_geist_documents_do_not_prove_native_translation() {
        let mut archive = side("archive", vec![fp("geist-song", 1, Some("h"), 4, None)]);
        let mut local = side("local", archive.documents.clone());
        archive.evidence.as_mut().unwrap().legacy_geist_hashes.insert("narrative/song.json".into(), "expected".into());
        let missing = compare(&archive, &local, None, None);
        assert_eq!(missing["summary"]["import_verified"], false);
        assert_eq!(kinds(&missing)["geist_translation_unverified"], 1);
        local.evidence.as_mut().unwrap().legacy_geist_hashes = archive.evidence.as_ref().unwrap().legacy_geist_hashes.clone();
        assert_eq!(compare(&archive, &local, None, None)["summary"]["import_verified"], true);
        local.evidence.as_mut().unwrap().legacy_geist_hashes.insert("narrative/song.json".into(), "changed".into());
        assert_eq!(compare(&archive, &local, None, None)["summary"]["import_verified"], false);
    }

    #[test]
    fn native_geist_and_chronicle_contents_are_required_but_originals_are_advisory() {
        let mut archive = side("archive", vec![fp("a", 1, Some("h"), 4, None)]);
        let mut local = side("local", archive.documents.clone());
        archive.originals.push("documents/a/original/unused.pdf".into());
        assert_eq!(compare(&archive, &local, None, None)["summary"]["import_verified"], true);
        archive.evidence.as_mut().unwrap().geist_hashes.insert("memory/state.json".into(), "hash".into());
        local.geist.push("memory/state.json".into());
        assert_eq!(compare(&archive, &local, None, None)["summary"]["import_verified"], false);
        local.evidence.as_mut().unwrap().geist_hashes = archive.evidence.as_ref().unwrap().geist_hashes.clone();
        archive.evidence.as_mut().unwrap().chronicle.insert("a".into(), json!({"revision":3}));
        local.evidence.as_mut().unwrap().chronicle.insert("a".into(), json!({"revision":2}));
        assert_eq!(compare(&archive, &local, None, None)["summary"]["import_verified"], false);
    }
}

#[cfg(all(test, feature = "headless", not(feature = "desktop")))]
mod round_trip_tests {
    use super::*;
    use crate::crdt_operation_types::EnqueueCrdtOperationInput;
    use crate::crdt_queue::enqueue_crdt_operation;
    use crate::graph_service::{create_graph_service, CreateGraphInput};
    use crate::storage::{create_dir_all, display_path};
    use serde_json::json;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_profile(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("garden-fidelity-{name}-{nanos}"))
    }

    fn write_doc(app: &AppHandle, graph_id: &str, doc_id: &str, markdown: &str) {
        crate::app_runtime::async_runtime::block_on(enqueue_crdt_operation(
            app.clone(),
            EnqueueCrdtOperationInput {
                kind: "document.write".to_string(),
                graph_id: graph_id.to_string(),
                document_id: Some(doc_id.to_string()),
                payload: json!({
                    "documentId": doc_id,
                    "content": markdown,
                    "format": "markdown",
                    "title": "Fidelity Doc",
                }),
            },
        ))
        .expect("document.write drains through the real CRDT engine");
    }

    #[test]
    fn export_import_report_is_exact_then_detects_local_drift() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let profile = temp_profile("round-trip");
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);

        create_graph_service(
            &app,
            CreateGraphInput {
                graph_id: Some("fid-src".to_string()),
                title: "Fidelity Source".to_string(),
                description: None,
                operation_id: None,
            },
        )
        .expect("create source graph");
        write_doc(&app, "fid-src", "fid-doc", "# Fidelity\n\nThe aquifer holds the snowmelt.\n\n- one\n- two");
        write_doc(&app, "fid-src", "fid-doc-2", "Second document, one paragraph.");

        let built = crate::graph_archive_export::build_graph_archive(&app, "fid-src", true)
            .expect("build archive");
        let bytes = std::fs::read(&built.archive_path).expect("read archive");

        let pending_dir = profile.join("pending-uploads");
        create_dir_all(&pending_dir).expect("pending dir");
        let staged = pending_dir.join(&built.archive_file);
        std::fs::copy(&built.archive_path, &staged).expect("stage archive");
        crate::app_runtime::async_runtime::block_on(enqueue_crdt_operation(
            app.clone(),
            EnqueueCrdtOperationInput {
                kind: "graph.importArchive".to_string(),
                graph_id: "fid-dst".to_string(),
                document_id: None,
                payload: json!({
                    "filename": built.archive_file,
                    "newGraphId": "fid-dst",
                    "newTitle": "Fidelity Restored",
                    "pendingArchivePath": display_path(&staged),
                    "sizeBytes": built.archive_size_bytes,
                }),
            },
        ))
        .expect("import archive");

        let report = build_fidelity_report(&app, "fid-dst", &bytes, None).expect("report");
        assert_eq!(report["summary"]["documents_in_archive"], 2);
        assert_eq!(report["summary"]["documents_exact"], 2, "{}", report["markdown"]);
        assert_eq!(report["summary"]["fidelity_percent"], 100.0);
        assert_eq!(report["summary"]["ready"], true, "{}", report["markdown"]);
        assert_eq!(report["sides"]["archive"]["geist"], report["sides"]["local"]["geist"]);
        assert!(report["markdown"].as_str().unwrap().contains("IMPORT_VERIFIED"));

        // Edit the restored copy: the authority now drifts from the archive.
        write_doc(&app, "fid-dst", "fid-doc", "# Fidelity\n\nThe aquifer is dry.");
        let report = build_fidelity_report(&app, "fid-dst", &bytes, None).expect("report after edit");
        assert_eq!(report["summary"]["documents_exact"], 1, "{}", report["markdown"]);
        assert_eq!(report["summary"]["exception_counts"]["authority_drift"], 1);
        assert_eq!(report["summary"]["ready"], false);

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
    }
}
