use crate::document_service::DocumentRecord;

pub(super) fn fallback_workspace_documents(
    document_records: &[DocumentRecord],
) -> Vec<serde_json::Value> {
    let mut documents = document_records
        .iter()
        .map(|document| {
            serde_json::json!({
                "type": "document",
                "document_id": document.document_id.clone(),
                "documentId": document.document_id.clone(),
                "title": document.title.clone(),
                "updated_at": document.updated_at.clone(),
                "updatedAt": document.updated_at.clone(),
                "createdAt": document.created_at.clone(),
                "revision": document.revision,
                "updatedBy": document.chronicle.updated_by.as_ref().map(|actor| actor.label()),
                "block_count": document.blocks.len(),
                "blockCount": document.blocks.len(),
                "rdfTripleCount": document.rdf_triple_count,
                "readOnly": false,
            })
        })
        .collect::<Vec<_>>();
    documents.sort_by(|left, right| {
        left.get("title")
            .and_then(serde_json::Value::as_str)
            .cmp(&right.get("title").and_then(serde_json::Value::as_str))
    });
    documents
}

pub(super) fn merge_workspace_document_counts(
    documents: &mut [serde_json::Value],
    document_records: &[DocumentRecord],
) {
    for document in documents {
        let Some(object) = document.as_object_mut() else {
            continue;
        };
        let document_id = object
            .get("documentId")
            .or_else(|| object.get("document_id"))
            .or_else(|| object.get("id"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let Some(record) = document_records
            .iter()
            .find(|record| record.document_id == document_id)
        else {
            continue;
        };
        object.insert(
            "document_id".to_string(),
            serde_json::json!(record.document_id),
        );
        object.insert(
            "documentId".to_string(),
            serde_json::json!(record.document_id),
        );
        object.insert(
            "block_count".to_string(),
            serde_json::json!(record.blocks.len()),
        );
        object.insert(
            "blockCount".to_string(),
            serde_json::json!(record.blocks.len()),
        );
        object.insert(
            "rdfTripleCount".to_string(),
            serde_json::json!(record.rdf_triple_count),
        );
        merge_workspace_document_chronicle(object, record);
    }
}

/// Chronicle: the workspace catalog's `updatedAt` only moves on catalog
/// events (create / move / rename / reorder) — a content edit through the
/// editor or an MCP write leaves it stale, and an imported catalog carries
/// whatever the source graph had (often null). The document record's clock
/// is the authority, so the catalog entry is lifted to it whenever the record
/// is newer, and `createdAt` is filled from the record when the catalog has
/// none. `revision` and `updatedBy` ride along for the recency surfaces.
fn merge_workspace_document_chronicle(
    object: &mut serde_json::Map<String, serde_json::Value>,
    record: &DocumentRecord,
) {
    let catalog_updated = object.get("updatedAt").and_then(serde_json::Value::as_f64);
    if let Ok(record_updated) = record.updated_at.parse::<u64>() {
        if catalog_updated.is_none_or(|catalog| (record_updated as f64) > catalog) {
            object.insert("updatedAt".to_string(), serde_json::json!(record_updated));
            object.insert("updated_at".to_string(), serde_json::json!(record_updated));
        }
    }
    let catalog_created = object.get("createdAt").and_then(serde_json::Value::as_f64);
    if catalog_created.is_none() {
        if let Ok(record_created) = record.created_at.parse::<u64>() {
            object.insert("createdAt".to_string(), serde_json::json!(record_created));
        }
    }
    object.insert("revision".to_string(), serde_json::json!(record.revision));
    if let Some(updated_by) = &record.chronicle.updated_by {
        object.insert("updatedBy".to_string(), serde_json::json!(updated_by.label()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document_service::{BlockSnapshot, DocumentRecord};
    use crate::runtime_config::{DOCUMENT_SCHEMA_VERSION, LOCAL_GRAPH_ORIGIN, LOCAL_PROVIDER_ID};

    fn test_block(id: &str) -> BlockSnapshot {
        BlockSnapshot {
            id: id.to_string(),
            block_type: "paragraph".to_string(),
            content: format!("content {id}"),
            parent_id: None,
            order: 0.0,
            level: None,
            checked: None,
            language: None,
            marks: vec![],
        }
    }

    fn test_document_record(id: &str, title: &str, blocks: Vec<BlockSnapshot>) -> DocumentRecord {
        DocumentRecord {
            document_id: id.to_string(),
            graph_id: "graph-a".to_string(),
            title: title.to_string(),
            revision: 1,
            body: String::new(),
            origin: LOCAL_GRAPH_ORIGIN.to_string(),
            provider_id: LOCAL_PROVIDER_ID.to_string(),
            local_path: format!("/tmp/{id}"),
            rdf_subject: format!("urn:test:{id}"),
            created_at: "1".to_string(),
            updated_at: "2".to_string(),
            capabilities: vec![],
            schema_version: DOCUMENT_SCHEMA_VERSION,
            tiptap_xml: String::new(),
            tiptap_json: None,
            ydoc_update_base64: String::new(),
            ydoc_state_path: String::new(),
            tree: None,
            blocks,
            rdf_triple_count: 0,
            chronicle: Default::default(),
        }
    }

    #[test]
    fn fallback_workspace_documents_sort_and_include_projection_counts() {
        let mut doc_b = test_document_record("doc-b", "B", vec![test_block("b1")]);
        doc_b.rdf_triple_count = 7;
        let doc_a = test_document_record("doc-a", "A", vec![test_block("a1"), test_block("a2")]);

        let values = fallback_workspace_documents(&[doc_b, doc_a]);

        assert_eq!(values[0]["documentId"], "doc-a");
        assert_eq!(values[0]["blockCount"], 2);
        assert_eq!(values[1]["document_id"], "doc-b");
        assert_eq!(values[1]["rdfTripleCount"], 7);
        assert_eq!(values[1]["readOnly"], false);
    }

    /// Chronicle: a stale or null catalog `updatedAt` is lifted to the
    /// record's authority clock; a newer catalog stamp (a move or rename after
    /// the last content save) is left alone.
    #[test]
    fn workspace_document_recency_is_lifted_to_the_record_clock() {
        let mut record = test_document_record("doc-a", "A", vec![test_block("a1")]);
        record.created_at = "1000".to_string();
        record.updated_at = "5000".to_string();
        record.revision = 4;
        record.chronicle.record_write(
            &crate::document_chronicle::ChronicleActor::agent("beta", None),
            "5000",
            4,
            false,
        );
        let mut documents = vec![
            serde_json::json!({ "id": "doc-a", "title": "A", "updatedAt": null, "createdAt": null }),
        ];
        merge_workspace_document_counts(&mut documents, std::slice::from_ref(&record));
        assert_eq!(documents[0]["updatedAt"], 5000);
        assert_eq!(documents[0]["createdAt"], 1000);
        assert_eq!(documents[0]["revision"], 4);
        assert_eq!(documents[0]["updatedBy"], "agent:beta");

        let mut stale = vec![serde_json::json!({ "id": "doc-a", "updatedAt": 2000.0 })];
        merge_workspace_document_counts(&mut stale, std::slice::from_ref(&record));
        assert_eq!(stale[0]["updatedAt"], 5000, "stale catalog stamp lifted");

        let mut newer = vec![serde_json::json!({ "id": "doc-a", "updatedAt": 9000.0, "createdAt": 500.0 })];
        merge_workspace_document_counts(&mut newer, std::slice::from_ref(&record));
        assert_eq!(newer[0]["updatedAt"], 9000.0, "newer catalog stamp kept");
        assert_eq!(newer[0]["createdAt"], 500.0, "catalog createdAt kept");
    }

    #[test]
    fn workspace_document_counts_merge_into_snapshot_documents() {
        let record = test_document_record("doc-a", "A", vec![test_block("a1"), test_block("a2")]);
        let mut documents = vec![serde_json::json!({
            "id": "doc-a",
            "title": "Snapshot Title"
        })];

        merge_workspace_document_counts(&mut documents, &[record]);

        assert_eq!(documents[0]["document_id"], "doc-a");
        assert_eq!(documents[0]["documentId"], "doc-a");
        assert_eq!(documents[0]["block_count"], 2);
        assert_eq!(documents[0]["blockCount"], 2);
    }
}
