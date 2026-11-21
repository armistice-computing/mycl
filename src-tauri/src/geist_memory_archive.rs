use crate::{
    app_runtime::AppHandle,
    document_service::BlockSnapshot,
    geist_memory_store::{memory_text, LocalMemoryRecord},
    geist_projection_document::write_geist_projection_document,
};
use std::path::Path;

pub(crate) fn write_memory_archive_document(
    app: &AppHandle,
    graph_dir: &Path,
    graph_id: &str,
    archive_doc_id: &str,
    archived_at: &str,
    kept: usize,
    memories: &[LocalMemoryRecord],
) -> Result<(), String> {
    let title = format!("Memory Archive {archived_at}");
    write_geist_projection_document(
        app,
        graph_dir,
        graph_id,
        archive_doc_id,
        &title,
        memory_archive_blocks(&title, kept, memories),
        vec![
            "document.local.read".to_string(),
            "document.local.materialize.rdf".to_string(),
            "memory.local.archive".to_string(),
        ],
        Some(archived_at),
    )
}

fn memory_archive_blocks(
    title: &str,
    kept: usize,
    memories: &[LocalMemoryRecord],
) -> Vec<BlockSnapshot> {
    let mut blocks = Vec::new();
    blocks.push(BlockSnapshot {
        id: "archive-heading".to_string(),
        block_type: "heading".to_string(),
        content: title.to_string(),
        parent_id: None,
        order: 0.0,
        level: Some(1),
        checked: None,
        language: None,
        marks: Vec::new(),
    });
    blocks.push(BlockSnapshot {
        id: "archive-summary".to_string(),
        block_type: "paragraph".to_string(),
        content: format!(
            "Archived {} memories. Kept {} most recently active.",
            memories.len(),
            kept
        ),
        parent_id: None,
        order: 1.0,
        level: None,
        checked: None,
        language: None,
        marks: Vec::new(),
    });
    for (index, memory) in memories.iter().enumerate() {
        blocks.push(BlockSnapshot {
            id: format!("memory-archive-{}", memory.number),
            block_type: "paragraph".to_string(),
            content: memory_text(memory),
            parent_id: None,
            order: (index + 2) as f64,
            level: None,
            checked: None,
            language: None,
            marks: Vec::new(),
        });
    }

    blocks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{crdt_engine::projection, geist_projection_document::project_geist_blocks};
    use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
    use yrs::{updates::decoder::Decode, Doc, Transact, Update};

    #[test]
    fn memory_archive_blocks_project_content_authority() {
        let archived_at = "1789056000123";
        let memories = vec![LocalMemoryRecord {
            anchors: Vec::new(),
            number: 42,
            block_id: "original-memory-block".into(),
            content: "Keep <the forest> & its voices.".into(),
            created_at: "1000".into(),
            last_active: "2000".into(),
            rememberer: None,
        }];
        let title = format!("Memory Archive {archived_at}");
        let blocks = memory_archive_blocks(&title, 5, &memories);
        let projected = project_geist_blocks("archive-a", &blocks).expect("project archive");
        let bytes = BASE64_STANDARD
            .decode(&projected.ydoc_update_base64)
            .expect("encoded authority");
        let doc = Doc::new();
        doc.transact_mut()
            .apply_update(Update::decode_v1(&bytes).expect("valid update"))
            .expect("apply projected update");
        let snapshot = projection::materialize_ydoc(&doc, "archive-a");
        let content = snapshot.tiptap_json["content"]
            .as_array()
            .expect("content array");
        assert_eq!(content.len(), 3, "archive authority must not be empty");
        assert_eq!(content[0]["type"], "heading");
        assert_eq!(content[0]["attrs"]["level"].as_f64(), Some(1.0));
        assert_eq!(content[0]["content"][0]["text"], title);
        assert_eq!(
            content[1]["content"][0]["text"],
            "Archived 1 memories. Kept 5 most recently active."
        );
        assert_eq!(content[2]["attrs"]["data-block-id"], "memory-archive-42");
        assert_eq!(
            content[2]["content"][0]["text"],
            "42. Keep <the forest> & its voices."
        );
        assert_eq!(projected.snapshot.body, snapshot.body);
        assert_eq!(projected.snapshot.tiptap_json, snapshot.tiptap_json);
        assert_eq!(projected.tiptap_xml, projection::ydoc_to_tiptap_xml(&doc));
        assert_eq!(projected.snapshot.tree_json, snapshot.tree_json);
        assert_eq!(projected.snapshot.blocks_json, snapshot.blocks_json);
    }
}
