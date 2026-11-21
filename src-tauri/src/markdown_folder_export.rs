//! Whole-graph export as a folder of markdown: leaving is as easy as
//! arriving (Ways In, 2026-10-17; release plan, Layer 2). One `.md` per
//! document, in its folder, with front matter that carries what the graph
//! knew and a wikilink for every wire, so Obsidian shows the same graph and
//! the vault door brings it back.
//!
//! The folder is delivered as a zip in the profile's `exports/` directory,
//! downloaded through the same short-lived token as the archive export.
//!
//! Layout:
//!   graph.json                      (format "mycl-markdown-folder", version 1)
//!   <Folder>/<Sub folder>/<Title>.md
//!   attachments/<artifact id>/<file> (attached artifacts' bytes)
//!
//! Front matter (YAML, JSON-quoted strings):
//!   id, title, created, updated, and `wires:` — one entry per wire at the
//!   document, with the predicate and the other end by title and id. The
//!   body is the document's markdown; a trailing "Wires" section repeats
//!   each outgoing wire as a wikilink, which is what the vault importer
//!   reads (an incoming one written here too would come back doubled).

#[path = "markdown_image_export.rs"]
mod images;

use crate::app_runtime::AppHandle;
use crate::{
    clock::timestamp,
    document_service::{read_workspace_record, DocumentRecord},
    graph_paths::{documents_dir, existing_graph_dir},
    graph_record_store::read_graph_record_no_heal,
    ids::safe_filename,
    json_utils::json_string,
    profile_paths::exports_dir,
    storage::{create_dir_all, display_path},
    wire_state::wire_is_active,
    workspace_entity_projection::{
        workspace_documents, workspace_entity_id, workspace_entity_title, workspace_folders,
        workspace_wires,
    },
};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::Write,
    path::Path,
};

pub(crate) const MARKDOWN_FOLDER_FORMAT: &str = "mycl-markdown-folder";

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MarkdownFolderExportResult {
    pub(crate) archive_file: String,
    pub(crate) archive_path: String,
    pub(crate) archive_size_bytes: u64,
    pub(crate) document_count: usize,
    pub(crate) folder_count: usize,
    pub(crate) wire_count: usize,
    pub(crate) attachment_count: usize,
    pub(crate) warnings: Vec<String>,
}

/// One wire as the front matter and the Wires section write it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WireLine {
    pub(crate) predicate: String,
    /// `to` when this document is the source, `from` when it is the target.
    pub(crate) outgoing: bool,
    pub(crate) other_id: String,
    pub(crate) other_title: String,
    /// Block-level endpoints, when the wire has them: this document's block
    /// and the other document's block.
    pub(crate) own_block: Option<String>,
    pub(crate) other_block: Option<String>,
}

/// A document ready to be written: where, and what.
#[derive(Debug, Clone)]
pub(crate) struct ExportedDocument {
    pub(crate) document_id: String,
    pub(crate) relative_path: String,
    pub(crate) text: String,
    /// Outgoing wires written as wikilinks.
    pub(crate) wires_written: usize,
}

pub(crate) fn build_markdown_folder_export(
    app: &AppHandle,
    graph_id: &str,
) -> Result<MarkdownFolderExportResult, String> {
    let graph_dir = existing_graph_dir(app, graph_id)?;
    let mut warnings = Vec::new();
    if let Err(error) = crate::app_runtime::async_runtime::block_on(
        crate::crdt_projection_flush::flush_graph_projection(app.clone(), graph_id),
    ) {
        warnings.push(format!(
            "best-effort CRDT flush failed before export: {error}"
        ));
    }
    let title = read_graph_record_no_heal(app, graph_id)
        .ok()
        .map(|(_, record)| record.title)
        .unwrap_or_else(|| graph_id.to_string());

    let workspace = read_workspace_record(&graph_dir, graph_id)?;
    let snapshot = workspace.snapshot.as_ref();
    let folders = workspace_folders(snapshot);
    let documents = workspace_documents(snapshot);
    let wires = workspace_wires(snapshot);
    let records = read_records_tolerant(&graph_dir, &mut warnings);
    // Geist views (the Song, the Diary, the memory archive) are projections
    // the engine rebuilds; they are not the person's documents and would
    // re-import as editable copies.
    let derived: BTreeSet<String> = documents
        .iter()
        .filter_map(workspace_entity_id)
        .filter(|id| crate::geist_derived_view::is_derived(&graph_dir, id).unwrap_or(false))
        .collect();

    let attachments = images::inventory(&graph_dir)?;
    let image_paths = images::paths(&attachments);
    let exported = plan_documents_with_images(
        &folders,
        &documents,
        &wires,
        &records,
        &derived,
        &image_paths,
        &mut warnings,
    );
    let wire_count: usize = exported.iter().map(|document| document.wires_written).sum();

    let exports = exports_dir(app)?;
    create_dir_all(&exports)?;
    crate::graph_archive_export::reap_stale_exports(&exports);
    let archive_file = format!(
        "{}-{}-markdown.zip",
        safe_filename(graph_id),
        timestamp().replace([':', '.'], "-")
    );
    let archive_path = exports.join(&archive_file);
    // Written as a part file and renamed at the end, so a failure leaves no
    // half-zip that looks like an export.
    let part_path = exports.join(format!("{archive_file}.part"));
    let file = File::create(&part_path)
        .map_err(|error| format!("create export zip {}: {error}", display_path(&part_path)))?;
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    // Attachments are mostly PDFs, images and zips, already compressed.
    let stored = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .large_file(true);

    let manifest = serde_json::json!({
        "format": MARKDOWN_FOLDER_FORMAT,
        "version": 1,
        "graphId": graph_id,
        "title": title,
        "exportedAt": timestamp(),
        "exporter": "gardend",
        "documents": exported.len(),
        "folders": folders.len(),
        "wires": wire_count,
    });
    zip.start_file("graph.json", options)
        .and_then(|_| {
            zip.write_all(
                serde_json::to_string_pretty(&manifest)
                    .unwrap_or_default()
                    .as_bytes(),
            )
            .map_err(Into::into)
        })
        .map_err(|error| format!("write graph.json: {error}"))?;
    for document in &exported {
        zip.start_file(&document.relative_path, options)
            .and_then(|_| zip.write_all(document.text.as_bytes()).map_err(Into::into))
            .map_err(|error| format!("write {}: {error}", document.relative_path))?;
    }

    let attachment_count = attachments.len();
    for attachment in &attachments {
        let mut source = File::open(&attachment.path)
            .map_err(|e| format!("open attachment {}: {e}", attachment.name))?;
        zip.start_file(
            format!("attachments/{}/{}", attachment.id, attachment.name),
            stored,
        )
        .map_err(|e| e.to_string())?;
        std::io::copy(&mut source, &mut zip).map_err(|e| e.to_string())?;
    }
    zip.finish()
        .map_err(|error| format!("finalize export zip: {error}"))?;
    std::fs::rename(&part_path, &archive_path)
        .map_err(|error| format!("finish export zip {}: {error}", display_path(&archive_path)))?;
    let archive_size_bytes = std::fs::metadata(&archive_path)
        .map_err(|error| format!("stat export zip: {error}"))?
        .len();

    Ok(MarkdownFolderExportResult {
        archive_file,
        archive_path: display_path(&archive_path),
        archive_size_bytes,
        document_count: exported.len(),
        folder_count: folders.len(),
        wire_count,
        attachment_count,
        warnings,
    })
}

/// Every document record on disk, by id. One unreadable record is a warning,
/// not the end of the export: leaving is the rescue path.
fn read_records_tolerant(
    graph_dir: &Path,
    warnings: &mut Vec<String>,
) -> BTreeMap<String, DocumentRecord> {
    let mut records = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(documents_dir(graph_dir)) else {
        warnings.push("the documents directory could not be listed".to_string());
        return records;
    };
    for entry in entries.flatten() {
        let manifest_path = entry.path().join("document.json");
        if !manifest_path.is_file() {
            continue;
        }
        let Some(document_id) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if crate::document_tombstone_store::document_is_tombstoned(graph_dir, &document_id)
            .unwrap_or(false)
        {
            continue;
        }
        match crate::document_record_store::read_document_record_cold(graph_dir, &manifest_path) {
            Ok(record) => {
                records.insert(record.document_id.clone(), record);
            }
            Err(error) => warnings.push(format!("{document_id}: not exported: {error}")),
        }
    }
    records
}

/// Decide every document's path and text. Pure: the zip is written after.
pub(crate) fn plan_documents(
    folders: &[Value],
    documents: &[Value],
    wires: &[Value],
    records: &BTreeMap<String, DocumentRecord>,
    derived: &BTreeSet<String>,
    warnings: &mut Vec<String>,
) -> Vec<ExportedDocument> {
    plan_documents_with_images(
        folders,
        documents,
        wires,
        records,
        derived,
        &BTreeMap::new(),
        warnings,
    )
}
fn plan_documents_with_images(
    folders: &[Value],
    documents: &[Value],
    wires: &[Value],
    records: &BTreeMap<String, DocumentRecord>,
    derived: &BTreeSet<String>,
    image_paths: &BTreeMap<String, String>,
    warnings: &mut Vec<String>,
) -> Vec<ExportedDocument> {
    let titles: BTreeMap<String, String> = documents
        .iter()
        .filter_map(|document| {
            let id = workspace_entity_id(document)?;
            let title = workspace_entity_title(document).unwrap_or_else(|| id.clone());
            Some((id, title))
        })
        .collect();
    let mut lines_by_document: BTreeMap<String, Vec<WireLine>> = BTreeMap::new();
    for wire in wires {
        if !wire_is_active(wire) {
            continue;
        }
        let (Some(source), Some(target), Some(predicate)) = (
            json_string(wire.get("sourceDocumentId")),
            json_string(wire.get("targetDocumentId")),
            json_string(wire.get("predicate")),
        ) else {
            continue;
        };
        let predicate = short_predicate(&predicate);
        let source_block = json_string(wire.get("sourceBlockId"));
        let target_block = json_string(wire.get("targetBlockId"));
        lines_by_document
            .entry(source.clone())
            .or_default()
            .push(WireLine {
                predicate: predicate.clone(),
                outgoing: true,
                other_title: titles
                    .get(&target)
                    .cloned()
                    .unwrap_or_else(|| target.clone()),
                other_id: target.clone(),
                own_block: source_block.clone(),
                other_block: target_block.clone(),
            });
        lines_by_document.entry(target).or_default().push(WireLine {
            predicate,
            outgoing: false,
            other_title: titles
                .get(&source)
                .cloned()
                .unwrap_or_else(|| source.clone()),
            other_id: source,
            own_block: target_block,
            other_block: source_block,
        });
    }

    // First pass: where every document lands, so a link can name the file
    // it points at (Obsidian resolves a wikilink against file names, and a
    // duplicate title gets its own " (2)").
    let mut used: BTreeSet<String> = BTreeSet::new();
    let mut placed: Vec<(String, String, String, String)> = Vec::new(); // id, title, path, stem
    for document in documents {
        let Some(document_id) = workspace_entity_id(document) else {
            continue;
        };
        if derived.contains(&document_id) {
            continue;
        }
        let Some(record) = records.get(&document_id) else {
            warnings.push(format!(
                "{document_id}: listed in the workspace, no record on disk"
            ));
            continue;
        };
        let title = titles
            .get(&document_id)
            .cloned()
            .unwrap_or_else(|| record.title.clone());
        let folder = folder_chain(
            folders,
            json_string(
                document
                    .get("parentId")
                    .or_else(|| document.get("parent_id")),
            ),
        );
        let base = file_stem(&title);
        let mut stem = base.clone();
        let mut relative_path = match &folder {
            Some(folder) => format!("{folder}/{stem}.md"),
            None => format!("{stem}.md"),
        };
        let mut n = 2;
        while !used.insert(relative_path.to_lowercase()) {
            stem = format!("{base} ({n})");
            relative_path = match &folder {
                Some(folder) => format!("{folder}/{stem}.md"),
                None => format!("{stem}.md"),
            };
            n += 1;
        }
        placed.push((document_id, title, relative_path, stem));
    }
    let stems: BTreeMap<String, String> = placed
        .iter()
        .map(|(id, _, _, stem)| (id.clone(), stem.clone()))
        .collect();

    let mut exported = Vec::new();
    for (document_id, title, relative_path, _) in placed {
        let Some(record) = records.get(&document_id) else {
            continue;
        };
        let lines = lines_by_document.remove(&document_id).unwrap_or_default();
        let wires_written = lines.iter().filter(|line| line.outgoing).count();
        let image_body = images::markdown(record, &relative_path, image_paths, warnings);
        let text = render_document(record, &title, &lines, &stems, image_body);
        exported.push(ExportedDocument {
            document_id,
            relative_path,
            text,
            wires_written,
        });
    }
    exported
}

/// The folder titles from the root down, each made a safe path segment,
/// joined with `/`. `None` at the root. A cycle or a missing parent ends
/// the chain where it is found.
fn folder_chain(folders: &[Value], parent_id: Option<String>) -> Option<String> {
    let mut segments = Vec::new();
    let mut current = parent_id;
    let mut seen = BTreeSet::new();
    while let Some(folder_id) = current {
        if !seen.insert(folder_id.clone()) {
            break;
        }
        let Some(folder) = folders
            .iter()
            .find(|folder| workspace_entity_id(folder).as_deref() == Some(folder_id.as_str()))
        else {
            break;
        };
        segments.push(file_stem(
            &workspace_entity_title(folder).unwrap_or(folder_id),
        ));
        current = json_string(folder.get("parentId").or_else(|| folder.get("parent_id")));
    }
    if segments.is_empty() {
        None
    } else {
        segments.reverse();
        Some(segments.join("/"))
    }
}

/// A title as a file stem: path separators and control characters out,
/// characters other filesystems refuse replaced, length bounded.
pub(crate) fn file_stem(title: &str) -> String {
    let cleaned: String = safe_filename(title)
        .chars()
        .map(|c| match c {
            ':' | '*' | '?' | '"' | '<' | '>' | '|' | '#' | '[' | ']' => ' ',
            other => other,
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    // Bound by bytes (filesystems count bytes, 255 per segment, and the
    // ".md" and " (N)" come on top), then trim, so a cut never leaves a
    // trailing space or dot behind.
    let mut bounded = String::new();
    for c in cleaned.chars() {
        if bounded.len() + c.len_utf8() > FILE_STEM_MAX_BYTES {
            break;
        }
        bounded.push(c);
    }
    let bounded = bounded.trim_matches(|c| c == '.' || c == ' ').to_string();
    if bounded.is_empty() {
        return "Untitled".to_string();
    }
    // Windows refuses these names with any extension.
    let upper = bounded.to_ascii_uppercase();
    let head = upper.split('.').next().unwrap_or("");
    let reserved = matches!(head, "CON" | "PRN" | "AUX" | "NUL")
        || (head.len() == 4
            && (head.starts_with("COM") || head.starts_with("LPT"))
            && head.as_bytes()[3].is_ascii_digit());
    if reserved {
        format!("_{bounded}")
    } else {
        bounded
    }
}

/// A file name segment is at most this many bytes before ".md" and " (N)".
pub(crate) const FILE_STEM_MAX_BYTES: usize = 180;

fn short_predicate(predicate: &str) -> String {
    predicate
        .rsplit(['#', '/'])
        .next()
        .unwrap_or(predicate)
        .to_string()
}

/// A YAML scalar the vault importer reads back whole. Its title reader
/// strips one pair of quotes and nothing else, so a title with a double
/// quote or a backslash is single-quoted (only `'` is doubled there) and
/// everything else is a JSON string, which YAML accepts.
fn yaml_string(value: &str) -> String {
    if value.contains('"') || value.contains('\\') {
        if value.contains(['\n', '\r']) {
            return serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string());
        }
        return format!("'{}'", value.replace('\'', "''"));
    }
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

/// A wikilink to another exported document: the target is the file stem
/// (what Obsidian resolves against), the display is the title when the
/// two differ, with the characters a link cannot hold folded away.
pub(crate) fn wikilink(stem: &str, title: &str) -> String {
    let fold = |text: &str| -> String {
        text.chars()
            .map(|c| match c {
                '[' | ']' | '|' | '#' | '^' => ' ',
                other => other,
            })
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    let target = fold(stem);
    let target = if target.is_empty() {
        "Untitled"
    } else {
        &target
    };
    let display = fold(title);
    if display.is_empty() || display == target {
        format!("[[{target}]]")
    } else {
        format!("[[{target}|{display}]]")
    }
}

/// A body that came in through the vault door may still carry the Wires
/// section a previous export wrote; it is not content, and it must not
/// stack up across round trips.
fn strip_trailing_wires_section(body: &str) -> &str {
    let Some(at) = body.rfind("\n## Wires") else {
        return body;
    };
    let after = &body[at + 1..];
    let mut lines = after.lines();
    lines.next();
    if lines
        .map(str::trim)
        .all(|line| line.is_empty() || line.starts_with("- "))
    {
        body[..at].trim_end()
    } else {
        body
    }
}

fn iso_or_raw(value: &str) -> String {
    match crate::clock::parse_timestamp(value.trim()) {
        Some(ms) => {
            let ms = if ms < 10_000_000_000 { ms * 1000 } else { ms };
            chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms as i64)
                .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
                .unwrap_or_else(|| value.to_string())
        }
        None => value.to_string(),
    }
}

pub(crate) fn render_document(
    record: &DocumentRecord,
    title: &str,
    wires: &[WireLine],
    stems: &BTreeMap<String, String>,
    body: Option<String>,
) -> String {
    let mut out = String::new();
    out.push_str("---\n");
    out.push_str(&format!("id: {}\n", yaml_string(&record.document_id)));
    out.push_str(&format!("title: {}\n", yaml_string(title)));
    out.push_str(&format!(
        "created: {}\n",
        yaml_string(&iso_or_raw(&record.created_at))
    ));
    out.push_str(&format!(
        "updated: {}\n",
        yaml_string(&iso_or_raw(&record.updated_at))
    ));
    if !wires.is_empty() {
        out.push_str("wires:\n");
        for wire in wires {
            let direction = if wire.outgoing { "to" } else { "from" };
            out.push_str(&format!(
                "  - predicate: {}\n    {direction}: {}\n    {direction}Id: {}\n",
                yaml_string(&wire.predicate),
                yaml_string(&wire.other_title),
                yaml_string(&wire.other_id)
            ));
            if let Some(block) = &wire.own_block {
                out.push_str(&format!("    block: {}\n", yaml_string(block)));
            }
            if let Some(block) = &wire.other_block {
                out.push_str(&format!("    {direction}Block: {}\n", yaml_string(block)));
            }
        }
    }
    out.push_str("---\n\n");
    let body = body.unwrap_or_else(|| crate::document_export_rendering::document_markdown(record));
    let body = strip_trailing_wires_section(body.trim_end());
    if body.is_empty() {
        out.push_str(&format!("# {title}\n"));
    } else {
        out.push_str(body);
        out.push('\n');
    }
    // Only this document's own wires become wikilinks: the vault door makes
    // one wire per link it finds, so an incoming wire written here too would
    // come back doubled. Incoming wires stay in the front matter.
    let outgoing: Vec<&WireLine> = wires.iter().filter(|wire| wire.outgoing).collect();
    if !outgoing.is_empty() {
        out.push_str("\n## Wires\n\n");
        for wire in outgoing {
            let stem = stems
                .get(&wire.other_id)
                .map(String::as_str)
                .unwrap_or(&wire.other_title);
            out.push_str(&format!(
                "- {} {}\n",
                wire.predicate,
                wikilink(stem, &wire.other_title)
            ));
        }
    }
    out
}

/// Resolves a requested zip filename inside the exports dir, refusing
/// anything that is not a plain existing `-markdown.zip` basename.
pub(crate) fn resolve_markdown_export(
    app: &AppHandle,
    archive_file: &str,
) -> Option<std::path::PathBuf> {
    if archive_file.is_empty()
        || archive_file.contains('/')
        || archive_file.contains('\\')
        || archive_file.contains("..")
        || !archive_file.ends_with("-markdown.zip")
    {
        return None;
    }
    let exports = exports_dir(app).ok()?;
    let path: &Path = &exports.join(archive_file);
    path.is_file().then(|| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record(id: &str, title: &str, created: &str) -> DocumentRecord {
        serde_json::from_value(json!({
            "documentId": id, "graphId": "g", "title": title, "body": "",
            "origin": "local", "providerId": "local-profile", "localPath": "",
            "rdfSubject": "", "createdAt": created, "updatedAt": created,
            "capabilities": [],
            "tiptapJson": { "type": "doc", "content": [
                { "type": "paragraph", "content": [{ "type": "text", "text": format!("Body of {title}.") }] }
            ] },
        }))
        .expect("record")
    }

    #[test]
    fn stems_are_safe_and_bounded() {
        assert_eq!(file_stem("A/B: the \"quote\" [x]"), "A_B the quote x");
        assert_eq!(file_stem("..."), "Untitled");
        assert_eq!(file_stem(&"x".repeat(300)).len(), FILE_STEM_MAX_BYTES);
        // Bytes, not characters, and no trailing space or dot after the cut.
        let cjk = "漢".repeat(100);
        assert!(file_stem(&cjk).len() <= FILE_STEM_MAX_BYTES);
        let cut = format!("{} y", "x".repeat(FILE_STEM_MAX_BYTES - 1));
        assert!(!file_stem(&cut).ends_with(' '));
        assert_eq!(file_stem("CON"), "_CON");
        assert_eq!(file_stem("com1.txt"), "_com1.txt");
        assert_eq!(file_stem("Console"), "Console");
        assert_eq!(
            wikilink("Notes 1", "Notes [1] | #tag"),
            "[[Notes 1|Notes 1 tag]]"
        );
        assert_eq!(wikilink("Plain", "Plain"), "[[Plain]]");
        assert_eq!(yaml_string("He said \"hi\""), "'He said \"hi\"'");
        assert_eq!(yaml_string("it's"), "\"it's\"");
        assert_eq!(
            strip_trailing_wires_section("Body.\n\n## Wires\n\n- supports X\n- cites Y"),
            "Body."
        );
        assert_eq!(
            strip_trailing_wires_section("Body.\n\n## Wires\n\nA paragraph here."),
            "Body.\n\n## Wires\n\nA paragraph here."
        );
    }

    #[test]
    fn documents_land_in_their_folders_with_front_matter_and_wires() {
        let folders = vec![
            json!({ "id": "f-a", "title": "Reading", "parentId": null }),
            json!({ "id": "f-b", "title": "2026: Notes/Drafts", "parentId": "f-a" }),
        ];
        let documents = vec![
            json!({ "id": "d1", "title": "On Wires", "parentId": "f-b" }),
            json!({ "id": "d2", "title": "On Wires", "parentId": "f-b" }),
            json!({ "id": "d3", "title": "Root Doc", "parentId": null }),
            json!({ "id": "d4", "title": "Derived", "parentId": null }),
        ];
        let wires = vec![
            json!({ "id": "w1", "sourceDocumentId": "d1", "targetDocumentId": "d3",
                     "sourceBlockId": "b-1", "targetBlockId": "b-3",
                     "predicate": "http://mnemosyne.ai/vocab#supports" }),
            json!({ "id": "w2", "sourceDocumentId": "d3", "targetDocumentId": "d2",
                     "predicate": "contradicts", "deletedAt": "1" }),
        ];
        let records: BTreeMap<String, DocumentRecord> = [
            ("d1", "On Wires", "1700000000000"),
            ("d2", "On Wires", "1700000000000"),
            ("d3", "Root Doc", "2026-10-23T12:00:00.000Z"),
            ("d4", "Derived", "1700000000000"),
        ]
        .into_iter()
        .map(|(id, title, created)| (id.to_string(), record(id, title, created)))
        .collect();
        let mut warnings = Vec::new();
        let derived: BTreeSet<String> = ["d4".to_string()].into_iter().collect();
        let exported = plan_documents(
            &folders,
            &documents,
            &wires,
            &records,
            &derived,
            &mut warnings,
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        let paths: Vec<&str> = exported.iter().map(|d| d.relative_path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                "Reading/2026 Notes_Drafts/On Wires.md",
                "Reading/2026 Notes_Drafts/On Wires (2).md",
                "Root Doc.md",
            ]
        );
        let d1 = &exported[0].text;
        assert!(
            d1.starts_with(
                "---\nid: \"d1\"\ntitle: \"On Wires\"\ncreated: \"2023-11-14T22:13:20.000Z\""
            ),
            "{d1}"
        );
        assert!(
            d1.contains(
                "wires:\n  - predicate: \"supports\"\n    to: \"Root Doc\"\n    toId: \"d3\"\n    block: \"b-1\"\n    toBlock: \"b-3\"\n"
            ),
            "{d1}"
        );
        assert!(d1.contains("\nBody of On Wires.\n"), "{d1}");
        assert!(
            d1.ends_with("## Wires\n\n- supports [[Root Doc]]\n"),
            "{d1}"
        );
        assert_eq!(exported[0].wires_written, 1);
        assert_eq!(exported[1].wires_written, 0);
        let d3 = &exported[2].text;
        assert!(
            d3.contains("    from: \"On Wires\"\n    fromId: \"d1\"\n    block: \"b-3\"\n    fromBlock: \"b-1\"\n"),
            "{d3}"
        );
        assert!(
            !d3.contains("## Wires"),
            "incoming wires make no wikilink: {d3}"
        );
        // The deleted wire is nobody's.
        assert!(!d3.contains("contradicts"));
        assert!(!exported[1].text.contains("contradicts"));
    }

    /// The whole thing against the real engine: a graph with a folder, two
    /// documents, a wire and an attached artifact comes out as a zip whose
    /// members are where the plan put them.
    #[test]
    fn a_graph_exports_to_a_zip_of_markdown_in_folders() {
        use crate::crdt_queue::{enqueue_crdt_operation, EnqueueCrdtOperationInput};
        use crate::graph_service::{create_graph_service, CreateGraphInput};
        use std::io::Read;

        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let profile = std::env::temp_dir().join(format!("garden-md-export-{nanos}"));
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);
        let graph_id = "md-src";
        create_graph_service(
            &app,
            CreateGraphInput {
                graph_id: Some(graph_id.to_string()),
                title: "Markdown Folder".to_string(),
                description: None,
                operation_id: None,
            },
        )
        .expect("create graph");
        let enqueue = |kind: &str, document_id: Option<&str>, payload: serde_json::Value| {
            crate::app_runtime::async_runtime::block_on(enqueue_crdt_operation(
                app.clone(),
                EnqueueCrdtOperationInput {
                    kind: kind.to_string(),
                    graph_id: graph_id.to_string(),
                    document_id: document_id.map(str::to_string),
                    payload,
                },
            ))
            .expect(kind)
        };
        enqueue(
            "workspace.createFolder",
            None,
            json!({ "folderId": "f-read", "name": "Reading / Notes" }),
        );
        enqueue(
            "document.write",
            Some("d-a"),
            json!({ "documentId": "d-a", "title": "On Wires", "format": "markdown",
                    "content": "# On Wires\n\nA wire is a judgment.", "parentId": "f-read" }),
        );
        enqueue(
            "document.write",
            Some("d-b"),
            json!({ "documentId": "d-b", "title": "Root Doc", "format": "markdown",
                    "content": "# Root Doc\n\nThe root." }),
        );
        enqueue(
            "workspace.createWire",
            Some("d-a"),
            json!({ "wireId": "w-1", "sourceDocumentId": "d-a", "targetDocumentId": "d-b",
                    "predicate": "supports" }),
        );
        let graph_dir = existing_graph_dir(&app, graph_id).expect("graph dir");
        let original_dir =
            crate::document_paths::artifact_original_dir(&graph_dir, "artifact-1").unwrap();
        crate::original_file_storage::save_original_bytes_to_dir(
            &original_dir,
            "export.zip",
            "application/zip",
            b"not really a zip",
        )
        .expect("seed attachment");

        let built = build_markdown_folder_export(&app, graph_id).expect("export");
        assert_eq!(built.document_count, 2);
        assert_eq!(built.attachment_count, 1);
        assert_eq!(built.wire_count, 1);
        assert!(built.archive_file.ends_with("-markdown.zip"));
        let file = std::fs::File::open(&built.archive_path).expect("open zip");
        let mut archive = zip::ZipArchive::new(file).expect("read zip");
        let mut names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "Reading _ Notes/On Wires.md",
                "Root Doc.md",
                "attachments/artifact-1/export.zip",
                "graph.json",
            ]
        );
        let mut text = String::new();
        archive
            .by_name("Reading _ Notes/On Wires.md")
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert!(text.contains("title: \"On Wires\""), "{text}");
        assert!(text.contains("A wire is a judgment."), "{text}");
        assert!(text.contains("- supports [[Root Doc]]"), "{text}");
        assert!(
            resolve_markdown_export(&app, &built.archive_file).is_some(),
            "the download resolves the zip"
        );
        assert!(resolve_markdown_export(&app, "../x-markdown.zip").is_none());
        let _ = std::fs::remove_dir_all(&profile);
    }
}
