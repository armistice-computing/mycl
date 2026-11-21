use crate::document_chronicle::{ChronicleActor, ChronicleRestore, DocumentChronicle};
use crate::runtime_config::DOCUMENT_SCHEMA_VERSION;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CreateDocumentInput {
    pub(super) graph_id: String,
    pub(super) title: String,
    pub(super) document_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SaveDocumentInput {
    pub(super) graph_id: String,
    pub(super) document_id: String,
    pub(super) title: String,
    pub(super) body: String,
    pub(super) tiptap_xml: Option<String>,
    pub(super) tiptap_json: Option<serde_json::Value>,
    pub(super) ydoc_update_base64: Option<String>,
    pub(super) tree: Option<DocumentTreeSnapshot>,
    pub(super) blocks: Option<Vec<BlockSnapshot>>,
    pub(super) trace_operation_id: Option<String>,
    /// Optimistic-concurrency revision guard (A2 item 9).
    /// If present: increment only if current == expected (normal write) or
    /// current == expected + 1 (idempotent replay).  Absent: increment
    /// unconditionally (backwards-compatible).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) expected_revision: Option<u64>,
    /// Who is writing (chronicle). Absent = the local human owner.
    #[serde(default)]
    pub(super) actor: Option<ChronicleActor>,
    /// Explicit staleness-guard bypass; recorded on the record when true.
    #[serde(default)]
    pub(super) force: bool,
    /// Import only: restore the original clock and authorship instead of
    /// stamping this save's instant.
    #[serde(default)]
    pub(super) chronicle_restore: Option<ChronicleRestore>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct WorkspaceRecord {
    pub(super) graph_id: String,
    pub(super) ydoc_update_base64: String,
    pub(super) ydoc_state_path: String,
    pub(super) snapshot: Option<serde_json::Value>,
    pub(super) snapshot_path: String,
    pub(super) updated_at: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SaveWorkspaceInput {
    pub(super) graph_id: String,
    pub(super) ydoc_update_base64: String,
    pub(super) snapshot: Option<serde_json::Value>,
    pub(super) trace_operation_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SaveWorkspaceYDocStateInput {
    pub(super) graph_id: String,
    pub(super) ydoc_update_base64: String,
    pub(super) trace_operation_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(super) struct DocumentRecord {
    pub(super) document_id: String,
    pub(super) graph_id: String,
    pub(super) title: String,
    #[serde(default)]
    pub(super) revision: u64,
    pub(super) body: String,
    pub(super) origin: String,
    pub(super) provider_id: String,
    pub(super) local_path: String,
    pub(super) rdf_subject: String,
    pub(super) created_at: String,
    pub(super) updated_at: String,
    pub(super) capabilities: Vec<String>,
    #[serde(default = "default_document_schema_version")]
    pub(super) schema_version: u32,
    #[serde(default)]
    pub(super) tiptap_xml: String,
    #[serde(default)]
    pub(super) tiptap_json: Option<serde_json::Value>,
    #[serde(default)]
    pub(super) ydoc_update_base64: String,
    #[serde(default)]
    pub(super) ydoc_state_path: String,
    #[serde(default)]
    pub(super) tree: Option<DocumentTreeSnapshot>,
    #[serde(default)]
    pub(super) blocks: Vec<BlockSnapshot>,
    #[serde(default)]
    pub(super) rdf_triple_count: usize,
    /// Authorship and provenance (the chronicle), flattened into the record
    /// so `document.json` stays one flat object. Every field defaults.
    #[serde(default, flatten)]
    pub(super) chronicle: DocumentChronicle,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(super) struct DocumentTreeSnapshot {
    pub(super) root: TreeNodeSnapshot,
    pub(super) doc_id: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(super) struct TreeNodeSnapshot {
    pub(super) kind: String,
    pub(super) tag_name: Option<String>,
    pub(super) text_content: Option<String>,
    pub(super) attributes: TreeNodeAttributes,
    pub(super) children: Vec<TreeNodeSnapshot>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub(super) struct TreeNodeAttributes {
    pub(super) block_id: Option<String>,
    pub(super) level: Option<i64>,
    pub(super) href: Option<String>,
    pub(super) target: Option<String>,
    pub(super) language: Option<String>,
    pub(super) checked: Option<bool>,
    pub(super) footnote_content: Option<String>,
    pub(super) annotation_id: Option<String>,
    pub(super) wire_id: Option<String>,
    pub(super) src: Option<String>,
    pub(super) alt: Option<String>,
    pub(super) extra: BTreeMap<String, String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(super) struct BlockSnapshot {
    pub(super) id: String,
    #[serde(rename = "type")]
    pub(super) block_type: String,
    pub(super) content: String,
    pub(super) parent_id: Option<String>,
    pub(super) order: f64,
    pub(super) level: Option<i64>,
    pub(super) checked: Option<bool>,
    pub(super) language: Option<String>,
    pub(super) marks: Vec<InlineMarkSnapshot>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(super) struct InlineMarkSnapshot {
    pub(super) id: String,
    #[serde(rename = "type")]
    pub(super) mark_type: String,
    pub(super) start: u32,
    pub(super) end: u32,
    pub(super) href: Option<String>,
    pub(super) target_doc_id: Option<String>,
    pub(super) label: Option<String>,
    // The projection emits these for wikilinks (block/graph targets), comment
    // anchors, and wire anchors. Until 2026-09-12 the record schema dropped
    // them on the way through, so every read built from the stored blocks
    // lost the anchor (18 documents, 28 marks in the cutover rehearsal).
    pub(super) target_block_id: Option<String>,
    pub(super) target_graph_id: Option<String>,
    pub(super) annotation_id: Option<String>,
    pub(super) wire_id: Option<String>,
    // A file link (artifacts step 3): the artifact the link's wire targets,
    // the path as typed, and the region short form (L5-L8).
    #[serde(default)]
    pub(super) target_artifact_id: Option<String>,
    #[serde(default)]
    pub(super) file_path: Option<String>,
    #[serde(default)]
    pub(super) region: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SaveDocumentYDocStateInput {
    pub(super) graph_id: String,
    pub(super) document_id: String,
    pub(super) ydoc_update_base64: String,
    pub(super) trace_operation_id: Option<String>,
}

fn default_document_schema_version() -> u32 {
    DOCUMENT_SCHEMA_VERSION
}
