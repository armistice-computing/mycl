use crate::{
    crdt_engine::{builder, document_ops::block_to_tiptap_node, projection},
    document_service::{
        read_document_record, write_document_record, BlockSnapshot, DocumentRecord,
    },
    paths::{document_ydoc_state_path, ensure_document_dirs},
    rdf::document_subject,
    rdf_service::{
        document_tree_triples, materialize_document_record_with_triples, open_graph_store,
    },
    runtime_config::{DOCUMENT_SCHEMA_VERSION, LOCAL_GRAPH_ORIGIN, LOCAL_PROVIDER_ID},
    storage::display_path,
};
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use yrs::{ReadTxn, StateVector, Transact};
#[cfg(feature = "desktop")]
use tauri::Manager;

pub(crate) struct GeistProjection {
    pub(crate) snapshot: projection::ProjectionSnapshot,
    pub(crate) tiptap_xml: String,
    pub(crate) ydoc_update_base64: String,
}

pub(crate) fn project_geist_blocks(
    document_id: &str,
    blocks: &[BlockSnapshot],
) -> Result<GeistProjection, String> {
    let nodes = blocks
        .iter()
        .map(|block| {
            let value = serde_json::to_value(block)
                .map_err(|error| format!("serialize Geist block: {error}"))?;
            block_to_tiptap_node(&value)
        })
        .collect::<Result<Vec<_>, String>>()?;
    // Persist one authority and derive every cached representation from it.
    let doc = builder::ydoc_from_tiptap_json(&serde_json::json!({
        "type": "doc", "content": nodes,
    }));
    let snapshot = projection::materialize_ydoc(&doc, document_id);
    let tiptap_xml = projection::ydoc_to_tiptap_xml(&doc);
    let ydoc_update_base64 = BASE64_STANDARD.encode(
        doc.transact()
            .encode_state_as_update_v1(&StateVector::default()),
    );
    Ok(GeistProjection {
        snapshot,
        tiptap_xml,
        ydoc_update_base64,
    })
}

pub(crate) fn write_geist_projection_document(
    app: &crate::app_runtime::AppHandle,
    graph_dir: &std::path::Path,
    graph_id: &str,
    document_id: &str,
    title: &str,
    blocks: Vec<BlockSnapshot>,
    capabilities: Vec<String>,
    timestamp_override: Option<&str>,
) -> Result<(), String> {
    let prepared = project_geist_blocks(document_id, &blocks)?;
    let nodes = prepared.snapshot.tiptap_json["content"].as_array().ok_or("missing projection content")?;
    crate::app_runtime::async_runtime::block_on(async {
        let registry = app.state::<crate::crdt_engine::rooms::RoomRegistry>();
        let room = registry.get_or_create(&format!("doc:{graph_id}:{document_id}"),
            crate::ydoc_paths::checked_document_ydoc_state_path(graph_dir, document_id)?).await?;
        let flush_guard = room.lock_projection_flush().await;
        let has_authority = graph_dir.join("documents").join(document_id).join("document.json").exists()
            || crate::ydoc_paths::document_ydoc_state_path(graph_dir, document_id).exists()
            || room.with_doc(|doc| !doc.transact().state_vector().is_empty()).await;
        if has_authority
            && !crate::geist_derived_view::is_derived(graph_dir, document_id)? {
            return Err(format!("Cannot overwrite ordinary document {document_id} with a Geist view"));
        }
        crate::geist_derived_view::register(graph_dir, document_id)?;
        room.replace_derived_content(nodes).await?;
        let (epoch, projected) = room.with_doc_version(|doc| GeistProjection {
            snapshot: projection::materialize_ydoc(doc, document_id),
            tiptap_xml: projection::ydoc_to_tiptap_xml(doc),
            ydoc_update_base64: BASE64_STANDARD.encode(doc.transact().encode_state_as_update_v1(&StateVector::default())),
        }).await;
        persist_projection_record(graph_dir, graph_id, document_id, title, projected, capabilities.clone(), timestamp_override)?;
        room.mark_projection_persisted(epoch);
        drop(flush_guard);
        // Publish the view's read-only status through the workspace clients use.
        // Preserve existing placement instead of moving it on every Song update.
        let ws = registry.get_or_create(&format!("workspace:{graph_id}"), crate::ydoc_paths::workspace_ydoc_state_path(graph_dir)).await?;
        let existing = ws.with_doc(|doc| {
            crate::crdt_engine::workspace_ops::materialize_workspace_snapshot_json(graph_id, doc)
        }).await?;
        let prior = existing["documents"].as_array().and_then(|items|items.iter().find(|item| item["id"] == document_id));
        let now = crate::clock::epoch_millis() as f64;
        crate::crdt_engine::workspace_ops::upsert_document_entry(app, graph_id, &serde_json::json!({
            "documentId":document_id,"title":title,"readOnly":true,
            "parentId":match prior {
                Some(prior) => prior.get("parentId").cloned().unwrap_or(serde_json::Value::Null),
                None => default_geist_parent(&capabilities, &existing),
            },
            "order":prior.and_then(|p|p.get("order")).cloned().unwrap_or_else(||serde_json::json!(now)),
            "updatedAt":now,
        }), "geist-projection").await?;
        Ok(())
    })
}

/// Where a Geist view is filed the first time it gets a workspace row: the
/// house's `_sophia` folder (`agent-scratchpad`), past or present by kind, as
/// cloud-1 kept them. Only folders that already exist in this graph are used,
/// so a graph without them keeps its views at the root and nothing of the
/// house's layout is created in someone else's graph. Later placement is the
/// owner's: an existing row keeps its parent.
fn default_geist_parent(capabilities: &[String], workspace: &serde_json::Value) -> serde_json::Value {
    let has_folder = |id: &str| {
        workspace["folders"]
            .as_array()
            .is_some_and(|folders| folders.iter().any(|folder| folder["id"] == id))
    };
    let has = |capability: &str| capabilities.iter().any(|c| c == capability);
    let preferred: &[&str] = if has("narrative.local.archive") || has("memory.local.archive") {
        &["geist-past", "agent-scratchpad"]
    } else if has("dreaming.local.journal") {
        &["agent-scratchpad"]
    } else {
        &["geist-present", "agent-scratchpad"]
    };
    preferred
        .iter()
        .find(|id| has_folder(id))
        .map(|id| serde_json::json!(id))
        .unwrap_or(serde_json::Value::Null)
}

fn persist_projection_record(
    graph_dir: &std::path::Path, graph_id: &str, document_id: &str, title: &str,
    projected: GeistProjection, capabilities: Vec<String>, timestamp_override: Option<&str>,
) -> Result<(), String> {
    let (document_dir, _) = ensure_document_dirs(graph_dir, document_id)?;
    let existing = document_dir
        .join("document.json")
        .is_file()
        .then(|| read_document_record(graph_dir, &document_dir.join("document.json")))
        .transpose()?;
    let now = timestamp_override.map(str::to_string).unwrap_or_else(crate::clock::timestamp);
    let GeistProjection {
        snapshot,
        tiptap_xml,
        ydoc_update_base64,
    } = projected;
    let mut document = DocumentRecord {
        document_id: document_id.to_string(),
        graph_id: graph_id.to_string(),
        title: title.to_string(),
        revision: existing
            .as_ref()
            .map(|document| document.revision.saturating_add(1))
            .unwrap_or(1),
        body: snapshot.body,
        origin: LOCAL_GRAPH_ORIGIN.to_string(),
        provider_id: LOCAL_PROVIDER_ID.to_string(),
        local_path: display_path(&document_dir),
        rdf_subject: document_subject(document_id),
        created_at: existing
            .as_ref()
            .map(|document| document.created_at.clone())
            .unwrap_or_else(|| now.clone()),
        updated_at: now,
        capabilities,
        schema_version: DOCUMENT_SCHEMA_VERSION,
        tiptap_xml,
        tiptap_json: Some(snapshot.tiptap_json),
        ydoc_update_base64,
        ydoc_state_path: display_path(&document_ydoc_state_path(graph_dir, document_id)),
        tree: Some(
            serde_json::from_value(snapshot.tree_json)
                .map_err(|error| format!("decode Geist tree projection: {error}"))?,
        ),
        blocks: serde_json::from_value(snapshot.blocks_json)
            .map_err(|error| format!("decode Geist block projection: {error}"))?,
        rdf_triple_count: 0,
        chronicle: Default::default(),
    };
    let tree_triples = document_tree_triples(&document);
    document.rdf_triple_count = tree_triples.len();
    write_document_record(graph_dir, &document)?;
    let store = open_graph_store(graph_dir)?;
    materialize_document_record_with_triples(&store, &document, &tree_triples)?;
    // Settlement: Geist projections (the Song, memory archives) are real,
    // searchable documents written outside the save_document choke point;
    // without this mark a graph whose only activity is singing never
    // settles (angel finding — the bypass the choke point was built to
    // prevent).
    crate::document_settlement::mark_document_dirty_global(graph_id, document_id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use yrs::{updates::decoder::Decode, Doc, Update};

    #[test]
    fn first_projection_persists_content_authority_and_matching_snapshots() {
        let dir = std::env::temp_dir().join(format!("geist-projection-{}", uuid::Uuid::new_v4()));
        let blocks = vec![
            BlockSnapshot {
                id: "song-title".into(),
                block_type: "heading".into(),
                content: "The Song".into(),
                parent_id: None,
                order: 0.0,
                level: Some(1),
                checked: None,
                language: None,
                marks: Vec::new(),
            },
            BlockSnapshot {
                id: "song-line".into(),
                block_type: "paragraph".into(),
                content: "Keep <the forest> & its voices.".into(),
                parent_id: None,
                order: 1.0,
                level: None,
                checked: None,
                language: None,
                marks: Vec::new(),
            },
        ];
        persist_projection_record(
            &dir,
            "graph-a",
            "geist-song",
            "The Song",
            project_geist_blocks("geist-song", &blocks).unwrap(),
            Vec::new(),
            None,
        )
        .expect("write first projection");

        let bytes = std::fs::read(document_ydoc_state_path(&dir, "geist-song"))
            .expect("persisted authority");
        let doc = Doc::new();
        doc.transact_mut()
            .apply_update(Update::decode_v1(&bytes).expect("valid update"))
            .expect("apply persisted update");
        let snapshot = projection::materialize_ydoc(&doc, "geist-song");
        let content = snapshot.tiptap_json["content"]
            .as_array()
            .expect("content array");
        assert_eq!(content.len(), 2, "authority must not be an empty Y.Doc");
        assert_eq!(content[0]["type"], "heading");
        assert_eq!(content[0]["attrs"]["level"].as_f64(), Some(1.0));
        assert_eq!(content[0]["attrs"]["data-block-id"], "song-title");
        assert_eq!(content[1]["attrs"]["data-block-id"], "song-line");
        assert_eq!(
            content[1]["content"][0]["text"],
            "Keep <the forest> & its voices."
        );

        let record: DocumentRecord = serde_json::from_slice(
            &std::fs::read(dir.join("documents/geist-song/document.json")).unwrap(),
        )
        .expect("persisted record");
        assert_eq!(record.revision, 1);
        assert_eq!(record.body, snapshot.body);
        assert_eq!(record.tiptap_json.as_ref(), Some(&snapshot.tiptap_json));
        assert_eq!(record.tiptap_xml, projection::ydoc_to_tiptap_xml(&doc));
        assert_eq!(
            serde_json::to_value(record.tree).unwrap(),
            snapshot.tree_json
        );
        assert_eq!(
            serde_json::to_value(record.blocks).unwrap(),
            serde_json::to_value(serde_json::from_value::<Vec<BlockSnapshot>>(snapshot.blocks_json).unwrap()).unwrap()
        );
        assert_eq!(
            BASE64_STANDARD.decode(record.ydoc_update_base64).unwrap(),
            bytes
        );
        std::fs::remove_dir_all(dir).expect("remove fixture");
    }
}
