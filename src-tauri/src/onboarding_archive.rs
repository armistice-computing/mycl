use crate::app_error::{AppError, AppResult};
use flate2::read::GzDecoder;
use serde::Deserialize;
use std::io::{Cursor, Read};
use tar::Archive;

pub(crate) const ARCHIVE_NAME: &str = "onboarding-v4.tar.gz";
pub(crate) const EXPECTED_FORMAT: &str = "mnemosyne-graph-export";
/// The shared (commons) value store inside an archive, as the graph exporter
/// writes it. Per-agent stores (`values/{observer}/…`) are never seeded.
pub(crate) const VALUES_ENTRY: &str = "values/block-values.json";

const ARCHIVE_BYTES: &[u8] = include_bytes!("../resources/onboarding/onboarding-v4.tar.gz");

#[derive(Debug, Deserialize)]
pub(crate) struct OnboardingManifest {
    pub version: u32,
    pub format: String,
    pub source_graph_id: String,
    #[serde(default)]
    pub source_graph_title: Option<String>,
    #[serde(default)]
    pub source_graph_description: Option<String>,
    #[serde(default)]
    pub includes_artifacts: bool,
}

pub(crate) struct OnboardingArchive {
    pub manifest: OnboardingManifest,
    pub workspace_bytes: Vec<u8>,
    pub documents: Vec<(String, Vec<u8>)>,
    /// The graph's block ratings, when the archive carries them.
    pub values: Option<Vec<u8>>,
}

pub(crate) fn extract() -> AppResult<OnboardingArchive> {
    extract_from(ARCHIVE_BYTES)
}

pub(crate) fn extract_from(bytes: &[u8]) -> AppResult<OnboardingArchive> {
    let cursor = Cursor::new(bytes);
    let decoder = GzDecoder::new(cursor);
    let mut archive = Archive::new(decoder);

    let mut manifest: Option<OnboardingManifest> = None;
    let mut workspace_bytes: Option<Vec<u8>> = None;
    let mut documents: Vec<(String, Vec<u8>)> = Vec::new();
    let mut values: Option<Vec<u8>> = None;

    let entries = archive
        .entries()
        .map_err(|error| AppError::storage(format!("read onboarding tar entries: {error}")))?;
    for entry_result in entries {
        let mut entry = entry_result
            .map_err(|error| AppError::storage(format!("read onboarding tar entry: {error}")))?;
        let path = entry
            .path()
            .map_err(|error| AppError::storage(format!("read tar entry path: {error}")))?
            .into_owned();
        let path_str = path.to_string_lossy().to_string();
        let normalized = path_str.trim_start_matches("./");

        let mut buf = Vec::new();
        entry.read_to_end(&mut buf).map_err(|error| {
            AppError::storage(format!("read tar entry bytes ({normalized}): {error}"))
        })?;

        if normalized == "manifest.json" {
            let parsed: OnboardingManifest = serde_json::from_slice(&buf).map_err(|error| {
                AppError::serialization(format!("parse onboarding manifest.json: {error}"))
            })?;
            manifest = Some(parsed);
        } else if normalized == "crdt/workspace.yjs" {
            workspace_bytes = Some(buf);
        } else if normalized == VALUES_ENTRY {
            values = Some(buf);
        } else if let Some(rest) = normalized.strip_prefix("crdt/documents/") {
            if let Some(doc_id) = rest.strip_suffix(".yjs") {
                if !doc_id.is_empty() {
                    documents.push((doc_id.to_string(), buf));
                }
            }
        }
        // Other archive members (rdf/graph.nq, narrative/, memory/,
        // per-agent values) are intentionally ignored: projection RDF is
        // regenerated from the Y.Docs, and the Song seeds itself the first
        // time an agent uses a Geist tool in the graph.
    }

    let manifest = manifest
        .ok_or_else(|| AppError::serialization("onboarding archive missing manifest.json"))?;
    let workspace_bytes = workspace_bytes
        .ok_or_else(|| AppError::storage("onboarding archive missing crdt/workspace.yjs"))?;

    if manifest.format != EXPECTED_FORMAT {
        return Err(AppError::validation(format!(
            "unexpected onboarding archive format: {} (expected {EXPECTED_FORMAT})",
            manifest.format,
        )));
    }

    documents.sort_by(|left, right| left.0.cmp(&right.0));

    Ok(OnboardingArchive {
        manifest,
        workspace_bytes,
        documents,
        values,
    })
}

/// Build the bundled onboarding archive from a graph directory (read only):
/// its ordinary documents, the workspace without Geist-derived views, the
/// ratings on the kept blocks, and a manifest carrying the graph's title and
/// description. Run by hand when the onboarding graph changes:
///   GARDEN_ONBOARDING_SOURCE_DIR=".../graphs/onboarding-v3" \
///   GARDEN_ONBOARDING_PREVIOUS=".../the archive this one replaces.tar.gz" \
///   cargo test --release --no-default-features --features headless --lib -- --ignored build_onboarding_archive --nocapture
/// with GARDEN_ONBOARDING_REWRITTEN="start-here,connecting-your-tools" naming
/// the pages rewritten since the previous archive.
///
/// `previous` is the archive this one replaces, with the ids of the pages
/// rewritten since. A rating of such a page that is in the old archive and
/// unchanged here was given to the page as it then was. A rewritten page
/// keeps its block ids by position, so that rating now sits on different
/// words, and it is left out. Pages not named keep all their ratings.
/// Returns the archive and the ratings left out.
#[cfg(test)]
pub(crate) fn build_archive(
    graph_dir: &std::path::Path,
    version: u32,
    previous: Option<(&[u8], &[String])>,
) -> Result<(Vec<u8>, Vec<String>), String> {
    use std::collections::BTreeSet;
    use yrs::{updates::decoder::Decode, Map, ReadTxn, StateVector, Transact, Update};

    let graph: serde_json::Value = serde_json::from_slice(
        &std::fs::read(graph_dir.join("graph.json")).map_err(|e| format!("read graph.json: {e}"))?,
    )
    .map_err(|e| format!("parse graph.json: {e}"))?;

    // Geist-derived views are made by the graph's agents; a new graph grows
    // its own.
    let mut derived: BTreeSet<String> = BTreeSet::new();
    if let Ok(entries) = std::fs::read_dir(graph_dir.join("narrative/derived-views")) {
        for entry in entries.flatten() {
            if let Some(id) = entry.file_name().to_str().and_then(|n| n.strip_suffix(".json")) {
                derived.insert(id.to_string());
            }
        }
    }

    let workspace_bytes = std::fs::read(graph_dir.join("ydocs/workspace/update-v1.bin"))
        .map_err(|e| format!("read the workspace Y.Doc: {e}"))?;
    let doc = yrs::Doc::new();
    {
        let update = Update::decode_v1(&workspace_bytes).map_err(|e| format!("decode workspace: {e}"))?;
        doc.transact_mut().apply_update(update).map_err(|e| format!("apply workspace: {e}"))?;
    }
    let documents_map = doc.get_or_insert_map("documents");
    let listed: Vec<String> = documents_map.keys(&doc.transact()).map(str::to_string).collect();
    {
        let mut txn = doc.transact_mut();
        for id in listed.iter().filter(|id| derived.contains(*id)) {
            documents_map.remove(&mut txn, id);
        }
    }
    // The archive carries no files (`includes_artifacts` is false), so it
    // carries no records of them either: a record without its file would
    // show a new person an upload that never finishes.
    let mut files_left_out = 0;
    for name in ["artifacts", "uploadBatches"] {
        let map = doc.get_or_insert_map(name);
        let ids: Vec<String> = map.keys(&doc.transact()).map(str::to_string).collect();
        let mut txn = doc.transact_mut();
        for id in &ids {
            map.remove(&mut txn, id);
        }
        files_left_out += ids.len();
    }
    if files_left_out > 0 {
        eprintln!("left out {files_left_out} file records (the archive carries no files)");
    }
    let kept: BTreeSet<String> = listed.into_iter().filter(|id| !derived.contains(id)).collect();
    let workspace = doc.transact().encode_state_as_update_v1(&StateVector::default());

    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    for id in &kept {
        let path = graph_dir.join("ydocs/documents").join(id).join("update-v1.bin");
        let bytes = std::fs::read(&path).map_err(|e| format!("read document {id}: {e}"))?;
        files.push((format!("crdt/documents/{id}.yjs"), bytes));
    }

    let mut stale: Vec<String> = Vec::new();
    let values_path = graph_dir.join(VALUES_ENTRY);
    if values_path.is_file() {
        let mut store: serde_json::Value = serde_json::from_slice(
            &std::fs::read(&values_path).map_err(|e| format!("read values: {e}"))?,
        )
        .map_err(|e| format!("parse values: {e}"))?;
        if let Some(blocks) = store.get_mut("blocks").and_then(serde_json::Value::as_object_mut) {
            blocks.retain(|_, record| {
                record.get("documentId").and_then(serde_json::Value::as_str).is_some_and(|d| kept.contains(d))
            });
        }
        if let Some((previous, rewritten)) = previous {
            let before = extract_from(previous).map_err(|e| format!("read the previous archive: {e:?}"))?;
            let before: serde_json::Value = match before.values {
                Some(bytes) => serde_json::from_slice(&bytes).map_err(|e| format!("parse previous values: {e}"))?,
                None => serde_json::json!({}),
            };
            let empty = serde_json::Map::new();
            let before = before.get("blocks").and_then(serde_json::Value::as_object).unwrap_or(&empty);
            // The agents' share of a record. The person's own rating is not
            // part of it and is never left out.
            const AGENTS: [&str; 5] = [
                "rawImportanceSum", "rawValenceSum", "importanceCount", "valenceCount", "valuationReceipts",
            ];
            let mut hand_rated: Vec<String> = Vec::new();
            if let Some(blocks) = store.get_mut("blocks").and_then(serde_json::Value::as_object_mut) {
                for (key, record) in blocks.iter() {
                    let Some(old) = before.get(key) else { continue };
                    let page = record.get("documentId").and_then(serde_json::Value::as_str).unwrap_or("");
                    if !rewritten.iter().any(|id| id == page) {
                        continue;
                    }
                    if AGENTS.iter().all(|field| old.get(field) == record.get(field)) {
                        let by_hand = ["userImportance", "userValence"]
                            .iter()
                            .any(|field| record.get(*field).is_some_and(|value| !value.is_null()));
                        if by_hand {
                            hand_rated.push(key.clone());
                        } else {
                            stale.push(key.clone());
                        }
                        continue;
                    }
                    // Rated by agents then and again since: the old share
                    // would have to be taken out of the sums. Nothing here
                    // does that, so say so instead of shipping a mixed number.
                    let old_receipts = old.get("valuationReceipts").and_then(serde_json::Value::as_object);
                    let carried = old_receipts.is_some_and(|receipts| {
                        receipts.keys().any(|id| record.get("valuationReceipts").and_then(|r| r.get(id)).is_some())
                    });
                    if carried {
                        return Err(format!("{key} was rated by agents before and after the previous archive; its sums mix both"));
                    }
                }
                for key in &stale {
                    blocks.remove(key);
                }
                // The person rated this block as it now reads: keep that,
                // and take out the agents' ratings of the old words. What
                // is left has the shape of a block only the person rated.
                for key in &hand_rated {
                    if let Some(record) = blocks.get_mut(key).and_then(serde_json::Value::as_object_mut) {
                        for field in ["rawImportanceSum", "rawValenceSum", "cumulativeImportance", "cumulativeValence"] {
                            record.insert(field.to_string(), serde_json::json!(0.0));
                        }
                        for field in ["importanceCount", "valenceCount", "valuationCount"] {
                            record.insert(field.to_string(), serde_json::json!(0));
                        }
                        record.remove("valuationReceipts");
                    }
                }
            }
            if !hand_rated.is_empty() {
                eprintln!("kept the person's rating and took out the agents' old ones on: {hand_rated:?}");
            }
        }
        if let Some(map) = store.as_object_mut() {
            map.remove("configHistory");
        }
        files.push((VALUES_ENTRY.to_string(), serde_json::to_vec_pretty(&store).map_err(|e| e.to_string())?));
    }

    let manifest = serde_json::json!({
        "version": version,
        "format": EXPECTED_FORMAT,
        "source_graph_id": graph_dir.file_name().and_then(|n| n.to_str()).unwrap_or(""),
        "source_graph_title": graph.get("title"),
        "source_graph_description": graph.get("description"),
        "includes_artifacts": false,
        "document_count": kept.len(),
        "files": { "workspace": "crdt/workspace.yjs", "documents_dir": "crdt/documents/", "values": VALUES_ENTRY },
    });

    let mut out = Vec::new();
    {
        let encoder = flate2::write::GzEncoder::new(&mut out, flate2::Compression::best());
        let mut tar = tar::Builder::new(encoder);
        let mut append = |path: &str, bytes: &[u8]| -> Result<(), String> {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_mtime(0);
            header.set_cksum();
            tar.append_data(&mut header, path, bytes).map_err(|e| format!("append {path}: {e}"))
        };
        append("manifest.json", &serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?)?;
        append("crdt/workspace.yjs", &workspace)?;
        for (path, bytes) in &files {
            append(path, bytes)?;
        }
        tar.into_inner().map_err(|e| e.to_string())?.finish().map_err(|e| e.to_string())?;
    }
    Ok((out, stale))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_archive_extracts_with_documents_workspace_and_values() {
        let archive = extract().expect("extract bundled onboarding archive");
        assert_eq!(archive.manifest.format, EXPECTED_FORMAT);
        assert!(!archive.workspace_bytes.is_empty(), "workspace.yjs empty");
        let ids: Vec<&str> = archive.documents.iter().map(|(id, _)| id.as_str()).collect();
        assert!(ids.contains(&"start-here"), "start-here not present in archive: {ids:?}");
        assert!(ids.iter().all(|id| !id.starts_with("geist-")), "a derived view was bundled: {ids:?}");
        assert!(archive.values.is_some(), "the ratings are bundled");
        assert_eq!(archive.manifest.source_graph_title.as_deref(), Some("Start"));
        assert_eq!(archive.manifest.version, 4);
        // The seeder rewrites the wires' graph ids from this (d0536cf).
assert_eq!(archive.manifest.source_graph_id, "onboarding-v3");
        // No record of a file the archive does not carry.
        let workspace = yrs::Doc::new();
        {
            use yrs::{updates::decoder::Decode, Transact};
            let update = yrs::Update::decode_v1(&archive.workspace_bytes).expect("decode workspace");
            workspace.transact_mut().apply_update(update).expect("apply workspace");
        }
        {
            use yrs::{Map, Transact};
            for name in ["artifacts", "uploadBatches"] {
                let map = workspace.get_or_insert_map(name);
                assert_eq!(map.len(&workspace.transact()), 0, "{name} records were bundled");
            }
        }
    }

    /// Writes resources/onboarding/<ARCHIVE_NAME> from a graph directory.
    #[test]
    #[ignore]
    fn build_onboarding_archive() {
        let Ok(dir) = std::env::var("GARDEN_ONBOARDING_SOURCE_DIR") else { return };
        let previous = std::env::var("GARDEN_ONBOARDING_PREVIOUS")
            .ok()
            .map(|path| std::fs::read(&path).expect("read GARDEN_ONBOARDING_PREVIOUS"));
        let rewritten: Vec<String> = std::env::var("GARDEN_ONBOARDING_REWRITTEN")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .collect();
        let (bytes, stale) = build_archive(
            std::path::Path::new(&dir),
            4,
            previous.as_deref().map(|bytes| (bytes, rewritten.as_slice())),
        )
        .expect("build");
        eprintln!("left out {} ratings already in the previous archive: {stale:?}", stale.len());
        let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("resources/onboarding").join(ARCHIVE_NAME);
        std::fs::write(&out, &bytes).expect("write archive");
        let archive = extract_from(&bytes).expect("the archive reads back");
        eprintln!(
            "wrote {} ({} bytes): {} documents {:?}, values {}",
            out.display(),
            bytes.len(),
            archive.documents.len(),
            archive.documents.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
            archive.values.as_ref().map_or(0, Vec::len)
        );
    }
}
