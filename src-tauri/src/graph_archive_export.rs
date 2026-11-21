//! graph.exportArchive — whole-graph tar.gz export, the mirror image of
//! `crdt_engine::import_archive_ops`. Closes the export asymmetry flagged in
//! the gap catalog (gardend could EAT an archive but not PRODUCE one), which
//! made profile-directory copies the only backup story for the offline room.
//!
//! Format: exactly what `parse_graph_archive` validates —
//!   manifest.json                 (version 1, format "mnemosyne-graph-export")
//!   rdf/graph.nq                  (n-quads dataset dump)
//!   crdt/workspace.yjs            (workspace Y.Doc update-v1 bytes)
//!   crdt/documents/{doc_id}.yjs   (per-document Y.Doc update-v1 bytes)
//! plus, when artifacts are included (Eschaton's call: default ON — the CRDT
//! is definitive and the bundle should be complete):
//!   artifacts/{id}/original/*     (artifact originals + store manifests)
//!   images/{id}/original/*
//!   documents/{id}/original/*     (source-file originals backing sf_storageKey)
//! Old importers that predate artifact entries simply ignore the extra paths
//! (the tar reader only collects entries it recognizes), so the format change
//! is additive.
//!
//! The archive lands in the profile-level `exports/` directory (deliberately
//! OUTSIDE any graph dir so the durable-flush walker and a later import can
//! never see a half-written bundle as graph state). Downloads are authorized
//! by a short-lived token minted next to the archive — the browser download
//! is a plain <a> click that cannot carry the loopback bearer header, same
//! pattern as image access tokens.

use crate::app_runtime::AppHandle;
use crate::{
    clock::timestamp,
    graph_paths::existing_graph_dir,
    graph_record_store::read_graph_record_no_heal,
    profile_paths::exports_dir,
    storage::{create_dir_all, display_path, read_bytes, read_json, write_json},
    ydoc_paths::workspace_ydoc_state_path,
};
use flate2::{write::GzEncoder, Compression};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::Write,
    path::{Path, PathBuf},
};

const EXPORT_TOKEN_TTL_SECONDS: u64 = 60 * 60;
/// Exports are staging for download, not an archive of archives: full
/// plaintext graph backups must not accumulate on disk forever (angel
/// finding). Anything older than this is reaped on the next export build.
const EXPORT_RETENTION_SECONDS: u64 = 7 * 24 * 60 * 60;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GraphArchiveExportResult {
    pub(crate) archive_file: String,
    pub(crate) archive_path: String,
    pub(crate) archive_size_bytes: u64,
    pub(crate) document_count: usize,
    pub(crate) chronicle_count: usize,
    pub(crate) artifact_file_count: usize,
    pub(crate) geist_file_count: usize,
    pub(crate) includes_artifacts: bool,
    pub(crate) warnings: Vec<String>,
}

pub(crate) fn build_graph_archive(
    app: &AppHandle,
    graph_id: &str,
    include_artifacts: bool,
) -> Result<GraphArchiveExportResult, String> {
    let graph_dir = existing_graph_dir(app, graph_id)?;
    let mut warnings = Vec::new();

    // Best-effort CRDT projection flush so the exported Y.Doc files carry
    // the latest hot-room state; mirrors the semantic refresh boundary.
    if let Err(error) = crate::app_runtime::async_runtime::block_on(
        crate::crdt_projection_flush::flush_graph_projection(app.clone(), graph_id),
    ) {
        warnings.push(format!(
            "best-effort CRDT flush failed before export: {error}"
        ));
    }

    let record = read_graph_record_no_heal(app, graph_id).ok();
    let (title, description) = record
        .map(|(_, record)| (record.title, record.description))
        .unwrap_or_else(|| (graph_id.to_string(), None));

    let rdf = crate::rdf_service::dump_rdf(
        app.clone(),
        crate::rdf_service::RdfDumpInput {
            graph_id: graph_id.to_string(),
            format: "application/n-quads".to_string(),
            source_graph_iri: None,
        },
    )
    .map_err(|error| format!("dump graph RDF for export: {error}"))?;

    let manifest = serde_json::json!({
        "version": 1,
        "format": "mnemosyne-graph-export",
        "source_user_id": "default",
        "source_graph_id": graph_id,
        "source_graph_title": title,
        "source_graph_description": description,
        "includes_artifacts": include_artifacts,
        "exported_at": timestamp(),
        "exporter": "gardend",
        "files": {
            "rdf": "rdf/graph.nq",
            "workspace": "crdt/workspace.yjs",
            "documents_dir": "crdt/documents/",
            "chronicle_dir": "chronicle/documents/"
        }
    });

    let exports = exports_dir(app)?;
    create_dir_all(&exports)?;
    reap_stale_exports(&exports);
    let archive_file = format!(
        "{}-{}.tar.gz",
        crate::ids::safe_filename(graph_id),
        timestamp().replace([':', '.'], "-")
    );
    let archive_path = exports.join(&archive_file);
    let file = File::create(&archive_path).map_err(|error| {
        format!(
            "create export archive {}: {error}",
            display_path(&archive_path)
        )
    })?;
    let encoder = GzEncoder::new(file, Compression::default());
    let mut tar = tar::Builder::new(encoder);

    append_bytes(
        &mut tar,
        "manifest.json",
        serde_json::to_string_pretty(&manifest)
            .map_err(|error| format!("serialize export manifest: {error}"))?
            .as_bytes(),
    )?;
    append_bytes(&mut tar, "rdf/graph.nq", rdf.data.as_bytes())?;

    let workspace_state = workspace_ydoc_state_path(&graph_dir);
    if workspace_state.is_file() {
        let bytes = read_bytes(&workspace_state).map_err(|error| error.to_string())?;
        append_bytes(&mut tar, "crdt/workspace.yjs", &bytes)?;
    } else {
        warnings.push("graph has no workspace Y.Doc state file".to_string());
    }

    let mut document_count = 0usize;
    let mut exported_document_ids: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    let documents_root = graph_dir.join("ydocs").join("documents");
    if documents_root.is_dir() {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(&documents_root)
            .map_err(|error| format!("list document Y.Docs: {error}"))?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.is_dir())
            .collect();
        entries.sort();
        for document_dir in entries {
            let Some(document_id) = document_dir.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let state_path = document_dir.join("update-v1.bin");
            if !state_path.is_file() {
                continue;
            }
            let bytes = read_bytes(&state_path).map_err(|error| error.to_string())?;
            append_bytes(
                &mut tar,
                &format!("crdt/documents/{document_id}.yjs"),
                &bytes,
            )?;
            document_count += 1;
            exported_document_ids.insert(document_id.to_string());
        }
    }

    // Chronicle (additive, 2026-09): every exported document's authority
    // clock and authorship travel as `chronicle/documents/{id}.json`, so an
    // import restores the original createdAt / createdBy instead of stamping
    // the import instant — the 2026-09-05 import gave all 1,075 documents the
    // same birthday. Records are read cold (no Y.Doc hydration).
    let mut chronicle_count = 0usize;
    if !exported_document_ids.is_empty() {
        match crate::document_record_store::read_graph_documents_cold(&graph_dir) {
            Ok(records) => {
                for record in records {
                    if !exported_document_ids.contains(&record.document_id) {
                        continue;
                    }
                    let entry = crate::document_chronicle::ChronicleEntry::from_record(&record);
                    let bytes = serde_json::to_vec_pretty(&entry)
                        .map_err(|error| format!("serialize chronicle entry: {error}"))?;
                    append_bytes(
                        &mut tar,
                        &format!("chronicle/documents/{}.json", record.document_id),
                        &bytes,
                    )?;
                    chronicle_count += 1;
                }
            }
            Err(error) => warnings.push(format!("chronicle entries skipped: {error}")),
        }
    }

    let mut artifact_file_count = 0usize;
    if include_artifacts {
        for root in ["artifacts", "images", "documents"] {
            artifact_file_count +=
                append_original_trees(&mut tar, &graph_dir, root, &mut warnings)?;
        }
        // The regions side store (what each artifact wire's region said)
        // travels with the artifacts, except the text of regions on
        // referenced files: that is text from outside the house and stays
        // out of the archive (angel review). The import drops the files of
        // wires it drops.
        let referenced: std::collections::HashSet<String> =
            crate::document_service::read_workspace_record(&graph_dir, graph_id)
                .ok()
                .and_then(|workspace| workspace.snapshot)
                .as_ref()
                .and_then(|snapshot| snapshot.get("artifacts"))
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .filter(|record| crate::artifact_reference::record_is_referenced(record))
                .filter_map(|record| crate::json_utils::json_string(record.get("id")))
                .collect();
        for (path, bytes) in crate::artifact_regions::export_entries(&graph_dir, &referenced) {
            append_bytes(&mut tar, &path, &bytes)?;
            artifact_file_count += 1;
        }
    }

    // Geist authorities travel unconditionally: a backup that loses the
    // memory queue, the Songs, and the valuations is not a backup of a
    // Garden. (The RDF dump only carries their PROJECTIONS, which the
    // importer deliberately filters — without these files an imported graph
    // would answer SPARQL about memories while recall()/music() read empty.)
    let mut geist_file_count = 0usize;
    for geist_root in ["memory", "narrative", "values"] {
        geist_file_count += append_tree(&mut tar, &graph_dir, geist_root, &mut warnings)?;
    }

    append_bytes(&mut tar, "README.md", ARCHIVE_README.as_bytes())?;

    let encoder = tar
        .into_inner()
        .map_err(|error| format!("finalize export tar stream: {error}"))?;
    encoder
        .finish()
        .map_err(|error| format!("finalize export gzip stream: {error}"))?;
    let archive_size_bytes = std::fs::metadata(&archive_path)
        .map_err(|error| format!("stat export archive: {error}"))?
        .len();

    Ok(GraphArchiveExportResult {
        archive_file,
        archive_path: display_path(&archive_path),
        archive_size_bytes,
        document_count,
        chronicle_count,
        artifact_file_count,
        geist_file_count,
        includes_artifacts: include_artifacts,
        warnings,
    })
}

/// Text of the README.md placed at the archive root so a human opening the
/// bundle years later can navigate it without Garden running.
const ARCHIVE_README: &str = r#"# Mnemosyne Graph Export

This archive is a complete portable copy of one Garden graph
(format: mnemosyne-graph-export, version 1). It re-imports through
Garden's graph import (settings page or POST /graphs/import), and every
piece is also readable on its own:

- manifest.json — export identity: source graph id/title, what is included,
  where each section lives.
- crdt/workspace.yjs — the workspace structure (folders, document catalog,
  wires, artifact registry) as a Y.js update. This is the authoritative
  CRDT state.
- crdt/documents/{id}.yjs — every document's full content as a Y.js update
  (authoritative). Convert to text by importing, or with any Y.js tooling.
- chronicle/documents/{id}.json — each document's chronicle: createdAt /
  updatedAt (epoch ms), revision, createdBy / updatedBy, recent authors, and
  the forced-write trail. Import restores these instead of stamping the
  import instant.
- rdf/graph.nq — the knowledge graph as N-Quads: documents, blocks, wires,
  and metadata as queryable triples. Human-skimmable in any text editor;
  loadable into any RDF store.
- artifacts/{id}/original/* — uploaded artifact files (PDFs etc.), verbatim.
- artifact-regions/{wireId}.json — what an artifact wire's region said when it was made.
- images/{id}/original/* — uploaded images, verbatim.
- documents/{id}/original/* — source files behind imported documents.
- memory/ — agent memory queue and its archives (Geist authority, JSON).
- narrative/{observer}/ — each agent's Song (Geist authority, JSON).
- values/ — block valuations per observer (Geist authority, JSON).

Notes:
- Derived state (RDF projection graphs, semantic search index) is NOT
  included: Garden regenerates it locally after import.
- Older Garden importers simply ignore entries they don't recognize.
"#;

/// Recursively appends every file under `{graph_dir}/{root}` at its
/// graph-dir-relative path. Bounded depth, and symlinks are SKIPPED
/// explicitly via symlink_metadata (angel finding: is_file()/is_dir()
/// FOLLOW symlinks, so without this check a link planted inside a Geist
/// dir could leak files from outside the graph directory into the
/// archive).
fn append_tree<W: Write>(
    tar: &mut tar::Builder<W>,
    graph_dir: &Path,
    root: &str,
    warnings: &mut Vec<String>,
) -> Result<usize, String> {
    fn walk<W: Write>(
        tar: &mut tar::Builder<W>,
        dir: &Path,
        relative: &str,
        depth: usize,
        appended: &mut usize,
        warnings: &mut Vec<String>,
    ) -> Result<(), String> {
        if depth > 6 {
            warnings.push(format!("skipped overly deep export subtree: {relative}"));
            return Ok(());
        }
        let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
            .map_err(|error| format!("list {relative}: {error}"))?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .collect();
        entries.sort();
        for path in entries {
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let child_relative = format!("{relative}/{name}");
            let is_symlink = std::fs::symlink_metadata(&path)
                .map(|meta| meta.file_type().is_symlink())
                .unwrap_or(true);
            if is_symlink {
                warnings.push(format!(
                    "skipped symlink in export subtree: {child_relative}"
                ));
                continue;
            }
            if path.is_dir() {
                walk(tar, &path, &child_relative, depth + 1, appended, warnings)?;
            } else if path.is_file() {
                match read_bytes(&path) {
                    Ok(bytes) => {
                        append_bytes(tar, &child_relative, &bytes)?;
                        *appended += 1;
                    }
                    Err(error) => warnings.push(format!(
                        "skipped unreadable export file {}: {error}",
                        display_path(&path)
                    )),
                }
            }
        }
        Ok(())
    }

    let root_dir = graph_dir.join(root);
    if !root_dir.is_dir() {
        return Ok(0);
    }
    let mut appended = 0usize;
    walk(tar, &root_dir, root, 1, &mut appended, warnings)?;
    Ok(appended)
}

/// Walks `{graph_dir}/{root}/{entity_id}/original/**` and appends every file
/// under its graph-dir-relative path, which is exactly where the import side
/// re-extracts it. Only `original/` subtrees are included: Y.Doc state,
/// history, and index files under the same entity dirs travel through their
/// own channels (crdt/, rdf/) or deliberately not at all (history).
fn append_original_trees<W: Write>(
    tar: &mut tar::Builder<W>,
    graph_dir: &Path,
    root: &str,
    warnings: &mut Vec<String>,
) -> Result<usize, String> {
    let root_dir = graph_dir.join(root);
    if !root_dir.is_dir() {
        return Ok(0);
    }
    let mut appended = 0usize;
    let mut entity_dirs: Vec<PathBuf> = std::fs::read_dir(&root_dir)
        .map_err(|error| format!("list {root} directory: {error}"))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.is_dir())
        .collect();
    entity_dirs.sort();
    for entity_dir in entity_dirs {
        let original_dir = entity_dir.join("original");
        if !original_dir.is_dir() {
            continue;
        }
        let Some(entity_id) = entity_dir.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let mut files: Vec<PathBuf> = std::fs::read_dir(&original_dir)
            .map_err(|error| format!("list {root}/{entity_id}/original: {error}"))?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.is_file())
            .collect();
        files.sort();
        for file_path in files {
            let Some(filename) = file_path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            match read_bytes(&file_path) {
                Ok(bytes) => {
                    append_bytes(
                        tar,
                        &format!("{root}/{entity_id}/original/{filename}"),
                        &bytes,
                    )?;
                    appended += 1;
                }
                Err(error) => warnings.push(format!(
                    "skipped unreadable original {}: {error}",
                    display_path(&file_path)
                )),
            }
        }
    }
    Ok(appended)
}

fn append_bytes<W: Write>(
    tar: &mut tar::Builder<W>,
    path: &str,
    bytes: &[u8],
) -> Result<(), String> {
    let mut header = tar::Header::new_ustar();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    tar.append_data(&mut header, path, bytes)
        .map_err(|error| format!("append {path} to export archive: {error}"))
}

/// Best-effort removal of export archives (and their token files) older than
/// the retention window, plus orphaned token files whose archive is gone.
/// Never fails the build — reaping is hygiene, not correctness.
pub(crate) fn reap_stale_exports(exports: &Path) {
    let Ok(entries) = std::fs::read_dir(exports) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let is_archive =
            name.ends_with(".tar.gz") || name.ends_with(".zip") || name.ends_with(".zip.part");
        let is_token = name.ends_with(".token.json");
        if !is_archive && !is_token {
            continue;
        }
        if is_token {
            let archive_name = name.trim_end_matches(".token.json");
            if !exports.join(archive_name).is_file() {
                let _ = std::fs::remove_file(&path);
                continue;
            }
        }
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .map(|age| age.as_secs() > EXPORT_RETENTION_SECONDS)
            .unwrap_or(false);
        if stale {
            let _ = std::fs::remove_file(&path);
            if is_archive {
                let _ = std::fs::remove_file(exports.join(format!("{name}.token.json")));
            }
        }
    }
}

// ── Download access tokens ──────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
struct ExportAccessToken {
    token: String,
    created_at_ms: u128,
    expires_at_ms: u128,
}

fn export_token_path(exports: &Path, archive_file: &str) -> PathBuf {
    exports.join(format!("{archive_file}.token.json"))
}

/// Mints a short-lived download token stored next to the archive. The
/// download GET authenticates with this token instead of the loopback
/// bearer, because a browser `<a download>` click cannot send headers.
pub(crate) fn write_export_access_token(
    app: &AppHandle,
    archive_file: &str,
) -> Result<String, String> {
    let exports = exports_dir(app)?;
    let now = crate::clock::epoch_millis();
    let token = uuid::Uuid::new_v4().simple().to_string();
    write_json(
        &export_token_path(&exports, archive_file),
        &ExportAccessToken {
            token: token.clone(),
            created_at_ms: now,
            expires_at_ms: now + u128::from(EXPORT_TOKEN_TTL_SECONDS) * 1000,
        },
    )
    .map_err(|error| format!("write export access token: {error}"))?;
    Ok(token)
}

pub(crate) fn export_access_token_matches(
    app: &AppHandle,
    archive_file: &str,
    presented: &str,
) -> bool {
    let Ok(exports) = exports_dir(app) else {
        return false;
    };
    let Ok(stored) = read_json::<ExportAccessToken>(&export_token_path(&exports, archive_file))
    else {
        return false;
    };
    if presented.is_empty() {
        return false;
    }
    // Compare SHA-256 digests instead of raw bytes: string == short-circuits
    // on the first mismatched byte, which leaks prefix length to a timing
    // observer. Digest comparison makes the work independent of where the
    // mismatch occurs.
    use sha2::{Digest, Sha256};
    let stored_digest = Sha256::digest(stored.token.as_bytes());
    let presented_digest = Sha256::digest(presented.as_bytes());
    if stored_digest != presented_digest {
        return false;
    }
    crate::clock::epoch_millis() <= stored.expires_at_ms
}

/// Resolves a requested archive filename inside the exports dir, refusing
/// anything that is not a plain existing `.tar.gz` basename.
pub(crate) fn resolve_export_archive(app: &AppHandle, archive_file: &str) -> Option<PathBuf> {
    if archive_file.is_empty()
        || archive_file.contains('/')
        || archive_file.contains('\\')
        || archive_file.contains("..")
        || !archive_file.ends_with(".tar.gz")
    {
        return None;
    }
    let exports = exports_dir(app).ok()?;
    let path = exports.join(archive_file);
    path.is_file().then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crdt_engine::import_archive_ops::parse_graph_archive;
    use crate::crdt_operation_types::EnqueueCrdtOperationInput;
    use crate::crdt_queue::enqueue_crdt_operation;
    use crate::graph_paths::existing_graph_dir;
    use crate::graph_service::{create_graph_service, CreateGraphInput};
    use serde_json::json;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_profile(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("garden-archive-export-{name}-{nanos}"))
    }

    fn seed_graph(app: &AppHandle, graph_id: &str) {
        create_graph_service(
            app,
            CreateGraphInput {
                graph_id: Some(graph_id.to_string()),
                title: "Archive Round Trip".to_string(),
                description: Some("export fidelity check".to_string()),
                operation_id: None,
            },
        )
        .expect("create graph");
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
                    "title": "Round Trip Doc",
                }),
            },
        ))
        .expect("document.write drains through the real CRDT engine");
    }

    #[test]
    fn export_import_round_trip_preserves_docs_and_artifact_originals() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let profile = temp_profile("round-trip");
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);

        seed_graph(&app, "rt-src");
        write_doc(
            &app,
            "rt-src",
            "rt-doc",
            "# Round Trip\n\nThe basalt aquifer holds the winter snowmelt.",
        );
        let src_graph_dir = existing_graph_dir(&app, "rt-src").expect("source graph dir");
        let original_dir = crate::document_paths::artifact_original_dir(&src_graph_dir, "art-1")
            .expect("artifact original dir");
        crate::original_file_storage::save_original_bytes_to_dir(
            &original_dir,
            "notes.txt",
            "text/plain",
            b"hello from the source graph",
        )
        .expect("seed artifact original");

        // Geist authorities: seed a memory queue, a Song, and valuations so
        // the round trip proves the whole Geist travels (shadow AND body).
        create_dir_all(&src_graph_dir.join("memory")).expect("memory dir");
        std::fs::write(
            src_graph_dir.join("memory").join("memory-queue.json"),
            br#"{"memories":[{"number":1,"text":"the aquifer remembers"}]}"#,
        )
        .expect("seed memory queue");
        create_dir_all(&src_graph_dir.join("narrative").join("beta")).expect("narrative dir");
        std::fs::write(
            src_graph_dir
                .join("narrative")
                .join("beta")
                .join("song.json"),
            br#"{"verses":["a verse that must survive export"]}"#,
        )
        .expect("seed song");
        create_dir_all(&src_graph_dir.join("values").join("beta")).expect("values dir");
        std::fs::write(
            src_graph_dir
                .join("values")
                .join("beta")
                .join("block-values.json"),
            br#"{"block-1":{"importance":5}}"#,
        )
        .expect("seed values");

        // Chronicle: the source record's clock + authorship must survive the
        // round trip byte-for-byte (the import instant is not a birthday).
        let src_records = crate::document_record_store::read_graph_documents_cold(&src_graph_dir)
            .expect("read source documents");
        let src_doc = src_records
            .iter()
            .find(|doc| doc.document_id == "rt-doc")
            .expect("source document record")
            .clone();
        assert_eq!(
            src_doc
                .chronicle
                .created_by
                .as_ref()
                .map(|actor| actor.label())
                .as_deref(),
            Some("human:owner"),
            "a write without a request principal is the owner's"
        );

        let built = build_graph_archive(&app, "rt-src", true).expect("build archive");
        assert!(built.geist_file_count >= 3, "geist authorities exported");
        assert!(built.document_count >= 1, "doc ydoc exported");
        assert_eq!(
            built.chronicle_count, built.document_count,
            "one chronicle entry per doc"
        );
        assert!(
            built.artifact_file_count >= 1,
            "artifact original exported (got {})",
            built.artifact_file_count
        );
        assert!(built.includes_artifacts);

        let bytes = std::fs::read(&built.archive_path).expect("read archive");
        let parsed = parse_graph_archive(&bytes).expect("parse own archive");
        assert_eq!(parsed.manifest.source_graph_id, "rt-src");
        assert_eq!(parsed.manifest.source_user_id, "default");
        assert!(parsed.manifest.includes_artifacts);
        assert!(parsed.workspace_bytes.is_some(), "workspace ydoc present");
        assert!(
            parsed.documents.iter().any(|(id, _)| id == "rt-doc"),
            "document ydoc present"
        );
        assert!(
            parsed
                .originals
                .iter()
                .any(|(path, _)| path == "artifacts/art-1/original/notes.txt"),
            "artifact original entry present: {:?}",
            parsed.originals.iter().map(|(p, _)| p).collect::<Vec<_>>()
        );
        assert!(!parsed.rdf_n_quads.trim().is_empty(), "RDF dump present");
        let chronicle_entry = parsed
            .chronicle
            .iter()
            .find(|(id, _)| id == "rt-doc")
            .map(|(_, entry)| entry.clone())
            .expect("chronicle entry present");
        assert_eq!(chronicle_entry.created_at, src_doc.created_at);
        assert_eq!(chronicle_entry.revision, src_doc.revision);
        assert!(
            parsed
                .originals
                .iter()
                .any(|(path, _)| path == "memory/memory-queue.json"),
            "memory queue entry present"
        );
        assert!(
            parsed
                .originals
                .iter()
                .any(|(path, _)| path == "narrative/beta/song.json"),
            "song entry present"
        );

        // Full import through the real executor, same op the route enqueues.
        // The op requires its archive staged in the profile's pending-uploads
        // dir (the route stages uploads there); mirror that.
        let pending_dir = profile.join("pending-uploads");
        create_dir_all(&pending_dir).expect("create pending-uploads dir");
        let staged_path = pending_dir.join(&built.archive_file);
        std::fs::copy(&built.archive_path, &staged_path).expect("stage archive for import");
        crate::app_runtime::async_runtime::block_on(enqueue_crdt_operation(
            app.clone(),
            EnqueueCrdtOperationInput {
                kind: "graph.importArchive".to_string(),
                graph_id: "rt-dst".to_string(),
                document_id: None,
                payload: json!({
                    "filename": built.archive_file,
                    "newGraphId": "rt-dst",
                    "newTitle": "Round Trip Restored",
                    "pendingArchivePath": display_path(&staged_path),
                    "sizeBytes": built.archive_size_bytes,
                }),
            },
        ))
        .expect("archive import drains through the real CRDT engine");

        let dst_graph_dir = existing_graph_dir(&app, "rt-dst").expect("imported graph dir");
        let restored_doc_state =
            crate::ydoc_paths::document_ydoc_state_path(&dst_graph_dir, "rt-doc");
        assert!(
            restored_doc_state.is_file(),
            "imported document ydoc persisted"
        );
        let restored_original = dst_graph_dir
            .join("artifacts")
            .join("art-1")
            .join("original")
            .join("notes.txt");
        let restored_bytes =
            std::fs::read(&restored_original).expect("imported artifact original exists");
        assert_eq!(restored_bytes, b"hello from the source graph");
        let documents = crate::document_record_store::read_graph_documents_cold(&dst_graph_dir)
            .expect("read imported documents");
        let doc = documents
            .iter()
            .find(|doc| doc.document_id == "rt-doc")
            .expect("imported document readable");
        assert!(
            doc.body.contains("basalt aquifer"),
            "imported document carries content"
        );
        assert_eq!(
            doc.created_at, src_doc.created_at,
            "createdAt restored, not re-stamped"
        );
        assert_eq!(doc.updated_at, src_doc.updated_at, "updatedAt restored");
        assert!(
            doc.revision >= src_doc.revision,
            "revision never goes backwards"
        );
        assert_eq!(doc.chronicle.created_by, src_doc.chronicle.created_by);
        assert_eq!(doc.chronicle.updated_by, src_doc.chronicle.updated_by);
        assert_eq!(
            doc.chronicle.created_at_source.as_deref(),
            Some("archive-chronicle"),
            "the restored clock names its evidence"
        );

        // RDF identity must be rewritten to the target graph (angel finding:
        // the local-scheme URN previously survived import untouched, leaving
        // restored triples invisible to graph-scoped queries).
        let restored_rdf = crate::rdf_service::dump_rdf(
            app.clone(),
            crate::rdf_service::RdfDumpInput {
                graph_id: "rt-dst".to_string(),
                format: "application/n-quads".to_string(),
                source_graph_iri: None,
            },
        )
        .expect("dump imported graph RDF");
        assert!(
            !restored_rdf
                .data
                .contains("urn:mnemosyne:local:graph:rt-src"),
            "imported RDF must not retain the source graph URN"
        );
        assert!(
            restored_rdf
                .data
                .contains("urn:mnemosyne:local:graph:rt-dst"),
            "imported RDF must carry the target graph URN"
        );

        let restored_song = std::fs::read(
            dst_graph_dir
                .join("narrative")
                .join("beta")
                .join("song.json"),
        )
        .expect("imported song exists");
        assert_eq!(
            restored_song,
            br#"{"verses":["a verse that must survive export"]}"#.to_vec(),
            "Song authority round-trips byte-identical"
        );
        let restored_memory = std::fs::read(dst_graph_dir.join("memory").join("memory-queue.json"))
            .expect("imported memory queue exists");
        assert!(String::from_utf8_lossy(&restored_memory).contains("the aquifer remembers"));
        assert!(
            dst_graph_dir
                .join("values")
                .join("beta")
                .join("block-values.json")
                .is_file(),
            "valuations authority round-trips"
        );

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
    }

    #[test]
    fn export_download_tokens_gate_archive_access() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let profile = temp_profile("tokens");
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);

        let exports = exports_dir(&app).expect("exports dir");
        create_dir_all(&exports).expect("create exports dir");
        std::fs::write(exports.join("g-1.tar.gz"), b"gz").expect("stage archive");

        let token = write_export_access_token(&app, "g-1.tar.gz").expect("mint token");
        assert!(export_access_token_matches(&app, "g-1.tar.gz", &token));
        assert!(!export_access_token_matches(&app, "g-1.tar.gz", "wrong"));
        assert!(!export_access_token_matches(&app, "other.tar.gz", &token));

        assert!(resolve_export_archive(&app, "g-1.tar.gz").is_some());
        assert!(resolve_export_archive(&app, "../g-1.tar.gz").is_none());
        assert!(resolve_export_archive(&app, "g-1.tar").is_none());
        assert!(resolve_export_archive(&app, "").is_none());

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
    }
}
