//! Query-only deserialization of the existing document manifest. The full file
//! is still loaded into a string; unused fields are scanned but not constructed
//! as objects (especially tree, TipTap, XML and CRDT payload).
//! No extra persistent projection or cache, and no sidecar hydration/backfill.

use crate::{
    document_record_store::read_graph_documents_using,
    document_types::{BlockSnapshot, DocumentRecord},
    ids::validate_local_id,
    runtime_config::DOCUMENT_SCHEMA_VERSION,
    storage::read_json,
};
use serde::Deserialize;
use std::path::Path;

// Do not flatten a catch-all/Chronicle into this type: serde would buffer the
// ignored projections. Search ranking needs only these fields, including the
// original created_at for salience and body/title for legacy block fallback.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchDocument {
    document_id: String,
    graph_id: String,
    title: String,
    body: String,
    created_at: String,
    updated_at: String,
    #[serde(default)]
    blocks: Vec<BlockSnapshot>,
    #[serde(default)]
    rdf_triple_count: usize,
}

fn read_search_document(graph_dir: &Path, path: &Path) -> Result<DocumentRecord, String> {
    let record: SearchDocument = read_json(path)?;
    validate_local_id(&record.graph_id, "graph_id")?;
    validate_local_id(&record.document_id, "document_id")?;
    crate::document_tombstone_store::require_document_not_tombstoned(
        graph_dir,
        &record.document_id,
    )?;
    // Adapt to existing lexical/salience routines only. These partial records
    // must never escape into persistence, export or general document reads.
    Ok(DocumentRecord {
        document_id: record.document_id,
        graph_id: record.graph_id,
        title: record.title,
        body: record.body,
        created_at: record.created_at,
        updated_at: record.updated_at,
        blocks: record.blocks,
        rdf_triple_count: record.rdf_triple_count,
        revision: 0,
        origin: String::new(),
        provider_id: String::new(),
        local_path: String::new(),
        rdf_subject: String::new(),
        capabilities: Vec::new(),
        schema_version: DOCUMENT_SCHEMA_VERSION,
        tiptap_xml: String::new(),
        tiptap_json: None,
        ydoc_update_base64: String::new(),
        ydoc_state_path: String::new(),
        tree: None,
        chronicle: Default::default(),
    })
}

/// Only for lexical/title search and its salience scoring. Uses the same
/// directory/tombstone filtering and stable recency ordering as the cold walk.
pub(super) fn read_search_only_documents(graph_dir: &Path) -> Result<Vec<DocumentRecord>, String> {
    read_graph_documents_using(graph_dir, read_search_document)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    /// Opt-in, file-read-only comparison against an existing graph or fixture.
    /// No RDF, rooms, cache, sidecar hydration or writes are involved.
    #[test]
    #[ignore = "set GARDEN_STORAGE_BENCH_GRAPH to an existing graph directory"]
    fn compare_existing_manifest_readers() {
        let root = std::env::var("GARDEN_STORAGE_BENCH_GRAPH").unwrap();
        let dir = Path::new(&root);
        assert!(crate::paths::documents_dir(dir).is_dir());
        let signature = |docs: &[DocumentRecord]| {
            docs.iter()
                .map(|d| {
                    serde_json::json!([
                        d.document_id,
                        d.graph_id,
                        d.title,
                        d.body,
                        d.created_at,
                        d.updated_at,
                        d.blocks,
                        d.rdf_triple_count
                    ])
                })
                .collect::<Vec<_>>()
        };
        let started = std::time::Instant::now();
        let cold = crate::document_record_store::read_graph_documents_cold(dir).unwrap();
        let cold_elapsed = started.elapsed();
        let expected = signature(&cold);
        drop(cold);
        let started = std::time::Instant::now();
        let narrow = read_search_only_documents(dir).unwrap();
        let narrow_elapsed = started.elapsed();
        assert_eq!(signature(&narrow), expected);
        eprintln!(
            "documents={} cold_ms={} narrow_ms={}",
            narrow.len(),
            cold_elapsed.as_millis(),
            narrow_elapsed.as_millis()
        );
    }

    #[test]
    fn search_walk_matches_cold_ranking_order_and_tombstones() {
        let dir = std::env::temp_dir().join(format!("search-parity-{}", uuid::Uuid::new_v4()));
        for (id, updated, body, blocks) in [
            ("old", "1000", "needle in body", json!([])),
            (
                "new",
                "3000",
                "",
                json!([{"id":"b1","type":"heading","content":"needle heading","order":0.0,"marks":[]}]),
            ),
            ("title", "2000", "", json!([])),
            ("deleted", "4000", "needle deleted", json!([])),
        ] {
            let path = crate::paths::documents_dir(&dir)
                .join(id)
                .join("document.json");
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            crate::storage::write_json(&path, &json!({
                "documentId":id,"graphId":"g","title":"needle title","body":body,
                "origin":"local","providerId":"local","localPath":"unused","rdfSubject":"unused",
                "createdAt":"500","updatedAt":updated,"capabilities":[],"blocks":blocks,
                "tiptapXml":"<doc/>","tiptapJson":{"type":"doc","content":[]},
                "ydocUpdateBase64":"legacy-payload-never-hydrated"
            })).unwrap();
        }
        crate::document_tombstone_store::write_document_tombstone_for_operation(
            &dir, "deleted", None,
        )
        .unwrap();
        let cold = crate::document_record_store::read_graph_documents_cold(&dir).unwrap();
        let narrow = read_search_only_documents(&dir).unwrap();
        assert_eq!(
            narrow
                .iter()
                .map(|d| d.document_id.as_str())
                .collect::<Vec<_>>(),
            vec!["new", "title", "old"]
        );
        for filter in [None, Some("old"), Some("new"), Some("title")] {
            let queries = vec!["needle".to_string(), "heading".to_string()];
            let hits = |docs: &[DocumentRecord]| {
                crate::search_lexical_projection::ranked_lexical_block_hits(
                    docs, &queries, filter, false,
                )
                .into_iter()
                .map(|h| (h.document_id, h.block_id, h.content, h.score, h.order))
                .collect::<Vec<_>>()
            };
            assert_eq!(hits(&cold), hits(&narrow));
        }
        for (a, b) in cold.iter().zip(&narrow) {
            assert_eq!(a.created_at, b.created_at);
            assert_eq!(a.updated_at, b.updated_at);
            assert_eq!(
                serde_json::to_value(&a.blocks).unwrap(),
                serde_json::to_value(&b.blocks).unwrap()
            );
        }
        assert!(
            !dir.join("ydocs/documents").exists(),
            "search must not backfill sidecars"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn search_manifest_skips_other_representations_and_preserves_fallback() {
        let dir = std::env::temp_dir().join(format!("search-manifest-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("document.json");
        let mut manifest = json!({
            "documentId":"doc-a", "graphId":"g", "title":"Title", "body":"legacy body",
            "createdAt":"1000", "updatedAt":"2000", "rdfTripleCount":42,
            "tree":{"not":"a document tree"}, "tiptapJson":{"large":[1,2,3]},
            "tiptapXml":"<paragraph>unused</paragraph>", "ydocUpdateBase64":"not-base64"
        });
        fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let record = read_search_document(&dir, &path).unwrap();
        assert_eq!(record.created_at, "1000");
        assert_eq!(record.rdf_triple_count, 42);
        assert!(record.tree.is_none() && record.tiptap_json.is_none());
        assert!(record.ydoc_update_base64.is_empty() && record.tiptap_xml.is_empty());
        let blocks = crate::document_block_rendering::document_blocks_for_read(&record);
        assert_eq!(blocks[0].id, "body");
        assert_eq!(blocks[0].content, "legacy body");
        assert!(!dir.join("ydocs").exists());

        manifest["documentId"] = json!("../unsafe");
        fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert!(read_search_document(&dir, &path).is_err());
        fs::write(&path, b"{\"documentId\":").unwrap();
        assert!(read_search_document(&dir, &path).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}
