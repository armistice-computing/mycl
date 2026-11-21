use crate::app_runtime::AppHandle;
use crate::{
    clock::{parse_timestamp, timestamp},
    document_service::read_graph_documents_cold,
    paths::semantic_index_path,
    semantic_index::{
        read_semantic_index, semantic_index_document_count, write_semantic_index,
        SemanticIndexStatus,
    },
    semantic_models::{read_semantic_model_config, semantic_model_spec_by_id},
    semantic_search_projection::semantic_block_sources,
    storage::display_path,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

pub(super) fn semantic_index_status(
    app: &AppHandle,
    graph_dir: &Path,
    graph_id: &str,
) -> Result<SemanticIndexStatus, String> {
    let index_path = semantic_index_path(graph_dir);
    let documents = read_graph_documents_cold(graph_dir)?;
    let config = read_semantic_model_config(app)?;
    let spec = semantic_model_spec_by_id(&config.selected_model_id)?;
    if !index_path.is_file() {
        return Ok(SemanticIndexStatus {
            generation: String::new(),
            enrichment: crate::semantic_enrichment_state::status(graph_dir, None),
            graph_id: graph_id.to_string(),
            provider_id: spec.provider_id.to_string(),
            model_id: spec.model_id.to_string(),
            dimensions: spec.dimensions,
            active_model_id: spec.model_id.to_string(),
            active_dimensions: spec.dimensions,
            compatible: true,
            compatibility_reason: "not-indexed".to_string(),
            exists: false,
            index_path: display_path(&index_path),
            block_count: 0,
            document_count: documents.len(),
            stale_document_count: documents.len(),
            indexed_at: None,
        });
    }

    let index = read_semantic_index(graph_dir)?;
    let sources = semantic_block_sources(&documents);
    let indexed_at = parse_timestamp(&index.manifest.indexed_at);
    let mut stale_document_ids = BTreeSet::<String>::new();
    let compatible =
        index.manifest.model_id == spec.model_id && index.manifest.dimensions == spec.dimensions;
    if !compatible {
        for document in &documents {
            stale_document_ids.insert(document.document_id.clone());
        }
    }

    let mut blocks_by_id = BTreeMap::new();
    for block in &index.blocks {
        blocks_by_id
            .entry((block.document_id.as_str(), block.block_id.as_str()))
            .or_insert(block);
    }
    let indexed_docs = index
        .blocks
        .iter()
        .map(|b| b.document_id.as_str())
        .collect::<BTreeSet<_>>();
    let source_docs = sources
        .iter()
        .map(|s| s.document_id.as_str())
        .collect::<BTreeSet<_>>();
    let current_docs = documents
        .iter()
        .map(|d| d.document_id.as_str())
        .collect::<BTreeSet<_>>();
    if compatible {
        for source in &sources {
            let matching =
                blocks_by_id.get(&(source.document_id.as_str(), source.block_id.as_str()));
            if matching.is_none_or(|block| block.content_hash != source.content_hash) {
                stale_document_ids.insert(source.document_id.clone());
            }
        }

        for document in &documents {
            let updated_after_index = match (parse_timestamp(&document.updated_at), indexed_at) {
                (Some(updated_at), Some(indexed_at)) => updated_at > indexed_at,
                _ => false,
            };
            if !updated_after_index {
                continue;
            }

            let has_indexed_blocks = indexed_docs.contains(document.document_id.as_str());
            let has_current_sources = source_docs.contains(document.document_id.as_str());
            if has_indexed_blocks || has_current_sources {
                stale_document_ids.insert(document.document_id.clone());
            }
        }

        for block in &index.blocks {
            if !current_docs.contains(block.document_id.as_str()) {
                stale_document_ids.insert(block.document_id.clone());
            }
        }
    }

    Ok(SemanticIndexStatus {
        generation: index.manifest.generation.clone(),
        enrichment: crate::semantic_enrichment_state::status(graph_dir, Some(&index.manifest)),
        graph_id: graph_id.to_string(),
        provider_id: index.manifest.provider_id,
        model_id: index.manifest.model_id.clone(),
        dimensions: index.manifest.dimensions,
        active_model_id: spec.model_id.to_string(),
        active_dimensions: spec.dimensions,
        compatible,
        compatibility_reason: if compatible {
            "compatible".to_string()
        } else {
            format!(
                "active model {} has {} dimensions; index was built with {} at {} dimensions",
                spec.model_id, spec.dimensions, index.manifest.model_id, index.manifest.dimensions
            )
        },
        exists: true,
        index_path: display_path(&index_path),
        block_count: index.blocks.len(),
        document_count: documents.len(),
        stale_document_count: stale_document_ids.len(),
        indexed_at: Some(index.manifest.indexed_at),
    })
}

pub(super) fn remove_document_from_semantic_index(
    graph_dir: &Path,
    document_id: &str,
) -> Result<(), String> {
    let lock = crate::semantic_index::semantic_index_graph_lock(graph_dir);
    let _guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let index_path = semantic_index_path(graph_dir);
    if !index_path.is_file() {
        return Ok(());
    }
    let mut index = read_semantic_index(graph_dir)?;
    let previous_len = index.blocks.len();
    index
        .blocks
        .retain(|block| block.document_id != document_id);
    if index.blocks.len() == previous_len {
        return Ok(());
    }
    index.manifest.block_count = index.blocks.len();
    index.manifest.document_count = semantic_index_document_count(&index.blocks);
    index.manifest.indexed_at = timestamp();
    index.manifest.generation = uuid::Uuid::new_v4().to_string();
    crate::semantic_enrichment_state::invalidate(graph_dir)?;
    write_semantic_index(graph_dir, &index)
}
