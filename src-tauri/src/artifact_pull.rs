//! Step 4 of "Artifacts — a body outside the house": pull a region of an
//! artifact into blocks. `insert_blocks` with an artifact as the source
//! reads the region through the same gate as step 3, splits it into
//! blocks, and the caller wires every block back to the artifact with a
//! `cites` record wire narrowed to the block's own lines, plus one
//! `authoredBy` on the document. The artifact's mode does not change: a
//! partial pull is a quotation with provenance.

use crate::app_runtime::AppHandle;
use crate::{
    artifact_regions::{open_artifact_bytes, ArtifactBytes, Region},
    crdt_engine::workspace_ops::artifact_file_type,
    json_utils::json_string,
    mcp_utils::mcp_arg_string,
};
use serde_json::{json, Value};
use std::io::{Read, Seek, SeekFrom};

/// A pull makes at most this many blocks and reads at most this much text.
pub(crate) const PULL_BLOCK_CAP: usize = 2_000;
pub(crate) const PULL_TEXT_CAP_BYTES: u64 = 1024 * 1024;

const CODE_EXTENSIONS: &[&str] = &[
    "rs", "py", "ts", "tsx", "js", "jsx", "mjs", "cjs", "go", "java", "kt", "swift", "c", "h", "cc", "cpp",
    "hpp", "cs", "rb", "php", "sh", "zsh", "bash", "fish", "sql", "toml", "yaml", "yml", "json", "css",
    "scss", "html", "xml", "lua", "zig", "hs", "ml", "ex", "exs", "erl", "clj", "scala", "r", "jl", "nim",
    "dart", "vue", "svelte", "tf", "proto", "graphql", "make", "cmake", "dockerfile", "ini", "cfg",
];

/// How the pulled text becomes blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PullAs {
    Paragraphs,
    Code,
    Quote,
}

impl PullAs {
    fn parse(value: Option<String>, filename: &str) -> Result<PullAs, String> {
        match value.as_deref().map(str::trim) {
            None | Some("") => Ok(if is_code_file(filename) { PullAs::Code } else { PullAs::Paragraphs }),
            Some("paragraphs") | Some("paragraph") | Some("text") => Ok(PullAs::Paragraphs),
            Some("code") => Ok(PullAs::Code),
            Some("quote") | Some("quotes") => Ok(PullAs::Quote),
            Some(other) => Err(format!("as must be paragraphs, code or quote, not {other}")),
        }
    }
}

fn is_code_file(filename: &str) -> bool {
    let extension = artifact_file_type(filename);
    CODE_EXTENSIONS.contains(&extension.as_str())
}

/// What a pull produced: the structured blocks for `block.insert` (ids
/// assigned here, as the engine requires) and, beside each, the region the
/// block's `cites` wire names.
pub(crate) struct PulledBlocks {
    pub(crate) artifact_id: String,
    pub(crate) region: Option<Region>,
    pub(crate) blocks: Vec<Value>,
    pub(crate) block_regions: Vec<Option<Region>>,
    pub(crate) as_kind: PullAs,
    pub(crate) filename: String,
}

/// Block ids: derived from the operation id and the block's index when
/// the caller gave one (so a retry of the same operation names the same
/// blocks and the engine's replay guard holds), random otherwise.
fn block_id_for(operation_id: Option<&str>, index: usize) -> String {
    match operation_id {
        Some(operation_id) => {
            use sha2::Digest;
            let digest = sha2::Sha256::digest(format!("{operation_id}\u{0}{index}").as_bytes());
            format!("block-{}", &format!("{digest:x}")[..12])
        }
        None => format!("block-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]),
    }
}

/// The artifact the call names, when it names one: `artifactId`, or
/// `artifactPath` resolved to its referenced record (made if absent, as
/// `create_wires` does). None when the call is an ordinary insert.
pub(crate) async fn artifact_for_pull(
    app: &AppHandle,
    graph_id: &str,
    arguments: &Value,
) -> Result<Option<String>, String> {
    if let Some(id) = mcp_arg_string(arguments, &["artifactId", "artifact_id"]).filter(|id| !id.trim().is_empty()) {
        return Ok(Some(id.trim().to_string()));
    }
    let path = mcp_arg_string(arguments, &["artifactPath", "artifact_path"])
        .map(|path| path.trim().to_string())
        .filter(|path| !path.is_empty());
    match path {
        Some(path) => crate::artifact_reference::find_or_reference_by_path(app.clone(), graph_id, &path)
            .await
            .map(Some),
        None => Ok(None),
    }
}

/// Read the region (or the whole text) and split it into blocks. Runs
/// blocking file IO; call from `spawn_blocking`.
pub(crate) fn pull_blocks(
    app: &AppHandle,
    graph_id: &str,
    artifact_id: &str,
    region: Option<Region>,
    as_value: Option<String>,
    operation_id: Option<&str>,
) -> Result<PulledBlocks, String> {
    let record = crate::artifact_storage::artifact_record(app, graph_id, artifact_id)?
        .ok_or_else(|| format!("artifact not found: {artifact_id}"))?;
    let mut bytes = open_artifact_bytes(app, graph_id, artifact_id, &record)?;
    let filename = bytes.filename.clone();
    let explicit_as = as_value.as_deref().map(str::trim).filter(|value| !value.is_empty()).is_some();
    let as_kind = PullAs::parse(as_value, &filename)?;
    // Caps are checked before the bytes are read where the region says its
    // own size (angel review): a byte region over the cap never allocates.
    if let Some(Region::Bytes { start, end }) = &region {
        if end.saturating_sub(*start) > PULL_TEXT_CAP_BYTES {
            return Err(format!("the region is {} bytes; a pull reads at most {PULL_TEXT_CAP_BYTES}", end - start));
        }
    }
    let mut blocks = Vec::new();
    let mut block_regions = Vec::new();
    let mut next_id = {
        let mut index = 0usize;
        move || {
            let id = block_id_for(operation_id, index);
            index += 1;
            id
        }
    };
    let mut whole_region = region.clone();
    if let Some(Region::JsonPointer { pointer }) = &region {
        // Resolved on the parsed document, not on the region's text, so a
        // string that happens to look like an array is a string.
        let document = json_document(&mut bytes)?;
        let pointed = document
            .pointer(pointer)
            .ok_or_else(|| format!("json pointer {pointer} resolves to nothing in {filename}"))?;
        match pointed {
            Value::Array(items) => {
                for (index, item) in items.iter().enumerate() {
                    // A string element is prose unless told otherwise; the
                    // rest is data and reads as code.
                    let (content, kind) = match item {
                        Value::String(text) => (text.clone(), if explicit_as { as_kind } else { PullAs::Paragraphs }),
                        other => (serde_json::to_string_pretty(other).unwrap_or_default(), if explicit_as { as_kind } else { PullAs::Code }),
                    };
                    blocks.push(block_value(kind, &content, &filename, next_id()));
                    block_regions.push(Some(Region::JsonPointer { pointer: format!("{pointer}/{index}") }));
                }
            }
            Value::String(text) => {
                let kind = if explicit_as { as_kind } else { PullAs::Paragraphs };
                blocks.push(block_value(kind, text, &filename, next_id()));
                block_regions.push(region.clone());
            }
            other => {
                let text = serde_json::to_string_pretty(other).unwrap_or_default();
                let kind = if explicit_as { as_kind } else { PullAs::Code };
                blocks.push(block_value(kind, &text, &filename, next_id()));
                block_regions.push(region.clone());
            }
        }
    } else {
        let (text, base_line) = match &region {
            Some(region) => {
                crate::artifact_regions::region_fits_record(region, &record)?;
                let read = bytes.read_region(region)?;
                let text = read
                    .full_text
                    .ok_or_else(|| format!("a {} region carries no text to pull", region_kind_name(region)))?;
                let base = match region {
                    Region::TextLines { start, .. } => Some(*start),
                    _ => None,
                };
                (text, base)
            }
            None => {
                let text = whole_text(&mut bytes)?;
                // A whole-file pull still names its lines, so step 5 can
                // answer a line question with it.
                let line_count = text.trim_end_matches('\n').split('\n').count() as u64;
                whole_region = Some(Region::TextLines { start: 1, end: line_count.max(1) });
                (text, Some(1))
            }
        };
        if text.len() as u64 > PULL_TEXT_CAP_BYTES {
            return Err(format!("the region is {} bytes of text; a pull reads at most {PULL_TEXT_CAP_BYTES}", text.len()));
        }
        match as_kind {
            PullAs::Code => {
                blocks.push(block_value(PullAs::Code, text.trim_end_matches('\n'), &filename, next_id()));
                block_regions.push(whole_region.clone());
            }
            PullAs::Paragraphs | PullAs::Quote => {
                for (first, last, group) in paragraph_groups(&text) {
                    blocks.push(block_value(as_kind, &group, &filename, next_id()));
                    block_regions.push(match (&whole_region, base_line) {
                        // Lines: each paragraph names its own lines.
                        (Some(Region::TextLines { .. }), Some(base)) => {
                            Some(Region::TextLines { start: base + first as u64, end: base + last as u64 })
                        }
                        _ => whole_region.clone(),
                    });
                }
            }
        }
    }
    if blocks.is_empty() {
        return Err("the region holds no text to pull".to_string());
    }
    if blocks.len() > PULL_BLOCK_CAP {
        return Err(format!("the region would make {} blocks; a pull makes at most {PULL_BLOCK_CAP}", blocks.len()));
    }
    Ok(PulledBlocks { artifact_id: artifact_id.to_string(), region: whole_region, blocks, block_regions, as_kind, filename })
}

/// The whole file parsed as JSON (the light index's parse cap applies).
fn json_document(bytes: &mut ArtifactBytes) -> Result<Value, String> {
    const JSON_CAP: u64 = 8 * 1024 * 1024;
    if bytes.len > JSON_CAP {
        return Err(format!("{} is {} bytes; a json document is pulled only under {JSON_CAP}", bytes.filename, bytes.len));
    }
    bytes.file.seek(SeekFrom::Start(0)).map_err(|error| format!("seek: {error}"))?;
    let mut raw = Vec::with_capacity(bytes.len as usize);
    bytes.file.read_to_end(&mut raw).map_err(|error| format!("read json: {error}"))?;
    serde_json::from_slice(&raw).map_err(|error| format!("{} is not valid JSON: {error}", bytes.filename))
}

fn region_kind_name(region: &Region) -> &'static str {
    match region {
        Region::TextLines { .. } => "text-lines",
        Region::PdfPage { .. } => "pdf-page",
        Region::PdfSelector { .. } => "pdf-selector",
        Region::Bytes { .. } => "bytes",
        Region::Member { .. } => "member",
        Region::JsonPointer { .. } => "json-pointer",
    }
}

/// The whole file as normalized text, when it is text and under the cap.
fn whole_text(bytes: &mut ArtifactBytes) -> Result<String, String> {
    if bytes.len > PULL_TEXT_CAP_BYTES {
        return Err(format!(
            "{} is {} bytes; a whole-file pull reads at most {PULL_TEXT_CAP_BYTES}, name a region",
            bytes.filename, bytes.len
        ));
    }
    bytes.file.seek(SeekFrom::Start(0)).map_err(|error| format!("seek: {error}"))?;
    let mut raw = Vec::with_capacity(bytes.len as usize);
    bytes.file.read_to_end(&mut raw).map_err(|error| format!("read artifact: {error}"))?;
    if !crate::artifact_reference::looks_like_text(&raw) {
        return Err(format!("{} does not read as text", bytes.filename));
    }
    let mut text = String::from_utf8_lossy(&raw).to_string();
    if let Some(stripped) = text.strip_prefix('\u{FEFF}') {
        text = stripped.to_string();
    }
    let text = text.replace("\r\n", "\n");
    // The same normalization as the region reader: a bare CR at the end of
    // a last line without a newline goes too.
    Ok(text.strip_suffix('\r').map(str::to_string).unwrap_or(text))
}

/// Paragraphs: runs of non-blank lines, with the 0-based first and last
/// line index of each run in the text. Lines inside a run are joined by a
/// space, as markdown prose soft-wraps (a paragraph block shows one text
/// node, where a newline would render as a space anyway); the cited region
/// still names the original lines.
fn paragraph_groups(text: &str) -> Vec<(usize, usize, String)> {
    let mut groups = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    let mut first = 0usize;
    let mut last_index = 0usize;
    for (index, line) in text.split('\n').enumerate() {
        last_index = index;
        if line.trim().is_empty() {
            if !current.is_empty() {
                groups.push((first, index - 1, current.join(" ")));
                current.clear();
            }
        } else {
            if current.is_empty() {
                first = index;
            }
            current.push(line.trim_end());
        }
    }
    if !current.is_empty() {
        groups.push((first, last_index, current.join(" ")));
    }
    groups
}

fn block_value(as_kind: PullAs, content: &str, filename: &str, id: String) -> Value {
    match as_kind {
        PullAs::Code => {
            let language = artifact_file_type(filename);
            let language = if language == "unknown" { Value::Null } else { json!(language) };
            json!({ "id": id, "type": "code", "content": content, "language": language })
        }
        PullAs::Quote => json!({ "id": id, "type": "quote", "content": content }),
        PullAs::Paragraphs => json!({ "id": id, "type": "paragraph", "content": content }),
    }
}

/// The blocks that already have an active `cites` wire to the artifact
/// (a retry under the same operation id names the same blocks; their
/// wires are not made twice).
pub(crate) fn blocks_already_cited(snapshot: Option<&Value>, document_id: &str, artifact_id: &str) -> std::collections::HashSet<String> {
    crate::workspace_entity_projection::workspace_wires(snapshot)
        .iter()
        .filter(|wire| crate::wire_state::wire_is_active(wire))
        .filter(|wire| {
            json_string(wire.get("sourceDocumentId")).as_deref() == Some(document_id)
                && json_string(wire.get("targetArtifactId")).as_deref() == Some(artifact_id)
                && json_string(wire.get("predicate"))
                    .map(|predicate| {
                        crate::wire_predicates::predicate_short_name(&crate::rdf_service::wire_predicate_uri(&predicate)) == "cites"
                    })
                    .unwrap_or(false)
        })
        .filter_map(|wire| json_string(wire.get("sourceBlockId")))
        .collect()
}

/// The wires a pull makes: `cites` from each block to its region, and one
/// `authoredBy` from the document when it has none to this artifact yet.
pub(crate) fn cites_payloads(document_id: &str, block_ids: &[String], pulled: &PulledBlocks) -> Vec<Value> {
    block_ids
        .iter()
        .zip(pulled.block_regions.iter())
        .map(|(block_id, region)| {
            let mut payload = json!({
                "sourceDocumentId": document_id,
                "sourceBlockId": block_id,
                "targetArtifactId": pulled.artifact_id,
                "predicate": "cites",
            });
            if let Some(region) = region {
                payload["targetRegion"] = region.to_json();
            }
            payload
        })
        .collect()
}

pub(crate) fn document_has_authored_by(snapshot: Option<&Value>, document_id: &str, artifact_id: &str) -> bool {
    crate::workspace_entity_projection::workspace_wires(snapshot)
        .iter()
        .filter(|wire| crate::wire_state::wire_is_active(wire))
        .any(|wire| {
            json_string(wire.get("sourceDocumentId")).as_deref() == Some(document_id)
                && json_string(wire.get("targetArtifactId")).as_deref() == Some(artifact_id)
                && json_string(wire.get("predicate"))
                    .map(|predicate| {
                        crate::wire_predicates::predicate_short_name(&crate::rdf_service::wire_predicate_uri(&predicate))
                            == "authoredBy"
                    })
                    .unwrap_or(false)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paragraphs_keep_their_line_offsets() {
        let text = "a\nb\n\n\nc\n";
        let groups = paragraph_groups(text);
        assert_eq!(groups.len(), 2);
        assert_eq!((groups[0].0, groups[0].1, groups[0].2.as_str()), (0, 1, "a b"));
        assert_eq!((groups[1].0, groups[1].1, groups[1].2.as_str()), (4, 4, "c"));
        let tail = paragraph_groups("x\ny");
        assert_eq!((tail[0].0, tail[0].1, tail[0].2.as_str()), (0, 1, "x y"));
        assert!(paragraph_groups("\n\n").is_empty());
        let one = paragraph_groups("only");
        assert_eq!((one[0].0, one[0].1), (0, 0));
    }

    #[test]
    fn as_defaults_by_file_type_and_blocks_carry_ids() {
        assert_eq!(PullAs::parse(None, "rooms.rs").unwrap(), PullAs::Code);
        assert_eq!(PullAs::parse(None, "notes.md").unwrap(), PullAs::Paragraphs);
        assert_eq!(PullAs::parse(Some("quote".into()), "rooms.rs").unwrap(), PullAs::Quote);
        assert!(PullAs::parse(Some("table".into()), "x").is_err());
        let block = block_value(PullAs::Code, "fn a() {}", "rooms.rs", block_id_for(None, 0));
        assert_eq!(block["type"], "code");
        assert_eq!(block["language"], "rs");
        assert!(block["id"].as_str().unwrap().starts_with("block-"));
        let block = block_value(PullAs::Paragraphs, "words", "notes.txt", block_id_for(None, 0));
        assert_eq!(block["type"], "paragraph");
        // Deterministic under an operation id, distinct per index, random otherwise.
        assert_eq!(block_id_for(Some("op-1"), 0), block_id_for(Some("op-1"), 0));
        assert_ne!(block_id_for(Some("op-1"), 0), block_id_for(Some("op-1"), 1));
        assert_ne!(block_id_for(Some("op-1"), 0), block_id_for(Some("op-2"), 0));
        assert_ne!(block_id_for(None, 0), block_id_for(None, 0));
        assert_eq!(block_id_for(Some("op-1"), 0).len(), "block-".len() + 12);
    }

    #[test]
    fn cites_payloads_name_each_block_and_its_region() {
        let pulled = PulledBlocks {
            artifact_id: "artifact-1".into(),
            region: Some(Region::TextLines { start: 10, end: 20 }),
            blocks: vec![json!({}), json!({})],
            block_regions: vec![Some(Region::TextLines { start: 10, end: 12 }), None],
            as_kind: PullAs::Paragraphs,
            filename: "x.md".into(),
        };
        let payloads = cites_payloads("doc", &["b1".into(), "b2".into()], &pulled);
        assert_eq!(payloads[0]["sourceBlockId"], "b1");
        assert_eq!(payloads[0]["targetRegion"]["end"], 12);
        assert_eq!(payloads[0]["predicate"], "cites");
        assert!(payloads[1].get("targetRegion").is_none());
    }
}
