//! Regions on artifact wires (step 3 of "Artifacts — a body outside the
//! house"): a wire to an artifact may name a part of it, and the graph
//! remembers what that part said.
//!
//! The wire itself carries only `targetRegion` and `fileHash` (both public
//! to `documents.read`: neither is file text). The region's text and its
//! hash live in a side store, one file per wire under
//! `graphs/{g}/artifact-regions/{wireId}.json`, read only through
//! `read_artifact` (gated by `artifacts.read`) and hard-deleted with the
//! wire. The invariant sits in the one write path: the `workspace.createWire`
//! operation validates the region against the bytes, sets `fileHash`
//! itself, and writes the side-store file after the commit; every path that
//! removes a wire ends in the workspace persist, which sweeps the files of
//! wires that are gone (Eschaton's ruling: hard deletes, not tombstones).
//!
//! Bytes always come through the same gate the readers use: the storage
//! resolver for owned and borrowed records, the roots and the no-follow walk
//! for referenced ones. Nothing here reads a whole large file: lines are
//! scanned up to a cap, byte ranges are seeked to.

use crate::app_runtime::AppHandle;
use crate::{
    artifact_kinds::kind_from_mime,
    artifact_reference::{looks_like_text, record_is_referenced, REFERENCE_HASH_CAP_BYTES},
    artifact_roots::{authorize, open_authorized, RootsDenial},
    clock::timestamp,
    crdt_engine::workspace_ops::artifact_file_type,
    ids::{local_upload_mime_type_for_filename, validate_local_id},
    json_utils::json_string,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

/// Region text kept in the side store and shown by `read_artifact`.
pub(crate) const REGION_TEXT_CAP_CHARS: usize = 2048;
/// A line region is found by scanning from the top of the file; past this
/// many bytes the region is refused rather than read.
pub(crate) const LINE_SCAN_CAP_BYTES: u64 = 64 * 1024 * 1024;
/// A byte region is hashed whole; above this it is refused.
pub(crate) const BYTE_RANGE_CAP: u64 = 64 * 1024 * 1024;
/// A JSON pointer is resolved only in a document this small (the light
/// index's parse cap).
const JSON_POINTER_PARSE_CAP: u64 = 8 * 1024 * 1024;
/// Re-anchoring reads the normalized file once, up to this size and this
/// many lines; past either it reports that it could not look.
const REANCHOR_SCAN_CAP_BYTES: u64 = 16 * 1024 * 1024;
const REANCHOR_LINE_CAP: usize = 200_000;
/// A line region may span at most this many lines.
pub(crate) const LINE_REGION_CAP: u64 = 100_000;
/// The sweep leaves a side-store file this young alone: the workspace
/// persist judges files against a snapshot materialized a moment earlier,
/// and a wire committed in between is live although the snapshot lacks it
/// (angel review, 2026-10-30). The next persist sees it.
const SWEEP_GRACE_SECS: u64 = 120;

/// One kind per wire, exactly as the frozen shape names them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Region {
    /// 1-based, inclusive.
    TextLines { start: u64, end: u64 },
    /// 1-based.
    PdfPage { page: u64 },
    /// Zeta's `page=2&start=14&end=29` string, unchanged.
    PdfSelector { selector: String },
    /// 0-based, start inclusive, end exclusive.
    Bytes { start: u64, end: u64 },
    /// A zip member's path.
    Member { path: String },
    /// An RFC 6901 pointer into a JSON document.
    JsonPointer { pointer: String },
}

impl Region {
    pub(crate) fn to_json(&self) -> Value {
        match self {
            Region::TextLines { start, end } => json!({ "kind": "text-lines", "start": start, "end": end }),
            Region::PdfPage { page } => json!({ "kind": "pdf-page", "page": page }),
            Region::PdfSelector { selector } => json!({ "kind": "pdf-selector", "selector": selector }),
            Region::Bytes { start, end } => json!({ "kind": "bytes", "start": start, "end": end }),
            Region::Member { path } => json!({ "kind": "member", "path": path }),
            Region::JsonPointer { pointer } => json!({ "kind": "json-pointer", "pointer": pointer }),
        }
    }

    /// The short form the link syntax uses: `L10-L20`, `p12`, `b100-200`,
    /// `m:path/in/zip`, `j:/conversations/3`.
    pub(crate) fn short_form(&self) -> String {
        match self {
            Region::TextLines { start, end } => format!("L{start}-L{end}"),
            Region::PdfPage { page } => format!("p{page}"),
            Region::PdfSelector { selector } => format!("s:{selector}"),
            Region::Bytes { start, end } => format!("b{start}-{end}"),
            Region::Member { path } => format!("m:{path}"),
            Region::JsonPointer { pointer } => format!("j:{pointer}"),
        }
    }

    /// Whether a line region holds the line (None for the other kinds).
    pub(crate) fn contains_line(&self, line: u64) -> Option<bool> {
        match self {
            Region::TextLines { start, end } => Some(*start <= line && line <= *end),
            _ => None,
        }
    }

    /// Parse the wire-level object or the short form string.
    pub(crate) fn parse(value: &Value) -> Result<Region, String> {
        match value {
            Value::String(short) => Region::parse_short(short),
            Value::Object(_) => Region::parse_object(value),
            _ => Err("a region is an object { kind, ... } or a short form such as L10-L20".to_string()),
        }
    }

    pub(crate) fn parse_object(value: &Value) -> Result<Region, String> {
        let kind = json_string(value.get("kind")).ok_or("region.kind is required")?;
        let number = |key: &str| -> Result<u64, String> {
            value
                .get(key)
                .and_then(Value::as_u64)
                .ok_or_else(|| format!("region.{key} must be a non-negative integer for kind {kind}"))
        };
        let text = |key: &str| -> Result<String, String> {
            json_string(value.get(key))
                .map(|text| text.trim().to_string())
                .filter(|text| !text.is_empty())
                .ok_or_else(|| format!("region.{key} is required for kind {kind}"))
        };
        let region = match kind.as_str() {
            "text-lines" => Region::TextLines { start: number("start")?, end: number("end")? },
            "pdf-page" => Region::PdfPage { page: number("page")? },
            "pdf-selector" => Region::PdfSelector { selector: text("selector")? },
            "bytes" => Region::Bytes { start: number("start")?, end: number("end")? },
            "member" => Region::Member { path: text("path")? },
            "json-pointer" => Region::JsonPointer {
                pointer: json_string(value.get("pointer")).ok_or("region.pointer is required for kind json-pointer")?,
            },
            other => {
                return Err(format!(
                    "unknown region kind {other}; one of text-lines, pdf-page, pdf-selector, bytes, member, json-pointer"
                ))
            }
        };
        region.validate_shape()?;
        Ok(region)
    }

    pub(crate) fn parse_short(short: &str) -> Result<Region, String> {
        let short = short.trim();
        let range = |text: &str, what: &str| -> Result<(u64, u64), String> {
            let (a, b) = text
                .split_once('-')
                .ok_or_else(|| format!("{what} range needs a dash: {short}"))?;
            let parse = |part: &str| part.trim().trim_start_matches('L').parse::<u64>().map_err(|_| format!("{what} range is not numeric: {short}"));
            Ok((parse(a)?, parse(b)?))
        };
        let region = if let Some(rest) = short.strip_prefix('L') {
            if rest.contains('-') {
                let (start, end) = range(rest, "line")?;
                Region::TextLines { start, end }
            } else {
                let line = rest.parse::<u64>().map_err(|_| format!("line is not numeric: {short}"))?;
                Region::TextLines { start: line, end: line }
            }
        } else if let Some(rest) = short.strip_prefix("m:") {
            Region::Member { path: rest.trim().to_string() }
        } else if let Some(rest) = short.strip_prefix("j:") {
            Region::JsonPointer { pointer: rest.trim().to_string() }
        } else if let Some(rest) = short.strip_prefix("s:") {
            Region::PdfSelector { selector: rest.trim().to_string() }
        } else if short.contains("page=") {
            // Zeta's selector string, whole.
            Region::PdfSelector { selector: short.to_string() }
        } else if let Some(rest) = short.strip_prefix('p') {
            Region::PdfPage { page: rest.parse::<u64>().map_err(|_| format!("page is not numeric: {short}"))? }
        } else if let Some(rest) = short.strip_prefix('b') {
            let (start, end) = range(rest, "byte")?;
            Region::Bytes { start, end }
        } else {
            return Err(format!(
                "unrecognized region {short}; use L10-L20 (lines), p12 (pdf page), b100-200 (bytes), m:path (zip member) or j:/pointer (json)"
            ));
        };
        region.validate_shape()?;
        Ok(region)
    }

    fn validate_shape(&self) -> Result<(), String> {
        match self {
            Region::TextLines { start, end } => {
                if *start == 0 {
                    return Err("text-lines are 1-based; start must be at least 1".to_string());
                }
                if end < start {
                    return Err(format!("text-lines end {end} is before start {start}"));
                }
                if end - start >= LINE_REGION_CAP {
                    return Err(format!("a line region spans at most {LINE_REGION_CAP} lines"));
                }
            }
            Region::PdfPage { page } if *page == 0 => {
                return Err("pdf pages are 1-based; page must be at least 1".to_string())
            }
            Region::Bytes { start, end } => {
                if end <= start {
                    return Err(format!("bytes end {end} must be after start {start} (end is exclusive)"));
                }
                if end - start > BYTE_RANGE_CAP {
                    return Err(format!("a byte region is at most {BYTE_RANGE_CAP} bytes"));
                }
            }
            Region::Member { path } => {
                if path.starts_with('/') || path.split('/').any(|segment| segment == ".." || segment.is_empty()) {
                    return Err(format!("member path must be relative inside the archive, without empty or .. segments: {path}"));
                }
            }
            Region::JsonPointer { pointer } => {
                if !pointer.is_empty() && !pointer.starts_with('/') {
                    return Err(format!("a json pointer is empty or starts with /: {pointer}"));
                }
            }
            Region::PdfSelector { selector } if selector.len() > 512 => {
                return Err("pdf selector is longer than 512 characters".to_string())
            }
            _ => {}
        }
        Ok(())
    }
}

/// What the capture or a later read saw of the region.
#[derive(Debug, Clone, Default)]
pub(crate) struct RegionRead {
    pub(crate) region_hash: Option<String>,
    pub(crate) region_text: Option<String>,
    /// The region's whole text before the cap, when it is text (for the
    /// reader's paging); None for regions with no text.
    pub(crate) full_text: Option<String>,
    pub(crate) notes: Vec<String>,
}

/// An artifact's bytes opened through the gate its mode requires, plus
/// what the reader needs to know about them.
pub(crate) struct ArtifactBytes {
    pub(crate) file: std::fs::File,
    pub(crate) len: u64,
    pub(crate) filename: String,
    pub(crate) mime_type: String,
    pub(crate) kind: &'static str,
    pub(crate) referenced: bool,
    /// The store's current hash for owned and borrowed bytes; None for a
    /// reference (computed on demand by `current_file_hash`).
    store_hash: Option<String>,
}

/// Open an artifact's bytes now, the way every reader does. A reference is
/// re-authorized against the roots and opened through the no-follow walk;
/// an owned or borrowed record's bytes come from the storage resolver.
pub(crate) fn open_artifact_bytes(
    app: &AppHandle,
    graph_id: &str,
    artifact_id: &str,
    record: &Value,
) -> Result<ArtifactBytes, String> {
    if record_is_referenced(record) {
        let path = record
            .get("source")
            .and_then(|source| json_string(source.get("path")))
            .ok_or("the referenced record names no path")?;
        let authorized = authorize(app, graph_id, Path::new(&path)).map_err(|denial| denial.to_string())?;
        if authorized.canonical != Path::new(&path) {
            return Err(format!("denied: {path} now resolves to {}", authorized.canonical.display()));
        }
        let file = open_authorized(&authorized).map_err(|denial: RootsDenial| denial.to_string())?;
        let len = file.metadata().map_err(|error| format!("stat referenced file: {error}"))?.len();
        let filename = authorized
            .canonical
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string)
            .unwrap_or_else(|| "unnamed".to_string());
        let mime_type = json_string(record.get("mimeType"))
            .filter(|mime| mime != "application/octet-stream")
            .unwrap_or_else(|| local_upload_mime_type_for_filename(&filename));
        let kind = kind_from_mime(&mime_type, Some(&artifact_file_type(&filename)));
        return Ok(ArtifactBytes { file, len, filename, mime_type, kind, referenced: true, store_hash: None });
    }
    let original_dir = crate::artifact_storage::resolve_artifact_original_dir(app, graph_id, artifact_id)?;
    let manifest = crate::original_file_manifest_store::read_original_manifest(&original_dir)?;
    let path = crate::original_file_manifest_store::original_manifest_file_path(&original_dir, &manifest.filename)?;
    let file = std::fs::File::open(&path).map_err(|error| format!("open artifact original: {error}"))?;
    let len = file.metadata().map_err(|error| format!("stat artifact original: {error}"))?.len();
    let kind = kind_from_mime(&manifest.mime_type, Some(&artifact_file_type(&manifest.filename)));
    Ok(ArtifactBytes {
        file,
        len,
        filename: manifest.filename.clone(),
        mime_type: manifest.mime_type.clone(),
        kind,
        referenced: false,
        store_hash: manifest.sha256.clone().or_else(|| json_string(record.get("sha256"))),
    })
}

impl ArtifactBytes {
    /// The file's hash as it is now: the store's for owned and borrowed
    /// bytes, computed by one bounded read for a reference (None above the
    /// reference hash cap, as `read_artifact` already reports).
    pub(crate) fn current_file_hash(&mut self) -> Result<Option<String>, String> {
        if !self.referenced {
            return Ok(self.store_hash.clone());
        }
        if self.len > REFERENCE_HASH_CAP_BYTES {
            return Ok(None);
        }
        // One hash per file state per process: a pull wires up to 2,000
        // blocks, each through capture(), and the file does not change
        // between them unless its length, mtime or inode does (angel
        // review). The key is the open descriptor's metadata, never a path.
        let key = reference_hash_key(&self.file);
        if let Some(key) = &key {
            if let Some(hash) = reference_hash_cache().lock().ok().and_then(|cache| cache.get(key).cloned()) {
                return Ok(Some(hash));
            }
        }
        self.file.seek(SeekFrom::Start(0)).map_err(|error| format!("seek: {error}"))?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; 1 << 20];
        loop {
            let read = self.file.read(&mut buffer).map_err(|error| format!("read referenced file: {error}"))?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        self.file.seek(SeekFrom::Start(0)).map_err(|error| format!("seek: {error}"))?;
        let hash = format!("{:x}", hasher.finalize());
        if let (Some(key), Ok(mut cache)) = (key, reference_hash_cache().lock()) {
            if cache.len() >= 256 {
                cache.clear();
            }
            cache.insert(key, hash.clone());
        }
        Ok(Some(hash))
    }

    /// Whether the bytes read as text: the declared kind, or a UTF-8 window
    /// with no NUL for files whose mime type says nothing.
    fn is_text(&mut self) -> Result<bool, String> {
        if matches!(self.kind, "text" | "scene") {
            return Ok(true);
        }
        if matches!(self.kind, "pdf" | "image" | "audio" | "video") {
            return Ok(false);
        }
        self.file.seek(SeekFrom::Start(0)).map_err(|error| format!("seek: {error}"))?;
        let mut window = vec![0u8; 8192.min(self.len as usize)];
        self.file.read_exact(&mut window).map_err(|error| format!("read window: {error}"))?;
        self.file.seek(SeekFrom::Start(0)).map_err(|error| format!("seek: {error}"))?;
        // A window cut inside a multi-byte character is still text.
        let valid_to = match std::str::from_utf8(&window) {
            Ok(_) => window.len(),
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            Err(_) => 0,
        };
        Ok(valid_to > 0 && looks_like_text(&window[..valid_to]))
    }

    /// Validate a region against this artifact's kind and extent, and read
    /// it: the text and hash the side store keeps.
    pub(crate) fn read_region(&mut self, region: &Region) -> Result<RegionRead, String> {
        match region {
            Region::TextLines { start, end } => {
                if !self.is_text()? {
                    return Err(format!("text-lines needs a text artifact; {} reads as {}", self.filename, self.kind));
                }
                let lines = self.read_lines(*start, *end)?;
                Ok(text_read(lines.join("\n")))
            }
            Region::Bytes { start, end } => {
                if *end > self.len {
                    return Err(format!("bytes {start}-{end} are out of range: the file is {} bytes long", self.len));
                }
                self.file.seek(SeekFrom::Start(*start)).map_err(|error| format!("seek: {error}"))?;
                let mut bytes = vec![0u8; (end - start) as usize];
                self.file.read_exact(&mut bytes).map_err(|error| format!("read byte region: {error}"))?;
                let region_hash = Some(format!("{:x}", Sha256::digest(&bytes)));
                let text = looks_like_text(&bytes).then(|| String::from_utf8_lossy(&bytes).to_string());
                let region_text = text.as_deref().map(cap_text);
                Ok(RegionRead { region_hash, region_text, full_text: text, notes: Vec::new() })
            }
            Region::PdfPage { page } => {
                if self.kind != "pdf" {
                    return Err(format!("pdf-page needs a PDF; {} is {}", self.filename, self.kind));
                }
                Ok(RegionRead {
                    notes: vec![format!("pdf page {page}: the page's text is not extracted here; the PDF reader shows it")],
                    ..Default::default()
                })
            }
            Region::PdfSelector { .. } => {
                if self.kind != "pdf" {
                    return Err(format!("pdf-selector needs a PDF; {} is {}", self.filename, self.kind));
                }
                Ok(RegionRead::default())
            }
            Region::Member { path } => {
                let file_type = artifact_file_type(&self.filename);
                if !(file_type == "zip" || self.mime_type.contains("zip")) {
                    return Err(format!("member needs a zip archive; {} is {}", self.filename, self.kind));
                }
                Ok(RegionRead {
                    notes: vec![format!("zip member {path} is addressed, not opened, in this step")],
                    ..Default::default()
                })
            }
            Region::JsonPointer { pointer } => {
                let file_type = artifact_file_type(&self.filename);
                if !(file_type == "json" || self.mime_type.contains("json")) {
                    return Err(format!("json-pointer needs a JSON document; {} is {}", self.filename, self.kind));
                }
                if self.len > JSON_POINTER_PARSE_CAP {
                    return Ok(RegionRead {
                        notes: vec![format!("json document is over {JSON_POINTER_PARSE_CAP} bytes; the pointer is recorded, not resolved")],
                        ..Default::default()
                    });
                }
                self.file.seek(SeekFrom::Start(0)).map_err(|error| format!("seek: {error}"))?;
                let mut bytes = Vec::with_capacity(self.len as usize);
                self.file.read_to_end(&mut bytes).map_err(|error| format!("read json: {error}"))?;
                let document: Value = serde_json::from_slice(&bytes).map_err(|error| format!("the artifact is not valid JSON: {error}"))?;
                let pointed = document
                    .pointer(pointer)
                    .ok_or_else(|| format!("json pointer {pointer} resolves to nothing in {}", self.filename))?;
                let text = match pointed {
                    Value::String(text) => text.clone(),
                    other => serde_json::to_string_pretty(other).unwrap_or_default(),
                };
                // The pointed value's text is the region's bytes here, so
                // its hash reports region staleness like a line region's.
                Ok(text_read(text))
            }
        }
    }

    /// Lines `start..=end` (1-based), scanned from the top, BOM stripped,
    /// CRLF folded, decoded lossily. Out of range refuses with the file's
    /// actual line count.
    fn read_lines(&mut self, start: u64, end: u64) -> Result<Vec<String>, String> {
        self.file.seek(SeekFrom::Start(0)).map_err(|error| format!("seek: {error}"))?;
        let scan_cap = LINE_SCAN_CAP_BYTES.min(self.len);
        let mut reader = BufReader::new((&self.file).take(scan_cap));
        let mut lines = Vec::with_capacity((end - start + 1).min(10_000) as usize);
        let mut line_number: u64 = 0;
        let mut raw = Vec::new();
        let mut scanned: u64 = 0;
        loop {
            raw.clear();
            let read = reader.read_until(b'\n', &mut raw).map_err(|error| format!("read lines: {error}"))?;
            if read == 0 {
                break;
            }
            scanned += read as u64;
            line_number += 1;
            if line_number >= start {
                lines.push(normalize_line(&raw, line_number == 1));
            }
            if line_number == end {
                return Ok(lines);
            }
        }
        if scanned >= scan_cap && self.len > scan_cap {
            return Err(format!(
                "line {end} is beyond the first {scan_cap} bytes of {}; line regions are read only in that window",
                self.filename
            ));
        }
        Err(format!(
            "lines {start}-{end} are out of range: {} has {line_number} lines",
            self.filename
        ))
    }

    /// The whole normalized text (bounded) for re-anchoring; None when the
    /// file is too long to search.
    fn normalized_lines(&mut self) -> Result<Option<Vec<String>>, String> {
        if self.len > REANCHOR_SCAN_CAP_BYTES {
            return Ok(None);
        }
        self.file.seek(SeekFrom::Start(0)).map_err(|error| format!("seek: {error}"))?;
        let mut reader = BufReader::new(&self.file);
        let mut lines = Vec::new();
        let mut raw = Vec::new();
        let mut first = true;
        loop {
            raw.clear();
            let read = reader.read_until(b'\n', &mut raw).map_err(|error| format!("read lines: {error}"))?;
            if read == 0 {
                break;
            }
            if lines.len() >= REANCHOR_LINE_CAP {
                return Ok(None);
            }
            lines.push(normalize_line(&raw, first));
            first = false;
        }
        Ok(Some(lines))
    }
}

#[derive(Hash, PartialEq, Eq, Clone)]
struct ReferenceHashKey {
    device: u64,
    inode: u64,
    len: u64,
    modified_ns: u128,
}

fn reference_hash_cache() -> &'static std::sync::Mutex<std::collections::HashMap<ReferenceHashKey, String>> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<ReferenceHashKey, String>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

#[cfg(unix)]
fn reference_hash_key(file: &std::fs::File) -> Option<ReferenceHashKey> {
    use std::os::unix::fs::MetadataExt;
    let meta = file.metadata().ok()?;
    let modified = meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos();
    Some(ReferenceHashKey { device: meta.dev(), inode: meta.ino(), len: meta.len(), modified_ns: modified })
}

#[cfg(not(unix))]
fn reference_hash_key(_file: &std::fs::File) -> Option<ReferenceHashKey> {
    None
}

fn normalize_line(raw: &[u8], first: bool) -> String {
    let mut bytes = raw;
    if first && bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        bytes = &bytes[3..];
    }
    if bytes.ends_with(b"\n") {
        bytes = &bytes[..bytes.len() - 1];
    }
    if bytes.ends_with(b"\r") {
        bytes = &bytes[..bytes.len() - 1];
    }
    String::from_utf8_lossy(bytes).to_string()
}

fn cap_text(text: &str) -> String {
    text.chars().take(REGION_TEXT_CAP_CHARS).collect()
}

fn text_read(text: String) -> RegionRead {
    RegionRead {
        region_hash: Some(format!("{:x}", Sha256::digest(text.as_bytes()))),
        region_text: Some(cap_text(&text)),
        full_text: Some(text),
        notes: Vec::new(),
    }
}

/// Whether the artifact's declared kind can carry this region at all; the
/// bytes decide the rest in `read_region`.
pub(crate) fn region_fits_record(region: &Region, record: &Value) -> Result<(), String> {
    if let Region::PdfPage { page } = region {
        let pages = record.get("index").and_then(|index| index.get("pages")).and_then(Value::as_u64);
        if let Some(pages) = pages {
            if *page > pages {
                return Err(format!("pdf page {page} is out of range: the index counts {pages} pages"));
            }
        }
    }
    Ok(())
}

/// Whether a createWire payload names a region (the doors that reach the
/// bytes ask for `artifacts.read` when it does).
pub(crate) fn payload_names_region(payload: &Value) -> bool {
    ["targetRegion", "target_region", "region"]
        .iter()
        .any(|key| payload.get(*key).is_some_and(|value| !value.is_null()))
}

// ---------------------------------------------------------------------------
// The side store
// ---------------------------------------------------------------------------

/// One file per wire: what the region said when the wire was made.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoredRegion {
    pub(crate) wire_id: String,
    pub(crate) artifact_id: String,
    pub(crate) region: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) file_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) region_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) region_text: Option<String>,
    pub(crate) captured_at: String,
}

pub(crate) const REGIONS_DIR: &str = "artifact-regions";

pub(crate) fn regions_dir(graph_dir: &Path) -> PathBuf {
    graph_dir.join(REGIONS_DIR)
}

pub(crate) fn region_path(graph_dir: &Path, wire_id: &str) -> Result<PathBuf, String> {
    validate_local_id(wire_id, "wire_id")?;
    Ok(regions_dir(graph_dir).join(format!("{wire_id}.json")))
}

/// Whether a wire id can have a side-store file at all (an id with other
/// characters never got one, so for it there is nothing to read or remove).
fn can_have_region_file(wire_id: &str) -> bool {
    validate_local_id(wire_id, "wire_id").is_ok()
}

/// Write the side-store file for a wire, under the durability guard.
pub(crate) fn write_stored_region(graph_dir: &Path, stored: &StoredRegion) -> Result<(), String> {
    let _flush_guard = crate::cell_durability::write_guard();
    let path = region_path(graph_dir, &stored.wire_id)?;
    crate::storage::create_dir_all(path.parent().ok_or("region path")?).map_err(|error| error.to_string())?;
    crate::storage::write_json(&path, stored).map_err(|error| error.to_string())
}

/// The stored record; Ok(None) when there is none, Err when a file is
/// there but cannot be read (the reader degrades, it does not fail).
pub(crate) fn read_stored_region(graph_dir: &Path, wire_id: &str) -> Result<Option<StoredRegion>, String> {
    if !can_have_region_file(wire_id) {
        return Ok(None);
    }
    let path = region_path(graph_dir, wire_id)?;
    if !path.is_file() {
        return Ok(None);
    }
    crate::storage::read_json::<StoredRegion>(&path)
        .map(Some)
        .map_err(|error| error.to_string())
}

/// Remove a wire's side-store file; false when there was none. Never an
/// error for an id that cannot have one (a legacy wire id with other
/// characters deletes like before this step).
pub(crate) fn remove_stored_region(graph_dir: &Path, wire_id: &str) -> Result<bool, String> {
    if !can_have_region_file(wire_id) {
        return Ok(false);
    }
    let _flush_guard = crate::cell_durability::write_guard();
    let path = region_path(graph_dir, wire_id)?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("remove region file {}: {error}", path.display())),
    }
}

/// The orphan rule, run at every workspace persist: a region file whose
/// wire is no longer in the snapshot (or no longer points at an artifact)
/// is removed. Every wire removal, whatever its path, ends here.
pub(crate) fn sweep_orphans(graph_dir: &Path, snapshot: &Value) -> usize {
    let dir = regions_dir(graph_dir);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return 0;
    };
    let live: std::collections::HashSet<String> = snapshot
        .get("wires")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|wire| json_string(wire.get("targetArtifactId")).is_some())
        .filter_map(|wire| json_string(wire.get("id")))
        .collect();
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if path.extension().and_then(|ext| ext.to_str()) != Some("json")
            || !can_have_region_file(stem)
            || live.contains(stem)
        {
            continue;
        }
        // Young files belong to wires the snapshot may not show yet.
        let young = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age.as_secs() < SWEEP_GRACE_SECS);
        if young {
            continue;
        }
        let _flush_guard = crate::cell_durability::write_guard();
        if std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// The side-store files an export may carry: those whose artifact is not
/// a reference. A reference's region text is text from outside the house
/// and does not travel in an archive (the import drops the reference too);
/// its hashes alone go, so a round trip keeps the staleness baseline.
pub(crate) fn export_entries(graph_dir: &Path, referenced_artifact_ids: &std::collections::HashSet<String>) -> Vec<(String, Vec<u8>)> {
    let dir = regions_dir(graph_dir);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") || !can_have_region_file(stem) {
            continue;
        }
        let Ok(mut stored) = crate::storage::read_json::<StoredRegion>(&path) else {
            continue;
        };
        if referenced_artifact_ids.contains(&stored.artifact_id) {
            stored.region_text = None;
        }
        if let Ok(bytes) = serde_json::to_vec_pretty(&stored) {
            out.push((format!("{REGIONS_DIR}/{stem}.json"), bytes));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

// ---------------------------------------------------------------------------
// Capture (at wiring) and the read with authority (read_artifact)
// ---------------------------------------------------------------------------

/// What the createWire operation stores: the validated region, the hash the
/// wire carries, and the side-store record to write after the commit.
pub(crate) struct Capture {
    pub(crate) region: Region,
    pub(crate) file_hash: Option<String>,
    pub(crate) stored: StoredRegion,
}

/// Validate a region against the artifact's bytes and capture it. Refuses
/// a region the artifact cannot carry (kind or range), and a reference
/// whose root is gone (the open is the gate).
pub(crate) fn capture(
    app: &AppHandle,
    graph_id: &str,
    wire_id: &str,
    artifact_id: &str,
    record: &Value,
    region: Region,
) -> Result<Capture, String> {
    validate_local_id(wire_id, "wireId")?;
    region_fits_record(&region, record)?;
    let mut bytes = open_artifact_bytes(app, graph_id, artifact_id, record)?;
    let read = bytes.read_region(&region)?;
    let file_hash = bytes.current_file_hash()?;
    let stored = StoredRegion {
        wire_id: wire_id.to_string(),
        artifact_id: artifact_id.to_string(),
        region: region.to_json(),
        file_hash: file_hash.clone(),
        region_hash: read.region_hash,
        region_text: read.region_text,
        captured_at: timestamp(),
    };
    Ok(Capture { region, file_hash, stored })
}

/// The region now, beside what was stored: `stale: { file, region }`, both
/// hashes, the current text, and when the text has moved, a re-anchor by
/// the stored text (one match reports the new lines, several report
/// `ambiguous` with the candidates, none reports `anchorLost`).
pub(crate) fn read_region_now(
    app: &AppHandle,
    graph_id: &str,
    artifact_id: &str,
    record: &Value,
    region: &Region,
    stored: Option<&StoredRegion>,
) -> Result<Value, String> {
    let mut bytes = open_artifact_bytes(app, graph_id, artifact_id, record)?;
    let file_hash_now = bytes.current_file_hash()?;
    let mut out = json!({
        "region": region.to_json(),
        "regionShort": region.short_form(),
        "current": { "fileHash": file_hash_now },
    });
    let read = match bytes.read_region(region) {
        Ok(read) => Some(read),
        Err(error) => {
            out["regionError"] = json!(error);
            None
        }
    };
    if let Some(read) = &read {
        out["current"]["regionHash"] = json!(read.region_hash);
        if let Some(text) = &read.full_text {
            out["regionText"] = json!(text);
        }
        if !read.notes.is_empty() {
            out["notes"] = json!(read.notes);
        }
    }
    let Some(stored) = stored else {
        return Ok(out);
    };
    out["stored"] = json!({
        "fileHash": stored.file_hash,
        "regionHash": stored.region_hash,
        "regionText": stored.region_text,
        "capturedAt": stored.captured_at,
    });
    let file_stale = match (&stored.file_hash, &file_hash_now) {
        (Some(stored), Some(now)) => Some(stored != now),
        _ => None,
    };
    let region_stale = match (&stored.region_hash, read.as_ref().and_then(|read| read.region_hash.as_ref())) {
        (Some(stored), Some(now)) => Some(stored != now),
        (Some(_), None) => Some(true),
        _ => None,
    };
    out["stale"] = json!({ "file": file_stale, "region": region_stale });
    // Re-anchor when the region's words moved: only for line regions, and
    // only by the text that was stored.
    if region_stale == Some(true) {
        if let (Region::TextLines { start, end }, Some(stored_text)) = (region, stored.region_text.as_deref()) {
            let wanted: Vec<&str> = stored_text.split('\n').collect();
            let original_len = (end - start + 1) as usize;
            let truncated = stored_text.chars().count() >= REGION_TEXT_CAP_CHARS;
            let Some(lines) = bytes.normalized_lines()? else {
                out["anchorSearch"] = json!("skipped: the file is too long to search for the stored text");
                return Ok(out);
            };
            let candidates = find_anchors(&lines, &wanted, truncated);
            match candidates.len() {
                0 => out["anchorLost"] = json!(true),
                1 => {
                    let new_start = candidates[0] + 1;
                    out["reanchored"] = json!({ "start": new_start, "end": new_start + original_len - 1 });
                }
                _ => {
                    out["ambiguous"] = json!(true);
                    out["candidates"] = json!(candidates
                        .iter()
                        .take(20)
                        .map(|index| json!({ "start": index + 1, "end": index + original_len }))
                        .collect::<Vec<_>>());
                }
            }
        }
    }
    Ok(out)
}

/// Every 0-based line index where `wanted` occurs in `lines`; the last
/// wanted line matches as a prefix when the stored text was cut at the cap.
fn find_anchors(lines: &[String], wanted: &[&str], truncated: bool) -> Vec<usize> {
    if wanted.is_empty() || wanted.len() > lines.len() {
        return Vec::new();
    }
    let last = wanted.len() - 1;
    (0..=lines.len() - wanted.len())
        .filter(|index| {
            wanted.iter().enumerate().all(|(offset, want)| {
                let have = lines[index + offset].as_str();
                if offset == last && truncated {
                    have.starts_with(want)
                } else {
                    have == *want
                }
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_forms_parse_to_the_six_kinds_and_back() {
        assert_eq!(Region::parse_short("L10-L20").unwrap(), Region::TextLines { start: 10, end: 20 });
        assert_eq!(Region::parse_short("L7").unwrap(), Region::TextLines { start: 7, end: 7 });
        assert_eq!(Region::parse_short("p12").unwrap(), Region::PdfPage { page: 12 });
        assert_eq!(Region::parse_short("b100-200").unwrap(), Region::Bytes { start: 100, end: 200 });
        assert_eq!(Region::parse_short("m:a/b.txt").unwrap(), Region::Member { path: "a/b.txt".into() });
        assert_eq!(Region::parse_short("j:/conversations/3").unwrap(), Region::JsonPointer { pointer: "/conversations/3".into() });
        assert_eq!(
            Region::parse_short("page=2&start=14&end=29").unwrap(),
            Region::PdfSelector { selector: "page=2&start=14&end=29".into() }
        );
        for short in ["L10-L20", "p12", "b100-200", "m:a/b.txt", "j:/x"] {
            let region = Region::parse_short(short).unwrap();
            assert_eq!(region.short_form(), short);
            assert_eq!(Region::parse_object(&region.to_json()).unwrap(), region);
        }
        assert!(Region::parse_short("L0-L3").is_err());
        assert!(Region::parse_short("L9-L3").is_err());
        assert!(Region::parse_short("b5-5").is_err());
        assert!(Region::parse_short("m:../etc").is_err());
        assert!(Region::parse_short("x1").is_err());
        assert!(Region::parse(&json!({ "kind": "pdf-page", "page": 0 })).is_err());
        assert!(Region::parse(&json!({ "kind": "nope" })).is_err());
    }

    #[test]
    fn lines_are_normalized_and_anchors_are_found_or_ambiguous() {
        assert_eq!(normalize_line(b"\xEF\xBB\xBFhello\r\n", true), "hello");
        assert_eq!(normalize_line(b"hello\r\n", false), "hello");
        assert_eq!(normalize_line(b"tail", false), "tail");
        let lines: Vec<String> = ["a", "b", "c", "a", "b", "d"].iter().map(|s| s.to_string()).collect();
        assert_eq!(find_anchors(&lines, &["b", "c"], false), vec![1]);
        assert_eq!(find_anchors(&lines, &["a", "b"], false), vec![0, 3]);
        assert!(find_anchors(&lines, &["z"], false).is_empty());
        assert_eq!(find_anchors(&lines, &["c", "a", "b"], true), vec![2]);
        // A truncated last line matches as a prefix.
        let lines: Vec<String> = ["one", "two words here"].iter().map(|s| s.to_string()).collect();
        assert_eq!(find_anchors(&lines, &["one", "two wo"], true), vec![0]);
        assert!(find_anchors(&lines, &["one", "two wo"], false).is_empty());
    }

    #[test]
    fn region_files_live_under_the_graph_and_orphans_are_swept() {
        let graph_dir = std::env::temp_dir().join(format!("garden-regions-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&graph_dir).unwrap();
        let stored = StoredRegion {
            wire_id: "wire-a".into(),
            artifact_id: "artifact-1".into(),
            region: json!({ "kind": "text-lines", "start": 1, "end": 2 }),
            file_hash: Some("f".into()),
            region_hash: Some("r".into()),
            region_text: Some("x".into()),
            captured_at: "now".into(),
        };
        write_stored_region(&graph_dir, &stored).unwrap();
        let mut other = stored.clone();
        other.wire_id = "wire-b".into();
        write_stored_region(&graph_dir, &other).unwrap();
        assert!(region_path(&graph_dir, "../escape").is_err());
        assert_eq!(read_stored_region(&graph_dir, "wire-a").unwrap().unwrap().artifact_id, "artifact-1");
        // wire-a lives and targets an artifact; wire-b is gone; wire-c's
        // file never existed. Only wire-b's file goes.
        let snapshot = json!({ "wires": [
            { "id": "wire-a", "targetArtifactId": "artifact-1" },
            { "id": "wire-c", "targetDocumentId": "doc" },
        ]});
        // Fresh files are spared (the snapshot may lag the wire); aged ones go.
        assert_eq!(sweep_orphans(&graph_dir, &snapshot), 0);
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(SWEEP_GRACE_SECS * 2);
        for name in ["wire-a.json", "wire-b.json"] {
            let file = std::fs::File::options().write(true).open(regions_dir(&graph_dir).join(name)).unwrap();
            file.set_modified(old).unwrap();
        }
        assert_eq!(sweep_orphans(&graph_dir, &snapshot), 1);
        assert!(!remove_stored_region(&graph_dir, "legacy:wire id").unwrap(), "an id with no possible file is not an error");
        assert!(read_stored_region(&graph_dir, "legacy:wire id").unwrap().is_none());
        let exported = export_entries(&graph_dir, &std::collections::HashSet::from(["artifact-1".to_string()]));
        assert_eq!(exported.len(), 1);
        assert!(!String::from_utf8_lossy(&exported[0].1).contains("\"regionText\""), "referenced text stays home");
        assert!(read_stored_region(&graph_dir, "wire-a").unwrap().is_some());
        assert!(read_stored_region(&graph_dir, "wire-b").unwrap().is_none());
        assert!(remove_stored_region(&graph_dir, "wire-a").unwrap());
        assert!(!remove_stored_region(&graph_dir, "wire-a").unwrap());
        std::fs::remove_dir_all(&graph_dir).ok();
    }

    fn bytes_for(path: &Path, filename: &str, mime: &str) -> ArtifactBytes {
        let file = std::fs::File::open(path).unwrap();
        let len = file.metadata().unwrap().len();
        ArtifactBytes {
            file,
            len,
            filename: filename.to_string(),
            mime_type: mime.to_string(),
            kind: kind_from_mime(mime, Some(&artifact_file_type(filename))),
            referenced: false,
            store_hash: Some("store".into()),
        }
    }

    #[test]
    fn line_and_byte_regions_read_bounded_and_refuse_out_of_range() {
        let dir = std::env::temp_dir().join(format!("garden-regions-read-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rooms.rs");
        std::fs::write(&path, "\u{FEFF}fn a() {}\r\nfn b() {}\r\n\r\nfn c() {}\n").unwrap();
        let mut bytes = bytes_for(&path, "rooms.rs", "application/octet-stream");
        let read = bytes.read_region(&Region::TextLines { start: 2, end: 4 }).unwrap();
        assert_eq!(read.full_text.as_deref(), Some("fn b() {}\n\nfn c() {}"));
        assert!(read.region_hash.is_some());
        let first = bytes.read_region(&Region::TextLines { start: 1, end: 1 }).unwrap();
        assert_eq!(first.full_text.as_deref(), Some("fn a() {}"), "BOM stripped, CR folded");
        let error = bytes.read_region(&Region::TextLines { start: 3, end: 9 }).unwrap_err();
        assert!(error.contains("has 4 lines"), "{error}");
        let range = bytes.read_region(&Region::Bytes { start: 3, end: 7 }).unwrap();
        assert_eq!(range.full_text.as_deref(), Some("fn a"));
        let error = bytes.read_region(&Region::Bytes { start: 0, end: 10_000 }).unwrap_err();
        assert!(error.contains("out of range"), "{error}");
        assert!(bytes.read_region(&Region::PdfPage { page: 1 }).unwrap_err().contains("needs a PDF"));
        assert!(bytes.read_region(&Region::Member { path: "a".into() }).unwrap_err().contains("needs a zip"));
        assert_eq!(bytes.current_file_hash().unwrap().as_deref(), Some("store"));

        let json_path = dir.join("export.json");
        std::fs::write(&json_path, r#"{"conversations":[{"title":"one"},{"title":"two"}]}"#).unwrap();
        let mut json_bytes = bytes_for(&json_path, "export.json", "application/json");
        let read = json_bytes.read_region(&Region::JsonPointer { pointer: "/conversations/1/title".into() }).unwrap();
        assert_eq!(read.full_text.as_deref(), Some("two"));
        assert!(json_bytes.read_region(&Region::JsonPointer { pointer: "/nope".into() }).unwrap_err().contains("resolves to nothing"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
