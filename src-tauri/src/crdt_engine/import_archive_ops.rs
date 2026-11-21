//! graph.importArchive — whole-graph tar.gz import for headless cells.
//! Port of frontend/src/native/native-local-runtime.ts importGraphArchive
//! (~2456-2649) + frontend/src/native/graph-archive-import.ts (all of it).
//!
//! Layering:
//! - the pure layer (`parse_graph_archive`, `rewrite_graph_archive_rdf`,
//!   `rewrite_graph_archive_workspace`) ports graph-archive-import.ts —
//!   gunzip + manual ustar reader (identical limits, path-safety rules and
//!   error strings to the TS readTarEntries), manifest validation, and the
//!   source→target identity rewrites for RDF and the workspace Y.Doc;
//! - `apply` orchestrates the import exactly like importGraphArchive:
//!   completion-ledger replay guard → graph-existence/partial-replay check →
//!   create graph → restore + rewrite workspace Y.Doc (save_workspace with a
//!   materialized snapshot) → per-document Y.Doc restore via the same
//!   create_document/save_document internals the desktop frontend uses →
//!   rewritten n-quads into the graph store → dual snake/camel envelope →
//!   Tier B completion-ledger append.
//!
//! Rooms note: imported document/workspace Y.Docs are written straight to
//! disk. The graph is freshly created, so the RoomRegistry holds no rooms
//! for it yet — first access hydrates from the files written here.

use super::executor::{ApplyOperationError, ApplyOperationResult};
use crate::app_runtime::AppHandle;
use crate::crdt_queue::CrdtOperation;
use base64::Engine;
use flate2::read::GzDecoder;
use serde_json::{json, Map as JsonMap, Value};
use std::collections::HashMap;
use std::io::Read;
use std::time::Instant;
use yrs::updates::decoder::Decode;
use yrs::{Any, Doc, Map as YMap, MapRef, Out, ReadTxn, StateVector, Transact, Update, WriteTxn};

const TAR_BLOCK_SIZE: usize = 512;
const MAX_ARCHIVE_FILES: usize = 20_000;
const MAX_EXTRACTED_SIZE: usize = 500 * 1024 * 1024;
// Payload limit plus one header and at most one padding block per member,
// and bounded conventional terminal zero padding. This bounds *all* inflated
// bytes, including unsupported members and data after the first zero header.
const MAX_TAR_TRAILING_BYTES: usize = 64 * 1024;
const MAX_TAR_CONTAINER_BYTES: usize = MAX_EXTRACTED_SIZE
    + MAX_ARCHIVE_FILES * TAR_BLOCK_SIZE * 2 + MAX_TAR_TRAILING_BYTES;


fn record_import_phase(app: &AppHandle, operation_id: &str, phase: &str, started: Instant) {
    crate::crdt_queue::record_crdt_phase(app, Some(operation_id), phase, started.elapsed());
}

// ─────────────────────────────────────────────────────────────────────────────
// Pure layer — port of frontend/src/native/graph-archive-import.ts
// ─────────────────────────────────────────────────────────────────────────────

/// Validated manifest identity (manifest.json, version 1,
/// format "mnemosyne-graph-export").
#[derive(Debug, Clone)]
pub struct GraphArchiveManifest {
    pub source_user_id: String,
    pub source_graph_id: String,
    pub source_graph_title: Option<String>,
    pub source_graph_description: Option<String>,
    pub includes_artifacts: bool,
    /// When the exporter built the archive (RFC 3339). Informational for
    /// import; the fidelity report uses it as the cut between cloud drift
    /// after the export and loss at export time.
    pub exported_at: Option<String>,
}

/// Port of ParsedGraphArchive. `documents` preserves tar entry order
/// (the TS Map iteration order).
#[derive(Debug)]
pub struct ParsedGraphArchive {
    pub manifest: GraphArchiveManifest,
    pub rdf_n_quads: String,
    pub workspace_bytes: Option<Vec<u8>>,
    pub documents: Vec<(String, Vec<u8>)>,
    /// Artifact/image/document original files, as graph-dir-relative paths
    /// (`artifacts/{id}/original/{file}`, `images/{id}/original/{file}`,
    /// `documents/{id}/original/{file}`). Additive archive-format extension
    /// (2026-08): absent in older archives, ignored by older importers.
    pub originals: Vec<(String, Vec<u8>)>,
    /// Per-document chronicle entries (`chronicle/documents/{id}.json`).
    /// Additive archive-format extension (2026-09): absent in older archives.
    pub chronicle: Vec<(String, crate::document_chronicle::ChronicleEntry)>,
    pub warnings: Vec<String>,
}

/// Port of parseGraphArchive.
pub fn parse_graph_archive(tar_gz_bytes: &[u8]) -> Result<ParsedGraphArchive, String> {
    require_compressed_archive_size(tar_gz_bytes.len())?;
    let tar_bytes = read_gzip_bounded(tar_gz_bytes, MAX_TAR_CONTAINER_BYTES)?;

    let entries = read_tar_entries(&tar_bytes)?;
    let manifest_bytes = entries_get(&entries, "manifest.json")
        .ok_or_else(|| "Cannot read manifest.json from archive".to_string())?;

    let manifest_value: Value = serde_json::from_str(&String::from_utf8_lossy(manifest_bytes))
        .map_err(|_| "Cannot parse manifest.json from archive".to_string())?;
    let manifest = parse_manifest(&manifest_value)?;

    let files = manifest_value
        .get("files")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let rdf_path = normalize_archive_path(files.get("rdf").and_then(Value::as_str).unwrap_or(""));
    let workspace_path =
        normalize_archive_path(files.get("workspace").and_then(Value::as_str).unwrap_or(""));
    let documents_dir = normalize_archive_prefix(
        files
            .get("documents_dir")
            .and_then(Value::as_str)
            .unwrap_or("crdt/documents/"),
    );

    let rdf_n_quads = if rdf_path.is_empty() {
        String::new()
    } else {
        entries_get(&entries, &rdf_path)
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
            .unwrap_or_default()
    };
    let workspace_bytes = if workspace_path.is_empty() {
        None
    } else {
        entries_get(&entries, &workspace_path).map(<[u8]>::to_vec)
    };

    let mut documents: Vec<(String, Vec<u8>)> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    for (entry_path, bytes) in &entries {
        if !entry_path.starts_with(&documents_dir) || !entry_path.ends_with(".yjs") {
            continue;
        }
        let filename = entry_path[documents_dir.len()..]
            .split('/')
            .next_back()
            .unwrap_or("");
        let document_id = filename.strip_suffix(".yjs").unwrap_or("");
        if document_id.is_empty() {
            warnings.push(format!(
                "Skipped document with invalid archive path: {entry_path}"
            ));
            continue;
        }
        // Map.set semantics: duplicates overwrite in place.
        if let Some(slot) = documents.iter_mut().find(|(id, _)| id == document_id) {
            slot.1 = bytes.clone();
        } else {
            documents.push((document_id.to_string(), bytes.clone()));
        }
    }

    let mut originals: Vec<(String, Vec<u8>)> = Vec::new();
    for (entry_path, bytes) in &entries {
        if !original_entry_path_is_valid(entry_path)
            && !geist_entry_path_is_valid(entry_path)
            && !region_entry_path_is_valid(entry_path)
        {
            continue;
        }
        if let Some(slot) = originals.iter_mut().find(|(path, _)| path == entry_path) {
            slot.1 = bytes.clone();
        } else {
            originals.push((entry_path.clone(), bytes.clone()));
        }
    }

    let chronicle_dir = normalize_archive_prefix(
        files
            .get("chronicle_dir")
            .and_then(Value::as_str)
            .unwrap_or("chronicle/documents/"),
    );
    let mut chronicle: Vec<(String, crate::document_chronicle::ChronicleEntry)> = Vec::new();
    for (entry_path, bytes) in &entries {
        if !entry_path.starts_with(&chronicle_dir) || !entry_path.ends_with(".json") {
            continue;
        }
        let filename = entry_path[chronicle_dir.len()..]
            .split('/')
            .next_back()
            .unwrap_or("");
        let document_id = filename.strip_suffix(".json").unwrap_or("");
        if document_id.is_empty() {
            continue;
        }
        match serde_json::from_slice::<crate::document_chronicle::ChronicleEntry>(bytes) {
            Ok(entry) => {
                if let Some(slot) = chronicle.iter_mut().find(|(id, _)| id == document_id) {
                    slot.1 = entry;
                } else {
                    chronicle.push((document_id.to_string(), entry));
                }
            }
            Err(error) => warnings.push(format!(
                "Skipped unreadable chronicle entry {entry_path}: {error}"
            )),
        }
    }

    if workspace_bytes.is_none() {
        warnings.push("Archive does not contain a workspace Y.Doc".to_string());
    }
    if documents.is_empty() {
        warnings.push("Archive does not contain document Y.Docs".to_string());
    }

    Ok(ParsedGraphArchive {
        manifest,
        rdf_n_quads,
        workspace_bytes,
        documents,
        originals,
        chronicle,
        warnings,
    })
}

/// Drops quads whose NAMED-GRAPH term is derived state the local
/// materializers own:
///
/// - any projection graph (`…:projection:…`), and
/// - anything `is_reserved_rdf_graph_iri` reserves for the NEW graph —
///   which includes the graph ROOT URN itself. Hosted-cloud archives keep
///   their whole document projection in that one named graph; after the
///   identity rewrite those quads target the local root, which
///   `load_rdf_dataset` refuses by design (the maiden import burned its
///   full retry budget on that refusal, 2026-08-29). Everything in them
///   regenerates from the document Y.Docs this same import materializes.
///
/// Deciding with the loader's OWN reservation rule means the filter and
/// the refusal can never drift apart. Only the graph label is inspected: a
/// user-graph quad whose SUBJECT or OBJECT mentions a reserved IRI is
/// preserved.
pub(crate) fn filter_reserved_projection_quads(
    rdf_n_quads: &str,
    new_graph_id: &str,
) -> (String, usize) {
    use oxigraph::io::{RdfFormat, RdfParser};
    use oxigraph::model::GraphName;

    let mut kept = String::with_capacity(rdf_n_quads.len());
    let mut dropped = 0usize;
    for line in rdf_n_quads.split_inclusive('\n') {
        let mut parser = RdfParser::from_format(RdfFormat::NQuads).for_slice(line.as_bytes());
        // Only discard a complete, valid single quad. In particular, retain
        // parse failures (including trailing garbage) for the loader to reject.
        if let (Some(Ok(quad)), None) = (parser.next(), parser.next()) {
            if let GraphName::NamedNode(graph) = quad.graph_name {
                let graph_iri = graph.as_str();
                if graph_iri.contains(":projection:")
                    || crate::rdf_authority::is_reserved_rdf_graph_iri(new_graph_id, graph_iri)
                {
                    dropped += 1;
                    continue;
                }
            }
        }
        kept.push_str(line);
    }
    (kept, dropped)
}

/// Geist authority files travel under `memory/`, `narrative/`, and
/// `values/` (2026-08 format extension, additive). Bounded depth, plain
/// relative segments only — extraction joins these directly under the new
/// graph dir, so the validator is the write boundary.
fn geist_entry_path_is_valid(path: &str) -> bool {
    let segments: Vec<&str> = path.split('/').collect();
    if segments.len() < 2 || segments.len() > 6 {
        return false;
    }
    matches!(segments[0], "memory" | "narrative" | "values")
        && segments.iter().all(|segment| {
            !segment.is_empty()
                && *segment != ".."
                && *segment != "."
                && !segment.contains('\\')
        })
}

/// Exactly `artifact-regions/{wireId}.json`: the regions side store (step
/// 3). The wire id is a local id, so the file lands where the sweep at the
/// first persist can judge it (a dropped wire's file goes then).
fn region_entry_path_is_valid(path: &str) -> bool {
    let segments: Vec<&str> = path.split('/').collect();
    segments.len() == 2
        && segments[0] == crate::artifact_regions::REGIONS_DIR
        && segments[1]
            .strip_suffix(".json")
            .is_some_and(|stem| crate::ids::validate_local_id(stem, "wire_id").is_ok())
}

/// Exactly `{artifacts|images|documents}/{entity_id}/original/{filename}` —
/// four non-empty segments, fixed root and `original` marker. Anything else
/// (including nested subdirectories) is not an original entry and is ignored,
/// which keeps extraction confined to well-known graph-dir locations.
fn original_entry_path_is_valid(path: &str) -> bool {
    let segments: Vec<&str> = path.split('/').collect();
    if segments.len() != 4 {
        return false;
    }
    matches!(segments[0], "artifacts" | "images" | "documents")
        && !segments[1].is_empty()
        && segments[2] == "original"
        && !segments[3].is_empty()
        && !segments.iter().any(|segment| *segment == "..")
}

fn require_compressed_archive_size(size: usize) -> Result<(), String> {
    if size > MAX_EXTRACTED_SIZE {
        return Err("Compressed archive, including its gzip header, exceeds 500 MiB".into());
    }
    Ok(())
}

fn read_gzip_bounded(bytes: &[u8], limit: usize) -> Result<Vec<u8>, String> {
    // Preserve first-member behavior: trailing compressed members/data are not decoded.
    let mut decoder = GzDecoder::new(bytes);
    let mut result = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        // Read at most the remaining budget and a single overflow witness.
        let read_limit = (limit - result.len()).saturating_add(1).min(chunk.len());
        let count = decoder.read(&mut chunk[..read_limit])
            .map_err(|_| "Invalid or corrupt archive: expected gzip-compressed tar data".to_string())?;
        if count == 0 { return Ok(result); }
        if count > limit - result.len() {
            return Err(format!("Decompressed tar container exceeds {limit} bytes"));
        }
        if result.capacity() - result.len() < count {
            let target = result.capacity().saturating_mul(2).max(result.len() + count).min(limit);
            result.try_reserve_exact(target - result.len())
                .map_err(|error| format!("Cannot allocate bounded archive buffer: {error}"))?;
        }
        result.extend_from_slice(&chunk[..count]);
    }
}

/// Manifest validation — error strings verbatim from parseGraphArchive.
fn parse_manifest(value: &Value) -> Result<GraphArchiveManifest, String> {
    let version = value.get("version");
    // JS strict `manifest.version !== 1`: only the number 1 (incl. 1.0) passes.
    if version.and_then(Value::as_f64) != Some(1.0) {
        return Err(format!(
            "Unsupported manifest version: {}",
            js_display(version)
        ));
    }
    let format = value.get("format");
    if format.and_then(Value::as_str) != Some("mnemosyne-graph-export") {
        return Err(format!("Unknown archive format: {}", js_display(format)));
    }
    let source_user_id = value.get("source_user_id");
    let source_graph_id = value.get("source_graph_id");
    if !js_truthy(source_user_id) || !js_truthy(source_graph_id) {
        return Err("Archive manifest is missing source user or graph identity".to_string());
    }
    Ok(GraphArchiveManifest {
        source_user_id: js_display(source_user_id),
        source_graph_id: js_display(source_graph_id),
        source_graph_title: nullable_string_value(value.get("source_graph_title")),
        source_graph_description: nullable_string_value(value.get("source_graph_description")),
        includes_artifacts: js_truthy(value.get("includes_artifacts")),
        exported_at: nullable_string_value(value.get("exported_at")),
    })
}

/// Port of rewriteGraphArchiveRdf: source graph URN → local graph URN, plus
/// the user/graph storage-path forms (`users/{u}/` → `users/default/`,
/// `graphs/{g}/` → `graphs/{newId}/`).
pub fn rewrite_graph_archive_rdf(
    rdf_n_quads: &str,
    manifest: &GraphArchiveManifest,
    target_graph_id: &str,
) -> String {
    let source_graph_uri = format!(
        "urn:mnemosyne:user:{}:graph:{}",
        manifest.source_user_id, manifest.source_graph_id
    );
    let target_graph_uri = format!("urn:mnemosyne:local:graph:{target_graph_id}");
    // Gardend-native archives already carry the local scheme
    // (`rdf::graph_subject` mints `urn:mnemosyne:local:graph:{id}` from the
    // start), so the user-scheme replacement above matches nothing for them —
    // that was the angel-found identity bug: restored triples kept the SOURCE
    // graph's URN, invisible to every graph-scoped query. Rewrite the local
    // scheme too, delimiter-anchored so a graph id that is a prefix of
    // another (`rt-src` vs `rt-src2`) can never be corrupted: in n-quads an
    // IRI ends at `>`, and any continuation within our URN vocabulary is `:`.
    let source_local_uri = format!("urn:mnemosyne:local:graph:{}", manifest.source_graph_id);
    rdf_n_quads
        .replace(&source_graph_uri, &target_graph_uri)
        .replace(
            &format!("{source_local_uri}>"),
            &format!("{target_graph_uri}>"),
        )
        .replace(
            &format!("{source_local_uri}:"),
            &format!("{target_graph_uri}:"),
        )
        .replace(
            &format!("users/{}/", manifest.source_user_id),
            "users/default/",
        )
        .replace(
            &format!("graphs/{}/", manifest.source_graph_id),
            &format!("graphs/{target_graph_id}/"),
        )
}

/// Port of rewriteGraphArchiveWorkspace: one transaction rewriting
/// `storageKey` on every artifact Y.Map and `sf_storageKey` on every
/// document Y.Map (sourceUserId → 'default', sourceGraphId → newGraphId).
pub fn rewrite_graph_archive_workspace(
    doc: &Doc,
    manifest: &GraphArchiveManifest,
    target_graph_id: &str,
) {
    let source_user_path = format!("users/{}/", manifest.source_user_id);
    let target_user_path = "users/default/";
    let source_graph_path = format!("graphs/{}/", manifest.source_graph_id);
    let target_graph_path = format!("graphs/{target_graph_id}/");

    let mut txn = doc.transact_mut();
    let artifacts = txn.get_or_insert_map("artifacts");
    let documents = txn.get_or_insert_map("documents");
    // A reference is a claim on THIS computer's roots: it does not travel.
    // An imported record with mode referenced would come alive the moment
    // the owner granted a covering root, past the put-time check; it goes,
    // with its wires (angel review of step 2).
    let referenced: Vec<String> = artifacts
        .iter(&txn)
        .filter_map(|(id, out)| match out {
            Out::YMap(child) => match child.get(&txn, "mode") {
                Some(Out::Any(Any::String(mode))) if &*mode == "referenced" => Some(id.to_string()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    if !referenced.is_empty() {
        log::info!(
            "import: {} referenced artifact record(s) dropped; references do not travel",
            referenced.len()
        );
    }
    for id in &referenced {
        artifacts.remove(&mut txn, id);
        super::workspace_ops::remove_wires_targeting_artifact(&mut txn, id);
    }
    for (root, key) in [(artifacts, "storageKey"), (documents, "sf_storageKey")] {
        let children: Vec<MapRef> = root
            .iter(&txn)
            .filter_map(|(_, out)| match out {
                Out::YMap(child) => Some(child),
                _ => None,
            })
            .collect();
        for child in children {
            let Some(Out::Any(Any::String(value))) = child.get(&txn, key) else {
                continue;
            };
            let next = value
                .replace(&source_user_path, target_user_path)
                .replace(&source_graph_path, &target_graph_path);
            if next != *value {
                child.insert(&mut txn, key, next);
            }
        }
    }

    // Wires embed the graph id in `targetGraphId` (and `sceneGraphId`): an
    // intra-graph wire stores the graph's OWN id as its target. The
    // storageKey rewrite above never touched the wires map, so before this
    // fix every intra-graph wire in an imported graph dangled at the old
    // graph id — clicking one asked for a graph that no longer exists
    // (`rdf_dump: graph not found: default`, 2026-09-03). Only an EXACT match
    // of the source graph id is rewritten (→ the new graph id, making the
    // wire resolve locally again). Wires targeting a DIFFERENT id are left
    // untouched: cross-graph wires are anomalies (the hard-boundary rule was
    // not always explicit), not a supported feature — they are not made to
    // work here, they only need to fail gracefully at click time, which is a
    // separate frontend concern.
    let wires = txn.get_or_insert_map("wires");
    let wire_children: Vec<MapRef> = wires
        .iter(&txn)
        .filter_map(|(_, out)| match out {
            Out::YMap(child) => Some(child),
            _ => None,
        })
        .collect();
    for wire in wire_children {
        for field in ["targetGraphId", "sceneGraphId"] {
            let Some(Out::Any(Any::String(value))) = wire.get(&txn, field) else {
                continue;
            };
            if value.as_ref() == manifest.source_graph_id {
                wire.insert(&mut txn, field, target_graph_id.to_string());
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tar member/path rules retain legacy behavior, but expansion, tail padding
// and offset arithmetic are bounded before parsing or allocation can overrun.
// ─────────────────────────────────────────────────────────────────────────────

fn read_tar_entries(tar_bytes: &[u8]) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    let mut offset = 0usize;
    let mut count = 0usize;
    let mut extracted_size = 0usize;

    while offset + TAR_BLOCK_SIZE <= tar_bytes.len() {
        let header = &tar_bytes[offset..offset + TAR_BLOCK_SIZE];
        if header.iter().all(|byte| *byte == 0) {
            let tail = &tar_bytes[offset..];
            if tail.len() > MAX_TAR_TRAILING_BYTES || tail.iter().any(|byte| *byte != 0) {
                return Err("Invalid tar tail: expected at most 64 KiB of zero padding".into());
            }
            return Ok(entries);
        }

        count += 1;
        if count > MAX_ARCHIVE_FILES {
            return Err(format!(
                "Archive contains too many files (>{MAX_ARCHIVE_FILES})"
            ));
        }

        let name = tar_string(header, 0, 100);
        let prefix = tar_string(header, 345, 155);
        let path = normalize_archive_path(&if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        });
        validate_archive_path(&path)?;
        let size = tar_octal(header, 124, 12)?;
        let typeflag = header[156];
        let data_offset = offset + TAR_BLOCK_SIZE;
        let padded_size = size.div_ceil(TAR_BLOCK_SIZE).checked_mul(TAR_BLOCK_SIZE)
            .ok_or("Invalid tar member size overflow")?;
        let next_offset = data_offset.checked_add(padded_size).ok_or("Invalid tar offset overflow")?;
        if next_offset > tar_bytes.len() {
            return Err(format!(
                "Invalid or corrupt archive: truncated entry {path}"
            ));
        }

        extracted_size = extracted_size.checked_add(size).ok_or("Invalid extracted size overflow")?;
        if extracted_size > MAX_EXTRACTED_SIZE {
            return Err(format!(
                "Archive too large. Maximum size: {}MB",
                MAX_EXTRACTED_SIZE / (1024 * 1024)
            ));
        }

        if typeflag == 0 || typeflag == b'0' {
            let bytes = tar_bytes[data_offset..data_offset + size].to_vec();
            if let Some(slot) = entries.iter_mut().find(|(existing, _)| *existing == path) {
                slot.1 = bytes;
            } else {
                entries.push((path, bytes));
            }
        }
        offset = next_offset;
    }

    if tar_bytes[offset..].iter().any(|byte| *byte != 0) {
        return Err("Invalid tar tail: incomplete nonzero header".into());
    }

    Ok(entries)
}

fn entries_get<'a>(entries: &'a [(String, Vec<u8>)], path: &str) -> Option<&'a [u8]> {
    entries
        .iter()
        .find(|(entry_path, _)| entry_path == path)
        .map(|(_, bytes)| bytes.as_slice())
}

/// Port of tarString: NUL-terminated field, UTF-8 decoded, trimmed.
fn tar_string(block: &[u8], start: usize, length: usize) -> String {
    let slice = &block[start..start + length];
    let end = slice
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(slice.len());
    String::from_utf8_lossy(&slice[..end]).trim().to_string()
}

/// Port of tarOctal: parseInt(raw, 8) semantics (leading octal digits, an
/// optional sign; anything unparsable or negative is an error).
fn tar_octal(block: &[u8], start: usize, length: usize) -> Result<usize, String> {
    let raw = tar_string(block, start, length).replace('\0', "");
    let raw = raw.trim().to_string();
    if raw.is_empty() {
        return Ok(0);
    }
    let (negative, digits) = match raw.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, raw.strip_prefix('+').unwrap_or(&raw)),
    };
    let mut value: usize = 0;
    let mut any = false;
    for ch in digits.chars() {
        match ch.to_digit(8) {
            Some(digit) => {
                value = value
                    .checked_mul(8)
                    .and_then(|v| v.checked_add(digit as usize))
                    .ok_or_else(|| format!("Invalid tar entry size: {raw}"))?;
                any = true;
            }
            None => break,
        }
    }
    if !any || negative {
        return Err(format!("Invalid tar entry size: {raw}"));
    }
    Ok(value)
}

/// Port of normalizeArchivePrefix.
fn normalize_archive_prefix(value: &str) -> String {
    let normalized = normalize_archive_path(value);
    if normalized.ends_with('/') {
        normalized
    } else {
        format!("{normalized}/")
    }
}

/// Port of normalizeArchivePath: backslashes → '/', strip one leading
/// "./"-run, collapse slash runs.
fn normalize_archive_path(value: &str) -> String {
    let replaced = value.replace('\\', "/");
    // /^\.\/+/: a leading "." followed by one or more "/".
    let stripped = match replaced.strip_prefix('.') {
        Some(rest) if rest.starts_with('/') => rest.trim_start_matches('/'),
        _ => replaced.as_str(),
    };
    // /\/+/g → "/"
    let mut out = String::with_capacity(stripped.len());
    let mut previous_was_slash = false;
    for ch in stripped.chars() {
        if ch == '/' {
            if !previous_was_slash {
                out.push(ch);
            }
            previous_was_slash = true;
        } else {
            out.push(ch);
            previous_was_slash = false;
        }
    }
    out
}

/// Port of validateArchivePath: reject empty, absolute, and `..` segments.
fn validate_archive_path(path: &str) -> Result<(), String> {
    if path.is_empty() || path.starts_with('/') {
        return Err(format!("Unsafe path in archive: {path}"));
    }
    if path.split('/').any(|part| part == "..") {
        return Err(format!("Unsafe path in archive: {path}"));
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// JS payload coercion helpers (objectPayload / stringValue /
// nullableStringValue and the `a ?? b` access chains)
// ─────────────────────────────────────────────────────────────────────────────

fn obj(value: &Value) -> JsonMap<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

/// `map.a ?? map.b ?? ...` — `??` only skips null/undefined.
fn pick<'a>(map: &'a JsonMap<String, Value>, keys: &[&str]) -> Option<&'a Value> {
    for key in keys {
        match map.get(*key) {
            Some(Value::Null) | None => continue,
            Some(value) => return Some(value),
        }
    }
    None
}

/// stringValue / nullableStringValue: undefined/null/'' → None, else String(v).
fn nullable_string_value(value: Option<&Value>) -> Option<String> {
    match value {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if s.is_empty() => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(other) => Some(js_display(Some(other))),
    }
}

/// JS String() coercion for the values that can plausibly appear here.
fn js_display(value: Option<&Value>) -> String {
    match value {
        None => "undefined".to_string(),
        Some(Value::Null) => "null".to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => {
            let f = n.as_f64().unwrap_or(f64::NAN);
            if f.is_finite() && f.fract() == 0.0 && f.abs() < 1e21 {
                format!("{}", f as i64)
            } else {
                f.to_string()
            }
        }
        Some(other) => other.to_string(),
    }
}

/// JS truthiness for serde_json values (missing key counts as undefined).
fn js_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0 && !f.is_nan()).unwrap_or(false),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(_)) | Some(Value::Object(_)) => true,
    }
}

/// Port of decodeBytesBase64 (atob).
fn decode_bytes_base64(value: &str) -> Result<Vec<u8>, String> {
    base64::engine::general_purpose::STANDARD
        .decode(value)
        .map_err(|error| format!("decode graph archive base64: {error}"))
}

fn encode_bytes_base64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn encode_full_state(doc: &Doc) -> Vec<u8> {
    let txn = doc.transact();
    txn.encode_state_as_update_v1(&StateVector::default())
}

fn apply_full_update(doc: &Doc, bytes: &[u8], what: &str) -> Result<(), String> {
    let update =
        Update::decode_v1(bytes).map_err(|error| format!("decode {what} Y.Doc update: {error}"))?;
    let mut txn = doc.transact_mut();
    txn.apply_update(update)
        .map_err(|error| format!("apply {what} Y.Doc update: {error}"))
}

// ─────────────────────────────────────────────────────────────────────────────
// Entry point — port of importGraphArchive
// ─────────────────────────────────────────────────────────────────────────────

pub(crate) async fn apply(app: &AppHandle, operation: &CrdtOperation) -> Result<Value, String> {
    apply_classified(app, operation)
        .await
        .map_err(ApplyOperationError::into_message)
}

fn archive_failure(error: impl Into<String>, durable_started: bool) -> ApplyOperationError {
    if durable_started {
        ApplyOperationError::retryable_after_hot_commit(error)
    } else {
        ApplyOperationError::terminal(error)
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ArchiveFailurePoint {
    BeforeDocument,
}

#[cfg(test)]
static FAIL_NEXT_ARCHIVE_STEP: std::sync::OnceLock<
    std::sync::Mutex<Option<(String, ArchiveFailurePoint)>>,
> = std::sync::OnceLock::new();

#[cfg(test)]
pub(crate) fn fail_next_archive_step_for_test(
    operation_id: impl Into<String>,
    point: ArchiveFailurePoint,
) {
    *FAIL_NEXT_ARCHIVE_STEP
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((operation_id.into(), point));
}

#[cfg(test)]
fn maybe_fail_archive_step_for_test(
    operation_id: &str,
    point: ArchiveFailurePoint,
) -> Result<(), ApplyOperationError> {
    let mut pending = FAIL_NEXT_ARCHIVE_STEP
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if pending
        .as_ref()
        .is_some_and(|(expected_id, expected_point)| {
            expected_id == operation_id && *expected_point == point
        })
    {
        pending.take();
        return Err(ApplyOperationError::retryable_after_hot_commit(format!(
            "injected graph.importArchive failure at {point:?} for {operation_id}"
        )));
    }
    Ok(())
}

pub(crate) async fn apply_classified(
    app: &AppHandle,
    operation: &CrdtOperation,
) -> ApplyOperationResult<Value> {
    // Defer periodic durable flushes for the duration of this import, and fire
    // one forced flush when it finishes (see cell_durability::ImportGuard).
    let _import_guard = crate::cell_durability::import_guard();
    let payload = obj(&operation.payload);
    let operation_id = operation.operation_id.as_str();

    let new_graph_id = nullable_string_value(pick(&payload, &["newGraphId", "new_graph_id"]))
        .ok_or_else(|| "graph.importArchive: newGraphId is required".to_string())?;
    crate::ids::validate_local_id(&new_graph_id, "graph_id")?;
    let tar_gz_base64 = nullable_string_value(pick(
        &payload,
        &["tarGzBase64", "tar_gz_base64", "dataBase64", "data_base64"],
    ));
    let pending_archive_path = nullable_string_value(pick(
        &payload,
        &["pendingArchivePath", "pending_archive_path"],
    ));
    if tar_gz_base64.is_none() && pending_archive_path.is_none() {
        return Err(ApplyOperationError::terminal(
            "graph.importArchive: tarGzBase64 or pendingArchivePath is required".to_string(),
        ));
    }

    // Tier B replay guard: if the operation completed previously, return the
    // cached envelope without re-touching the filesystem. The ledger entry is
    // written only after the full handler succeeds (see end of this function).
    let ledger_hit = crate::operation_completion_ledger::completion_entry_for(app, operation_id)
        .map_err(|error| {
            ApplyOperationError::retryable_after_hot_commit(format!(
                "read graph.importArchive completion ledger: {error}"
            ))
        })?;
    if let Some(hit) = ledger_hit {
        if hit.kind == "graph.importArchive" {
            let mut cached = hit
                .result
                .as_ref()
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            // Best-effort cleanup of any pending archive file before short-circuiting.
            if let Some(pending) = &pending_archive_path {
                cleanup_pending_archive(app, pending);
            }
            cached.insert("replayed".to_string(), json!(true));
            return Ok(Value::Object(cached));
        }
        return Err(ApplyOperationError::terminal(format!(
            "completion ledger operation {operation_id} belongs to {}, not graph.importArchive",
            hit.kind
        )));
    }

    // Existence + tag check: if the graph already exists, verify that this is
    // a partial-replay (same operationId) rather than a different op colliding
    // on graph_id. Mismatch is a hard error; match means the prior attempt
    // crashed mid-way and we continue from per-step idempotency.
    let existing = crate::graph_service::list_graphs(app.clone())
        .map_err(ApplyOperationError::retryable_after_hot_commit)?
        .into_iter()
        .find(|graph| graph.graph_id == new_graph_id);
    if let Some(existing) = &existing {
        if existing.created_by_operation_id.as_deref() != Some(operation_id) {
            return Err(ApplyOperationError::terminal(format!(
                "graph.importArchive: graph {new_graph_id} already exists with a different originating operation"
            )));
        }
        // partial-replay: continue. create_graph below will be skipped.
    }

    let durable_started = existing.is_some();
    let read_archive_started = Instant::now();
    let bytes: Vec<u8> = if let Some(pending) = &pending_archive_path {
        let read_result = crate::pending_upload_service::read_pending_upload_file(
            app.clone(),
            crate::pending_upload_service::PendingUploadFileInput {
                graph_id: None,
                pending_path: pending.clone(),
            },
        );
        let record = read_result.map_err(|error| archive_failure(error, durable_started))?;
        decode_bytes_base64(&record.data_base64)
            .map_err(|error| archive_failure(error, durable_started))?
    } else {
        decode_bytes_base64(tar_gz_base64.as_deref().unwrap_or(""))
            .map_err(|error| archive_failure(error, durable_started))?
    };
    record_import_phase(
        app,
        operation_id,
        "rustImportArchiveReadUploadMs",
        read_archive_started,
    );

    let parse_started = Instant::now();
    let parsed =
        parse_graph_archive(&bytes).map_err(|error| archive_failure(error, durable_started))?;
    record_import_phase(app, operation_id, "rustImportArchiveParseMs", parse_started);
    let manifest = &parsed.manifest;
    let geist_plan = crate::legacy_geist_import::plan(&parsed, &new_graph_id)
        .map_err(|error| archive_failure(error, durable_started))?;
    // Recover legacy value authorities before the graph-root RDF is filtered.
    // Preflight invalid/ambiguous records before creating any destination state.
    let legacy_plan = crate::legacy_valuation_import::plan_legacy_valuations(
        &parsed.rdf_n_quads, &manifest.source_user_id, &manifest.source_graph_id,
    ).map_err(|error| archive_failure(error, durable_started))?;
    let legacy_values = &legacy_plan.records;
    if !legacy_values.is_empty() && parsed.originals.iter().any(|(path, _)| path == "values/block-values.json") {
        return Err(archive_failure("archive supplies both legacy and native shared valuation authorities", durable_started));
    }
    let title = nullable_string_value(pick(&payload, &["newTitle", "new_title"]))
        .or_else(|| manifest.source_graph_title.clone())
        .unwrap_or_else(|| new_graph_id.clone());
    let description = manifest.source_graph_description.clone();

    let graph_started = Instant::now();
    let graph = match existing {
        Some(graph) => graph,
        None => crate::graph_service::create_graph_service_inner(
            app,
            crate::graph_service::CreateGraphInput {
                title: title.clone(),
                graph_id: Some(new_graph_id.clone()),
                description: description.clone(),
                operation_id: Some(operation_id.to_string()),
            },
        )
        .map_err(|error| ApplyOperationError::retryable_after_hot_commit(error.to_string()))?,
    };
    record_import_phase(
        app,
        operation_id,
        "rustImportArchiveGraphRecordMs",
        graph_started,
    );

    // Workspace Y.Doc: apply the archived full update, rewrite identity
    // paths, persist with a materialized snapshot (same shape the desktop
    // saveWorkspace bridge passes).
    let mut workspace_imported = false;
    let mut workspace_documents: HashMap<String, Value> = HashMap::new();
    if let Some(workspace_bytes) = &parsed.workspace_bytes {
        let workspace_started = Instant::now();
        let workspace_doc = Doc::new();
        apply_full_update(&workspace_doc, workspace_bytes, "workspace")
            .map_err(ApplyOperationError::retryable_after_hot_commit)?;
        rewrite_graph_archive_workspace(&workspace_doc, manifest, &new_graph_id);
        let snapshot = super::workspace_ops::materialize_workspace_snapshot_json(
            &new_graph_id,
            &workspace_doc,
        )
        .map_err(ApplyOperationError::retryable_after_hot_commit)?;
        if let Some(list) = snapshot.get("documents").and_then(Value::as_array) {
            for entry in list {
                if let Some(id) = entry.get("id").and_then(Value::as_str) {
                    workspace_documents.insert(id.to_string(), entry.clone());
                }
            }
        }
        let save_input: crate::document_types::SaveWorkspaceInput = serde_json::from_value(json!({
            "graphId": new_graph_id,
            "ydocUpdateBase64": encode_bytes_base64(&encode_full_state(&workspace_doc)),
            "snapshot": snapshot,
            "traceOperationId": operation_id,
        }))
        .map_err(|error| {
            ApplyOperationError::retryable_after_hot_commit(format!(
                "assemble save_workspace input: {error}"
            ))
        })?;
        crate::document_persistence_service::save_workspace_with_lease(app.clone(), save_input)
            .map_err(ApplyOperationError::retryable_after_hot_commit)?;
        workspace_imported = true;
        record_import_phase(
            app,
            operation_id,
            "rustImportArchiveWorkspaceMs",
            workspace_started,
        );
    }

    // Documents: every deterministic document is part of the transaction.
    // Never bless a partial graph as completion-success.
    let mut documents_imported: usize = 0;
    let mut document_ids: Vec<String> = Vec::new();
    let mut warnings = parsed.warnings.clone();
    let documents_started = Instant::now();
    let chronicle_entries: HashMap<&str, &crate::document_chronicle::ChronicleEntry> = parsed
        .chronicle
        .iter()
        .map(|(id, entry)| (id.as_str(), entry))
        .collect();
    for (document_id, document_bytes) in &parsed.documents {
        #[cfg(test)]
        maybe_fail_archive_step_for_test(operation_id, ArchiveFailurePoint::BeforeDocument)?;
        match import_one_document(
            app,
            &new_graph_id,
            document_id,
            document_bytes,
            workspace_documents.get(document_id),
            chronicle_entries.get(document_id.as_str()).copied(),
            operation_id,
            &mut warnings,
        ) {
            Ok(saved_document_id) => {
                documents_imported += 1;
                document_ids.push(saved_document_id);
            }
            Err(error) => {
                return Err(ApplyOperationError::retryable_after_hot_commit(format!(
                    "import document {document_id}: {error}"
                )));
            }
        }
    }
    record_import_phase(
        app,
        operation_id,
        "rustImportArchiveDocumentsMs",
        documents_started,
    );

    // Originals: artifact/image/document source files travel under their
    // graph-dir-relative paths (additive format extension, 2026-08). Paths
    // were validated at parse time (`original_entry_path_is_valid`), so the
    // join below cannot escape the graph dir. Manifest local_path fields are
    // repaired afterward, same as graph duplication does.
    let mut original_files_imported: usize = 0;
    if !parsed.originals.is_empty() {
        let originals_started = Instant::now();
        let graph_dir = crate::graph_paths::existing_graph_dir(app, &new_graph_id)
            .map_err(ApplyOperationError::retryable_after_hot_commit)?;
        for (relative_path, file_bytes) in &parsed.originals {
            let target = graph_dir.join(relative_path);
            if let Some(parent) = target.parent() {
                crate::storage::create_dir_all(parent)
                    .map_err(ApplyOperationError::retryable_after_hot_commit)?;
            }
            crate::storage::write_bytes(&target, file_bytes).map_err(|error| {
                ApplyOperationError::retryable_after_hot_commit(format!(
                    "write imported original {relative_path}: {error}"
                ))
            })?;
            original_files_imported += 1;
        }
        for root in ["artifacts", "images", "documents"] {
            if let Err(error) = crate::original_file_manifest_store::rewrite_original_manifests_under(
                &graph_dir.join(root),
            ) {
                log::warn!("repair imported original manifests under {root}: {error}");
            }
        }
        record_import_phase(
            app,
            operation_id,
            "rustImportArchiveOriginalsMs",
            originals_started,
        );
    }

    {
    let _value_guard = crate::salience_value_store::value_mutation_guard()
        .map_err(ApplyOperationError::retryable_after_hot_commit)?;
    if !legacy_values.is_empty() {
        let graph_dir = crate::graph_paths::existing_graph_dir(app, &new_graph_id)
            .map_err(ApplyOperationError::retryable_after_hot_commit)?;
        crate::legacy_valuation_import::install_legacy_valuations(&graph_dir, &new_graph_id, legacy_values)
            .map_err(ApplyOperationError::retryable_after_hot_commit)?;
        let values = crate::salience_value_store::read_value_store(&graph_dir, &new_graph_id)
            .map_err(ApplyOperationError::retryable_after_hot_commit)?;
        crate::salience_rdf_materializer::reconcile_value_store(&graph_dir, &values)
            .map_err(ApplyOperationError::retryable_after_hot_commit)?;
    }
    if !legacy_plan.quarantine.is_empty() {
        let graph_dir = crate::graph_paths::existing_graph_dir(app, &new_graph_id)
            .map_err(ApplyOperationError::retryable_after_hot_commit)?;
        crate::legacy_valuation_import::install_quarantine(&graph_dir, &legacy_plan.quarantine)
            .map_err(ApplyOperationError::retryable_after_hot_commit)?;
        warnings.push(format!("{} legacy valuation records retained in values/legacy-valuation-quarantine.json; unresolved source references/statistics require review", legacy_plan.quarantine.len()));
    }
    let graph_dir = crate::graph_paths::existing_graph_dir(app, &new_graph_id)
        .map_err(ApplyOperationError::retryable_after_hot_commit)?;
    geist_plan.install(&graph_dir, &new_graph_id)
        .map_err(ApplyOperationError::retryable_after_hot_commit)?;
    }

    // RDF: rewrite source identity → local graph identity, load as n-quads.
    let mut rdf_triple_count: usize = 0;
    if !parsed.rdf_n_quads.trim().is_empty() {
        let rewrite_started = Instant::now();
        let rewritten = rewrite_graph_archive_rdf(&parsed.rdf_n_quads, manifest, &new_graph_id);
        let (rewritten, projection_quads_dropped) =
            filter_reserved_projection_quads(&rewritten, &new_graph_id);
        if projection_quads_dropped > 0 {
            log::info!(
                "graph.importArchive dropped {projection_quads_dropped} reserved-graph quad(s); \
                 derived RDF regenerates locally from imported authorities"
            );
        }
        record_import_phase(
            app,
            operation_id,
            "rustImportArchiveRewriteRdfMs",
            rewrite_started,
        );
        let load_started = Instant::now();
        let result = crate::rdf_service::load_rdf_dataset(
            app.clone(),
            crate::rdf_service::RdfLoadInput {
                graph_id: new_graph_id.clone(),
                data: rewritten,
                format: "application/n-quads".to_string(),
                base_iri: None,
                target_graph_iri: None,
            },
        )
        .map_err(ApplyOperationError::retryable_after_hot_commit)?;
        rdf_triple_count = result.quad_count.unwrap_or_default();
        record_import_phase(
            app,
            operation_id,
            "rustImportArchiveLoadRdfMs",
            load_started,
        );
    }

    let graph_dir = crate::graph_paths::existing_graph_dir(app, &new_graph_id)
        .map_err(ApplyOperationError::retryable_after_hot_commit)?;
    geist_plan.reconcile(&graph_dir, &new_graph_id)
        .map_err(ApplyOperationError::retryable_after_hot_commit)?;
    let graph_value = serde_json::to_value(&graph).map_err(|error| {
        ApplyOperationError::retryable_after_hot_commit(format!("serialize graph record: {error}"))
    })?;
    let envelope = json!({
        "type": "import_graph",
        "graph_id": new_graph_id,
        "graphId": new_graph_id,
        "title": title,
        "document_count": documents_imported,
        "documentCount": documents_imported,
        "document_ids": document_ids,
        "documentIds": document_ids,
        "rdf_triple_count": rdf_triple_count,
        "rdfTripleCount": rdf_triple_count,
        "source_graph_id": manifest.source_graph_id,
        "sourceGraphId": manifest.source_graph_id,
        "source_user_id": manifest.source_user_id,
        "sourceUserId": manifest.source_user_id,
        "archive_size_bytes": bytes.len(),
        "archiveSizeBytes": bytes.len(),
        "includes_artifacts": manifest.includes_artifacts,
        "includesArtifacts": manifest.includes_artifacts,
        "original_files_imported": original_files_imported,
        "originalFilesImported": original_files_imported,
        "workspace_imported": workspace_imported,
        "workspaceImported": workspace_imported,
        "valuations_imported": legacy_values.len(),
        "geist_translated": geist_plan.summary(),
        "valuations_quarantined": legacy_plan.quarantine.len(),
        "warnings": warnings,
        "graph": graph_value,
    });

    // Append the Tier B completion entry only after every step has succeeded.
    // Failure leaves the outer journal operation retryable under the same ID.
    let entry = crate::operation_completion_ledger::OperationCompletionEntry {
        schema_version: 1,
        operation_id: operation_id.to_string(),
        kind: "graph.importArchive".to_string(),
        graph_id: Some(new_graph_id.clone()),
        completed_at: operation.enqueue_timestamp.clone(),
        payload_hash: None,
        result: Some(envelope.clone()),
    };
    let ledger_started = Instant::now();
    crate::operation_completion_ledger::append_completion_entry(app, entry)
        .map_err(ApplyOperationError::retryable_after_hot_commit)?;
    record_import_phase(
        app,
        operation_id,
        "rustImportArchiveCompletionLedgerMs",
        ledger_started,
    );

    if let Some(pending) = &pending_archive_path {
        let cleanup_started = Instant::now();
        cleanup_pending_archive(app, pending);
        record_import_phase(
            app,
            operation_id,
            "rustImportArchiveCleanupPendingMs",
            cleanup_started,
        );
    }

    Ok(envelope)
}

/// One archived document Y.Doc → create_document + save_document (the same
/// internals the desktop frontend's bridge commands run). Returns the saved
/// record's documentId.
/// Legacy graphs (hosted cloud never enforced local write validation) carry
/// titles the local rules reject — empty after trim, or over the
/// `ids::MAX_TITLE_CHARS` cap of `ids::normalize_title`. A whole-graph import
/// must never fail on a title (the maiden import died on a 101-char
/// philosophy title, 2026-08-29): sanitize to the local rules and report
/// what changed. Since 2026-09-12 the cap is 512, so real titles pass through
/// whole and the record agrees with the workspace catalog.
fn sanitize_import_title(document_id: &str, raw_title: &str) -> (String, Option<String>) {
    let trimmed = raw_title.trim();
    let base = if trimmed.is_empty() {
        document_id
    } else {
        trimmed
    };
    let clipped: String = base
        .chars()
        .take(crate::ids::MAX_TITLE_CHARS)
        .collect();
    let warning = (clipped != raw_title).then(|| {
        format!(
            "document {document_id}: title adjusted to local limits on import (was {} characters)",
            raw_title.chars().count()
        )
    });
    (clipped, warning)
}

#[allow(clippy::too_many_arguments)]
fn import_one_document(
    app: &AppHandle,
    graph_id: &str,
    document_id: &str,
    document_bytes: &[u8],
    catalog_entry: Option<&Value>,
    chronicle_entry: Option<&crate::document_chronicle::ChronicleEntry>,
    operation_id: &str,
    warnings: &mut Vec<String>,
) -> Result<String, String> {
    let doc = Doc::new();
    apply_full_update(&doc, document_bytes, "document")?;
    let snapshot = super::projection::materialize_ydoc(&doc, document_id);
    let tiptap_xml = super::projection::ydoc_to_tiptap_xml(&doc);
    let workspace_title = catalog_entry
        .and_then(|entry| entry.get("title"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let raw_title = workspace_title.unwrap_or_else(|| document_id.to_string());
    let (document_title, title_warning) = sanitize_import_title(document_id, &raw_title);
    if let Some(warning) = title_warning {
        warnings.push(warning);
    }

    let create_input: crate::document_types::CreateDocumentInput = serde_json::from_value(json!({
        "graphId": graph_id,
        "title": document_title,
        "documentId": document_id,
    }))
    .map_err(|error| format!("assemble create_document input: {error}"))?;
    crate::document_service::create_document_with_lease(app.clone(), create_input)?;

    let candidate = json!({
        "graphId": graph_id,
        "documentId": document_id,
        "title": document_title,
        "body": snapshot.body,
        "tiptapXml": tiptap_xml,
        "tiptapJson": snapshot.tiptap_json,
        "ydocUpdateBase64": encode_bytes_base64(&encode_full_state(&doc)),
        "tree": snapshot.tree_json,
        "blocks": snapshot.blocks_json,
        "traceOperationId": format!("{operation_id}-{document_id}"),
        // Chronicle: restore the original clock and authorship. Evidence in
        // order of trust — the archive's chronicle entry, else the exported
        // catalog's createdAt/updatedAt, else the import instant flagged as
        // such. Never the import instant pretending to be a birthday.
        "actor": crate::document_chronicle::ChronicleActor::system("archive-import").to_value(),
        "chronicleRestore": serde_json::to_value(
            chronicle_entry
                .map(crate::document_chronicle::ChronicleRestore::from_entry)
                .or_else(|| catalog_entry.and_then(crate::document_chronicle::ChronicleRestore::from_catalog_entry))
                .unwrap_or_else(crate::document_chronicle::ChronicleRestore::import_instant),
        )
        .map_err(|error| format!("serialize chronicle restore: {error}"))?,
    });
    if super::document_ops::reconcile_matching_document_projection(
        app,
        graph_id,
        document_id,
        &candidate,
    )?
    .is_none()
    {
        let save_input: crate::document_types::SaveDocumentInput =
            serde_json::from_value(candidate)
                .map_err(|error| format!("assemble save_document input: {error}"))?;
        crate::document_persistence_service::save_document_with_lease(app.clone(), save_input)?;
    }
    Ok(document_id.to_string())
}

/// Best-effort pending-archive cleanup (the TS .catch(console.warn) paths).
fn cleanup_pending_archive(app: &AppHandle, pending_path: &str) {
    if let Err(error) = crate::pending_upload_service::cleanup_pending_upload(
        app.clone(),
        crate::pending_upload_service::PendingUploadFileInput {
            graph_id: None,
            pending_path: pending_path.to_string(),
        },
    ) {
        log::warn!("[graph.importArchive] pending graph archive cleanup failed: {error}");
    }
}

#[cfg(test)]
mod import_title_sanitize_tests {
    use super::sanitize_import_title;

    #[test]
    fn real_long_titles_pass_through_whole() {
        // The two titles the 2026-09-12 rehearsal found clipped to 96 while
        // the workspace catalog kept them whole (100 and 117 characters).
        let hunt = "Hunt-Hendrix - The Correct Starting Point for Philosophy \
                    (Socrates, Descartes, Nietzsche, Balthasar)";
        let connectors = "Connectors Landscape Brief — what exists, what people pay for, \
                          and where the room's doors should open (2026-09-05)";
        for title in [hunt, connectors] {
            assert!(title.chars().count() > 96, "fixture must exceed the old cap");
            let (kept, warning) = sanitize_import_title("doc-x", title);
            assert_eq!(kept, title);
            assert!(warning.is_none(), "{warning:?}");
        }
    }

    #[test]
    fn runaway_titles_clip_at_the_shared_cap_with_warning() {
        let cap = crate::ids::MAX_TITLE_CHARS;
        let title: String = std::iter::repeat('x').take(cap + 40).collect();
        let (clipped, warning) = sanitize_import_title("doc-x", &title);
        assert_eq!(clipped.chars().count(), cap);
        assert!(title.starts_with(&clipped));
        let warning = warning.expect("clipping records a warning");
        assert!(warning.contains("doc-x"), "{warning}");
        // And the record write path agrees on the same limit.
        assert!(crate::ids::normalize_title(&clipped).is_ok());
        assert!(crate::ids::normalize_title(&title).is_err());
    }

    #[test]
    fn empty_and_whitespace_titles_fall_back_to_document_id() {
        let (from_empty, warning) = sanitize_import_title("doc-y", "");
        assert_eq!(from_empty, "doc-y");
        assert!(warning.is_some());

        let (from_spaces, warning) = sanitize_import_title("doc-z", "   ");
        assert_eq!(from_spaces, "doc-z");
        assert!(warning.is_some());
    }

    #[test]
    fn valid_titles_pass_through_untouched() {
        let (title, warning) = sanitize_import_title("doc-a", "A Reasonable Title");
        assert_eq!(title, "A Reasonable Title");
        assert!(warning.is_none());
    }
}

#[cfg(test)]
mod projection_filter_tests {
    use super::*;

    #[test]
    fn default_graph_reserved_iri_and_literal_objects_survive() {
        let rdf = concat!(
            "<urn:s> <urn:p> <urn:mnemosyne:local:graph:g1> .\n",
            "<urn:s> <urn:p> <urn:mnemosyne:local:graph:g1:projection:memory> .\n",
            "<urn:s> <urn:p> \"<urn:mnemosyne:local:graph:g1>\" .\n",
            "<urn:s> <urn:p> \"text\"^^<urn:mnemosyne:local:graph:g1> .\n",
            "<urn:s> <urn:p> \"text\"@en .",
        );
        let (kept, dropped) = filter_reserved_projection_quads(rdf, "g1");
        assert_eq!(dropped, 0);
        assert_eq!(kept, rdf);
        let store = oxigraph::store::Store::new().unwrap();
        crate::rdf_query_service::load_rdf_dataset_into_store(&store, "g1", &kept, "nquads", None)
            .expect("default-graph objects must load even when their IRIs are reserved");
        assert_eq!(store.len().unwrap(), 5);
    }

    #[test]
    fn escaped_literals_do_not_confuse_graph_detection() {
        let literal = r#""quoted \" <urn:mnemosyne:local:graph:g1> . \\ \n""#;
        let default_quad = format!("<urn:s> <urn:p> {literal} .\n");
        let user_quad = format!("<urn:s> <urn:p> {literal} <urn:user:graph> .\n");
        let reserved_quad =
            format!("<urn:s> <urn:p> {literal} <urn:mnemosyne:local:graph:g1> . # comment\n");
        let rdf = format!("{default_quad}{reserved_quad}{user_quad}");
        let (kept, dropped) = filter_reserved_projection_quads(&rdf, "g1");
        assert_eq!(dropped, 1);
        assert_eq!(kept, format!("{default_quad}{user_quad}"));
        let store = oxigraph::store::Store::new().unwrap();
        crate::rdf_query_service::load_rdf_dataset_into_store(&store, "g1", &kept, "nquads", None)
            .expect("escaped literal content must remain valid RDF");
        assert_eq!(store.len().unwrap(), 2);
    }

    #[test]
    fn malformed_lines_are_preserved_and_rejected_downstream() {
        for malformed in [
            "not RDF <urn:mnemosyne:local:graph:g1> .\n",
            "<urn:s> <urn:p> \"bad\\q\" <urn:mnemosyne:local:graph:g1> .\n",
            "<urn:s> <urn:p> \"unterminated <urn:mnemosyne:local:graph:g1> .\n",
            "<urn:s> <urn:p> <urn:o> <urn:mnemosyne:local:graph:g1> . garbage\n",
            "<urn:s> <urn:p> <urn:o> <urn:mnemosyne:local:graph:g1>",
        ] {
            let rdf =
                format!("<urn:s> <urn:p> <urn:o> <urn:mnemosyne:local:graph:g1> .\n{malformed}");
            let (kept, dropped) = filter_reserved_projection_quads(&rdf, "g1");
            assert_eq!(dropped, 1, "malformed line must not count as removed");
            assert_eq!(kept, malformed);
            let store = oxigraph::store::Store::new().unwrap();
            assert!(
                crate::rdf_query_service::load_rdf_dataset_into_store(
                    &store, "g1", &kept, "nquads", None,
                )
                .is_err(),
                "malformed input must still fail downstream: {malformed}"
            );
        }
    }

    #[test]
    fn reserved_graph_quads_are_dropped_and_user_quads_kept() {
        let rdf = concat!(
            // Root-graph quad: the materializer owns this graph — derived,
            // regenerates from the imported Y.Docs (hosted-cloud archives
            // keep their whole document projection here).
            "<urn:doc:a> <urn:p:title> \"Doc A\" <urn:mnemosyne:local:graph:g1> .\n",
            "<urn:mem:1> <urn:p:text> \"m\" <urn:mnemosyne:local:graph:g1:projection:memory> .\n",
            "<urn:doc:a> <urn:p:mentions> <urn:mnemosyne:local:graph:g1:projection:memory> <urn:mnemosyne:local:graph:g1:user:rdf> .\n",
            "<urn:doc:b> <urn:p:title> \"Default graph quad\" .\n",
        );
        let (kept, dropped) = filter_reserved_projection_quads(rdf, "g1");
        assert_eq!(dropped, 2, "root-graph AND projection-graph quads drop");
        assert!(
            !kept.contains("\"Doc A\""),
            "root-graph quads are derived state: {kept}"
        );
        assert!(
            kept.contains("<urn:p:mentions>"),
            "quad mentioning a reserved IRI as OBJECT is preserved"
        );
        assert!(kept.contains("\"Default graph quad\""));
        assert_eq!(kept.matches(" .").count(), 2);

        // Another graph's root is not reserved for g1.
        let other = "<urn:doc:c> <urn:p:t> \"other-graph quad\" <urn:mnemosyne:local:graph:g2> .\n";
        let (kept_other, dropped_other) = filter_reserved_projection_quads(other, "g1");
        assert_eq!(dropped_other, 0);
        assert!(kept_other.contains("\"other-graph quad\""));
    }

    #[test]
    fn local_scheme_rewrite_is_delimiter_anchored() {
        let manifest = GraphArchiveManifest {
            source_user_id: "default".to_string(),
            source_graph_id: "rt-src".to_string(),
            source_graph_title: None,
            source_graph_description: None,
            includes_artifacts: false,
            exported_at: None,
        };
        let rdf = concat!(
            "<urn:mnemosyne:local:graph:rt-src> <urn:p:a> \"x\" .\n",
            "<urn:mnemosyne:local:graph:rt-src:projection:memory> <urn:p:b> \"y\" .\n",
            "<urn:mnemosyne:local:graph:rt-src2> <urn:p:c> \"must not be corrupted\" .\n",
        );
        let rewritten = rewrite_graph_archive_rdf(rdf, &manifest, "rt-dst");
        assert!(rewritten.contains("<urn:mnemosyne:local:graph:rt-dst>"));
        assert!(rewritten.contains("<urn:mnemosyne:local:graph:rt-dst:projection:memory>"));
        assert!(
            rewritten.contains("<urn:mnemosyne:local:graph:rt-src2>"),
            "prefix-sharing graph id must survive: {rewritten}"
        );
    }

    #[test]
    fn workspace_rewrite_retargets_intra_graph_wires_and_leaves_anomalies() {
        let manifest = GraphArchiveManifest {
            source_user_id: "u1".to_string(),
            source_graph_id: "default".to_string(),
            source_graph_title: None,
            source_graph_description: None,
            includes_artifacts: false,
            exported_at: None,
        };
        let doc = Doc::new();
        {
            let mut txn = doc.transact_mut();
            let wires = txn.get_or_insert_map("wires");
            // Intra-graph wire: target is the source graph itself.
            let intra = wires.insert(&mut txn, "w-intra", yrs::MapPrelim::default());
            intra.insert(&mut txn, "targetGraphId", "default".to_string());
            // Cross-graph anomaly: target is some other graph.
            let cross = wires.insert(&mut txn, "w-cross", yrs::MapPrelim::default());
            cross.insert(&mut txn, "targetGraphId", "sophia-labs".to_string());
        }

        rewrite_graph_archive_workspace(&doc, &manifest, "default_8_25_26_3");

        let mut txn = doc.transact_mut();
        let wires = txn.get_or_insert_map("wires");
        let read_target = |txn: &yrs::TransactionMut, wire_id: &str| -> String {
            let Some(Out::YMap(wire)) = wires.get(txn, wire_id) else {
                panic!("missing wire {wire_id}");
            };
            match wire.get(txn, "targetGraphId") {
                Some(Out::Any(Any::String(value))) => value.to_string(),
                other => panic!("targetGraphId not a string: {other:?}"),
            }
        };
        assert_eq!(
            read_target(&txn, "w-intra"),
            "default_8_25_26_3",
            "intra-graph wire must be retargeted to the new graph id"
        );
        assert_eq!(
            read_target(&txn, "w-cross"),
            "sophia-labs",
            "cross-graph anomaly must be left untouched (fails gracefully, not rewritten)"
        );
    }
}


// Adapted from upstream 2074059 and abe5adc; owned-cell restore is not ported.
#[cfg(test)]
mod archive_expansion_tests {
    use super::*;
    use std::io::Write;
    fn gzip_for_limit_test(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }
    #[test]
    fn archive_compressed_input_size_is_inclusive_before_inflation() {
        // The production caller supplies the full input slice length, including
        // gzip headers. Exercise its admission arithmetic without allocating a
        // half-gigabyte attack fixture or claiming a total-process memory cap.
        assert!(require_compressed_archive_size(0).is_ok());
        assert!(require_compressed_archive_size(MAX_EXTRACTED_SIZE).is_ok());
        assert!(require_compressed_archive_size(MAX_EXTRACTED_SIZE + 1)
            .unwrap_err().contains("including its gzip header"));
    }
    #[test]
    fn archive_gzip_expansion_is_bounded_during_read_including_zero_header_tail() {
        let exact = vec![0u8; 1024];
        assert_eq!(read_gzip_bounded(&gzip_for_limit_test(&exact), 1024).unwrap(), exact);
        // The first zero tar header does not excuse inflating its following tail.
        let expanded_tail = vec![0u8; 4096];
        let compressed = gzip_for_limit_test(&expanded_tail);
        assert!(compressed.len() < 1024);
        assert!(read_gzip_bounded(&compressed, 1024).unwrap_err().contains("exceeds 1024 bytes"));
        assert!(read_gzip_bounded(&gzip_for_limit_test(&[0]), 0).is_err());
        assert!(read_gzip_bounded(&gzip_for_limit_test(&[]), 0).unwrap().is_empty());
        assert!(read_gzip_bounded(b"not gzip", 1024).is_err());
    }
    #[test]
    fn archive_tar_tail_is_zero_and_bounded() {
        assert!(read_tar_entries(&vec![0; MAX_TAR_TRAILING_BYTES]).unwrap().is_empty());
        assert!(read_tar_entries(&vec![0; MAX_TAR_TRAILING_BYTES + TAR_BLOCK_SIZE]).unwrap_err().contains("Invalid tar tail"));
        let mut tail = vec![0; TAR_BLOCK_SIZE * 2];
        tail[TAR_BLOCK_SIZE] = 1;
        assert!(read_tar_entries(&tail).unwrap_err().contains("Invalid tar tail"));
        assert!(read_tar_entries(&[1, 2, 3]).unwrap_err().contains("incomplete nonzero header"));
    }
}

/// Write a graph's contents from archive bytes the way an import does: the
/// workspace through save_workspace with its materialized snapshot (folders,
/// catalog, wires), then each document through create and save, so every
/// projection exists before anyone opens the graph. The graph must already
/// exist, and the caller holds its write lease. Used by the onboarding seed.
pub(crate) fn seed_graph_contents(
    app: &AppHandle,
    graph_id: &str,
    manifest: &GraphArchiveManifest,
    workspace_bytes: &[u8],
    documents: &[(String, Vec<u8>)],
    operation_id: &str,
) -> Result<(Vec<String>, Vec<String>), String> {
    let workspace_doc = Doc::new();
    apply_full_update(&workspace_doc, workspace_bytes, "workspace")?;
    rewrite_graph_archive_workspace(&workspace_doc, manifest, graph_id);
    let snapshot = super::workspace_ops::materialize_workspace_snapshot_json(graph_id, &workspace_doc)?;
    let mut catalog: HashMap<String, Value> = HashMap::new();
    if let Some(list) = snapshot.get("documents").and_then(Value::as_array) {
        for entry in list {
            if let Some(id) = entry.get("id").and_then(Value::as_str) {
                catalog.insert(id.to_string(), entry.clone());
            }
        }
    }
    let save_input: crate::document_types::SaveWorkspaceInput = serde_json::from_value(json!({
        "graphId": graph_id,
        "ydocUpdateBase64": encode_bytes_base64(&encode_full_state(&workspace_doc)),
        "snapshot": snapshot,
        "traceOperationId": operation_id,
    }))
    .map_err(|error| format!("assemble save_workspace input: {error}"))?;
    crate::document_persistence_service::save_workspace_with_lease(app.clone(), save_input)?;

    let mut seeded = Vec::new();
    let mut warnings = Vec::new();
    for (document_id, bytes) in documents {
        seeded.push(import_one_document(
            app,
            graph_id,
            document_id,
            bytes,
            catalog.get(document_id),
            None,
            operation_id,
            &mut warnings,
        )?);
    }
    Ok((seeded, warnings))
}
