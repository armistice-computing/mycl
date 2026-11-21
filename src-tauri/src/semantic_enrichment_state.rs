//! A small commit receipt makes enrichment readiness cheap and fail-closed.
//! It is written last, after both artifact files and the owned RDF projection.
use crate::{
    semantic_index::SemanticIndexManifest,
    storage::{read_json, write_json},
};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EnrichmentStatus {
    /// Advisory receipt/file presence; actual artifacts are parsed on use.
    pub(crate) available: bool,
    pub(crate) projection_stale: bool,
    pub(crate) source_generation_matches: bool,
    pub(crate) reason: String,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Receipt {
    generation: String,
    graph_id: String,
    model_id: String,
    dimensions: usize,
}
fn receipt_path(dir: &Path) -> std::path::PathBuf {
    crate::semantic_index_paths::semantic_index_dir(dir).join("enrichment-ready.json")
}
pub(crate) fn status(dir: &Path, index: Option<&SemanticIndexManifest>) -> EnrichmentStatus {
    let receipt = read_json::<Receipt>(&receipt_path(dir)).ok();
    let matches = match (index, receipt.as_ref()) {
        (Some(i), Some(r)) => {
            !i.generation.is_empty()
                && i.generation == r.generation
                && i.graph_id == r.graph_id
                && i.model_id == r.model_id
                && i.dimensions == r.dimensions
        }
        _ => false,
    };
    let available = matches
        && crate::semantic_index_paths::semantic_scaffold_path(dir).is_file()
        && crate::semantic_index_paths::semantic_relation_profiles_path(dir).is_file();
    EnrichmentStatus {
        available,
        projection_stale: !matches,
        source_generation_matches: matches,
        reason: if available {
            "receipt current; artifacts verified on use".into()
        } else {
            "semantic enrichment unavailable, stale, or incomplete; request a semantic refresh with enrich=true".into()
        },
    }
}
/// Caller holds the semantic index publication lock. Invalidate before index
/// replacement so a crash can never advertise a half-published enrichment.
pub(crate) fn invalidate(dir: &Path) -> Result<(), String> {
    crate::storage::remove_file_if_exists(&receipt_path(dir)).map_err(String::from)?;
    Ok(())
}
pub(crate) fn mark_ready(dir: &Path, index: &SemanticIndexManifest) -> Result<(), String> {
    write_json(
        &receipt_path(dir),
        &Receipt {
            generation: index.generation.clone(),
            graph_id: index.graph_id.clone(),
            model_id: index.model_id.clone(),
            dimensions: index.dimensions,
        },
    )
    .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn semantic_invalidation_and_deletion_ignore_unavailable_rdf() {
        use crate::semantic_index::{read_semantic_index, write_semantic_index, SemanticIndexFile};
        let dir = std::env::temp_dir().join(format!("semantic-no-rdf-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("store.oxigraph"), "not a database directory").unwrap();
        let mut index: SemanticIndexFile = serde_json::from_value(serde_json::json!({
            "manifest":{"generation":"old","schemaVersion":1,"graphId":"g","providerId":"test","modelId":"test","dimensions":1,"blockCount":1,"documentCount":1,"indexedAt":"1","indexPath":"blocks.json"},
            "blocks":[{"iri":"urn:b","kind":"block","graphId":"g","documentId":"d","documentTitle":"d","blockId":"b","blockType":"paragraph","content":"text","contentHash":"h","order":0.0,"vector":[1.0]}],"entities":[]
        })).unwrap();
        write_semantic_index(&dir, &index).unwrap();
        mark_ready(&dir, &index.manifest).unwrap();
        // Same file-only publication operations used by refresh, with RDF unusable.
        invalidate(&dir).unwrap();
        index.manifest.generation = uuid::Uuid::new_v4().to_string();
        write_semantic_index(&dir, &index).unwrap();
        assert_eq!(
            read_semantic_index(&dir).unwrap().manifest.generation,
            index.manifest.generation
        );
        assert!(!status(&dir, Some(&index.manifest)).available);
        crate::semantic_index_status::remove_document_from_semantic_index(&dir, "d").unwrap();
        let deleted = read_semantic_index(&dir).unwrap();
        assert!(deleted.blocks.is_empty());
        assert_ne!(deleted.manifest.generation, index.manifest.generation);
        assert!(crate::rdf_service::open_graph_store(&dir).is_err());
        assert_eq!(
            std::fs::read_to_string(dir.join("store.oxigraph")).unwrap(),
            "not a database directory"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
