//! Internal new-graph assembly. Public command/route registration remains gated.
//! The caller must retain prepared custody and supply final publication authority.
use crate::{
    graph_duplicate_storage::{ExistingTargetPolicy, GraphPublicationReservation},
    graph_record_store::GraphRecord,
    vault_note::PreparedNote,
    vault_plan::Plan,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};
use yrs::{Map, MapPrelim, ReadTxn, StateVector, Transact, WriteTxn};

#[path = "vault_graph_files.rs"]
mod files;
pub(crate) use files::artifact_id;

pub(crate) struct StagedGraph {
    publication: GraphPublicationReservation,
    graph: GraphRecord,
}

impl Drop for StagedGraph {
    fn drop(&mut self) {
        let _durability = crate::cell_durability::write_guard();
        for error in self.publication.cleanup_failed_stage(false) {
            log::warn!("unpublished vault stage cleanup: {error}");
        }
    }
}

impl StagedGraph {
    /// No rollback after publication. The caller handles repairable catalog tails
    /// and durable job completion; never interpret a lost reply as no publication.
    /// Caller holds lifecycle leases before assemble takes the publication lock;
    /// this callback must not acquire graph leases in the opposite order.
    pub(crate) fn publish<G>(
        self,
        final_authorize: impl FnOnce() -> Result<G, String>,
    ) -> Result<GraphRecord, String> {
        let _authority = final_authorize()?;
        let _durability = crate::cell_durability::write_guard();
        self.publication.publish_graph_record(&self.graph)?;
        self.publication.sync_publication_parents_best_effort();
        Ok(self.graph.clone())
    }
}

fn put(txn: &mut yrs::TransactionMut<'_>, collection: &str, id: &str, value: Value) {
    let root = txn.get_or_insert_map(collection);
    let row = root.insert(txn, id, MapPrelim::default());
    for (key, value) in value.as_object().unwrap() {
        row.insert(
            txn,
            key.as_str(),
            crate::crdt_engine::builder::json_to_any(value),
        );
    }
}

/// Blocking, hidden assembly only. `checkpoint` permits cancellation/fault tests
/// between notes; its errors discard only this reservation's unpublished stage.
pub(crate) fn assemble(
    profile: &Path,
    graph: GraphRecord,
    plan: &Plan,
    notes: &[PreparedNote],
    checkpoint: impl FnMut(usize) -> Result<(), String>,
) -> Result<StagedGraph, String> {
    assemble_with_files(
        profile,
        graph,
        plan,
        notes,
        &[],
        |_| Err::<std::io::Cursor<Vec<u8>>, _>("no captured file reader".into()),
        checkpoint,
    )
}

pub(crate) fn assemble_with_files<R: std::io::Read + std::io::Seek>(
    profile: &Path,
    graph: GraphRecord,
    plan: &Plan,
    notes: &[PreparedNote],
    file_entries: &[crate::vault_plan::Input],
    read_file: impl FnMut(usize) -> Result<R, String>,
    checkpoint: impl FnMut(usize) -> Result<(), String>,
) -> Result<StagedGraph, String> {
    assemble_tracked(
        profile,
        graph,
        plan,
        notes,
        file_entries,
        read_file,
        |_| Ok(()),
        checkpoint,
    )
}

pub(crate) fn assemble_tracked<R: std::io::Read + std::io::Seek>(
    profile: &Path,
    graph: GraphRecord,
    plan: &Plan,
    notes: &[PreparedNote],
    file_entries: &[crate::vault_plan::Input],
    read_file: impl FnMut(usize) -> Result<R, String>,
    register: impl FnOnce(&Path) -> Result<(), String>,
    mut checkpoint: impl FnMut(usize) -> Result<(), String>,
) -> Result<StagedGraph, String> {
    crate::ids::validate_local_id(&graph.graph_id, "graph_id")?;
    let target = profile.join("graphs").join(&graph.graph_id);
    if graph.status != crate::runtime_config::GRAPH_STATUS_ACTIVE
        || graph.local_path != crate::storage::display_path(&target)
        || graph.incarnation_id.as_deref().is_none_or(str::is_empty)
        || graph
            .created_by_operation_id
            .as_deref()
            .is_none_or(str::is_empty)
    {
        return Err("invalid new-graph publication identity".into());
    }
    let incarnation = graph.incarnation_id.as_deref().unwrap();
    if uuid::Uuid::parse_str(incarnation)
        .map_err(|_| "invalid graph incarnation")?
        .to_string()
        != incarnation
    {
        return Err("noncanonical graph incarnation".into());
    }
    let now_ms: u64 = graph
        .created_at
        .parse()
        .map_err(|_| "invalid graph creation clock")?;
    files::validate(plan, file_entries)?;
    let captured: BTreeMap<_, _> = plan.capture_entries.iter().map(|e| (e.index, e)).collect();
    let prepared_by_id: BTreeMap<_, _> =
        notes.iter().map(|n| (n.document_id.as_str(), n)).collect();
    for document in &plan.documents {
        let entry = captured
            .get(&document.entry)
            .ok_or("missing reviewed note inventory")?;
        let note = prepared_by_id
            .get(document.id.as_str())
            .ok_or("missing prepared note")?;
        if entry.kind != "note"
            || entry.path != note.source_path
            || entry.sha256 != note.source_sha256
            || entry.bytes != note.original.len() as u64
        {
            return Err("note differs from reviewed capture inventory".into());
        }
    }
    let source_bytes = notes
        .iter()
        .map(|n| n.original.len() as u64)
        .chain(file_entries.iter().map(|e| e.bytes))
        .try_fold(0u64, |sum, n| {
            sum.checked_add(n).ok_or("source size overflow")
        })?;
    if source_bytes > 1024 * 1024 * 1024
        || notes.len() + file_entries.len() + plan.tags.hubs.len() > 10_000
    {
        return Err("staged source exceeds capture limits".into());
    }
    if plan.links.iter().any(|l| match l.target {
        Some(_) => !matches!(l.target_kind.as_deref(), Some("document" | "artifact")),
        None => l.target_kind.is_some(),
    }) {
        return Err("invalid reviewed link target kind".into());
    }
    if notes.len() != plan.documents.len() {
        return Err("prepared note count mismatch".into());
    }
    let mut ids = BTreeSet::new();
    for id in std::iter::once(&plan.root_id)
        .chain(plan.folders.iter().map(|f| &f.id))
        .chain(plan.documents.iter().map(|d| &d.id))
        .chain(plan.tags.hubs.iter().map(|h| &h.id))
        .chain(plan.tags.folder_id.iter())
    {
        crate::ids::validate_local_id(id, "entity_id")?;
        if !ids.insert(id) {
            return Err("duplicate planned identity".into());
        }
    }
    let folders: BTreeSet<_> = std::iter::once(plan.root_id.as_str())
        .chain(plan.folders.iter().map(|f| f.id.as_str()))
        .collect();
    for parent in plan
        .folders
        .iter()
        .map(|f| f.parent_id.as_str())
        .chain(plan.documents.iter().map(|d| d.parent_id.as_str()))
    {
        if !folders.contains(parent) {
            return Err("missing planned parent".into());
        }
    }
    let by_id = prepared_by_id;
    if by_id.len() != notes.len() {
        return Err("duplicate prepared note identity".into());
    }
    let _durability = crate::cell_durability::write_guard();
    let staged = StagedGraph {
        publication: GraphPublicationReservation::acquire(
            profile,
            target.clone(),
            &graph.graph_id,
            ExistingTargetPolicy::Reject,
        )?,
        graph,
    };
    let dir = staged.publication.stage_dir();
    register(dir)?;
    crate::paths::ensure_graph_layout(dir)?;
    checkpoint(0)?;
    let workspace = yrs::Doc::new();
    let now = &staged.graph.created_at;
    {
        let mut txn = workspace.transact_mut();
        put(
            &mut txn,
            "folders",
            &plan.root_id,
            json!({"id":plan.root_id,"title":plan.folder_name,"parentId":null,"section":"documents"}),
        );
        for folder in &plan.folders {
            put(
                &mut txn,
                "folders",
                &folder.id,
                json!({"id":folder.id,"title":folder.path.rsplit('/').next().unwrap_or(&folder.path),"parentId":folder.parent_id,"section":"documents"}),
            );
        }
    }
    let store = crate::rdf_service::open_graph_store(dir)?;
    if let Some(folder) = &plan.tags.folder_id {
        put(
            &mut workspace.transact_mut(),
            "folders",
            folder,
            json!({"id":folder,"title":plan.tags.folder_title.as_deref().ok_or("tag folder title missing")?,"parentId":plan.root_id,"section":"documents"}),
        );
    }
    for (index, hub) in plan.tags.hubs.iter().enumerate() {
        checkpoint(index)?;
        let parent = plan
            .tags
            .folder_id
            .as_ref()
            .ok_or("tag hub folder missing")?;
        let content = json!({"type":"doc","content":[{"type":"heading","attrs":{"level":1},"content":[{"type":"text","text":hub.title}]}]});
        let doc = crate::crdt_engine::builder::ydoc_from_tiptap_json(&content);
        let projection = crate::crdt_engine::projection::materialize_ydoc(&doc, &hub.id);
        let update = doc
            .transact()
            .encode_state_as_update_v1(&StateVector::default());
        let record: crate::document_types::DocumentRecord = serde_json::from_value(json!({
            "documentId":hub.id,"graphId":staged.graph.graph_id,"title":hub.title,"revision":1,
            "body":projection.body,"origin":"local","providerId":"local-profile",
            "localPath":crate::storage::display_path(&target.join("documents").join(&hub.id)),
            "rdfSubject":crate::rdf::document_subject(&hub.id),"createdAt":now,"updatedAt":now,
            "createdAtSource":"import-generated",
            "capabilities":["document.local.read","document.local.write","document.local.materialize.rdf","document.local.ydoc","document.local.tiptap-tree"],
            "tiptapXml":crate::crdt_engine::projection::ydoc_to_tiptap_xml(&doc),
            "tiptapJson":projection.tiptap_json,"tree":projection.tree_json,"blocks":projection.blocks_json,
            "ydocUpdateBase64":STANDARD.encode(update),
            "ydocStatePath":crate::storage::display_path(&crate::paths::document_ydoc_state_path(&target,&hub.id))
        })).map_err(|e| e.to_string())?;
        crate::document_record_store::write_document_record(dir, &record)?;
        crate::document_meaningful_object::reconcile_document_record(&store, &record)?;
        put(
            &mut workspace.transact_mut(),
            "documents",
            &hub.id,
            json!({"id":hub.id,"title":hub.title,"parentId":parent,"createdAt":now_ms,"updatedAt":now_ms}),
        );
    }
    let mut staged_blocks = BTreeMap::new();
    for (index, document) in plan.documents.iter().enumerate() {
        let note = by_id
            .get(document.id.as_str())
            .ok_or("missing prepared note")?;
        if note.title != document.title
            || note.source_path != document.path
            || note.body_start != document.metadata.body_start
            || note.body_end != document.body_end
        {
            return Err("prepared note differs from reviewed plan".into());
        }
        note.verify()?;
        if serde_json::to_value(&note.metadata).map_err(|e| e.to_string())?
            != serde_json::to_value(&document.metadata).map_err(|e| e.to_string())?
        {
            return Err("prepared metadata differs from reviewed plan".into());
        }
        let doc = crate::crdt_engine::builder::ydoc_from_tiptap_json(&note.tiptap_json);
        let projection = crate::crdt_engine::projection::materialize_ydoc(&doc, &document.id);
        staged_blocks.insert(
            document.id.as_str(),
            projection
                .blocks_json
                .as_array()
                .ok_or("missing staged blocks")?
                .iter()
                .filter_map(|b| b["id"].as_str().map(str::to_owned))
                .collect::<BTreeSet<_>>(),
        );
        crate::vault_inline_links::validate_projection(
            &projection.tiptap_json,
            &projection.blocks_json,
            &note.inline_wires,
        )?;
        crate::vault_note::images::validate(&projection.tiptap_json, &note.images)?;
        let update = doc
            .transact()
            .encode_state_as_update_v1(&StateVector::default());
        let created = note
            .metadata
            .created_at
            .as_deref()
            .map(|raw| {
                chrono::DateTime::parse_from_rfc3339(raw).map(|d| d.timestamp_millis().to_string())
            })
            .transpose()
            .map_err(|e| e.to_string())?
            .unwrap_or_else(|| now.clone());
        let updated = note
            .metadata
            .updated_at
            .as_deref()
            .filter(|_| note.metadata.created_at.is_some())
            .map(|raw| {
                chrono::DateTime::parse_from_rfc3339(raw).map(|d| d.timestamp_millis().to_string())
            })
            .transpose()
            .map_err(|e| e.to_string())?
            .unwrap_or_else(|| created.clone());
        let created_ms: u64 = created
            .parse()
            .map_err(|_| "invalid prepared creation date")?;
        let updated_ms: u64 = updated
            .parse()
            .map_err(|_| "invalid prepared update date")?;
        if updated_ms < created_ms {
            return Err("update date precedes effective creation date; explicit provenance resolution required".into());
        }
        let record: crate::document_types::DocumentRecord = serde_json::from_value(json!({
            "documentId":document.id,"graphId":staged.graph.graph_id,"title":document.title,"revision":1,
            "body":projection.body,"origin":"local","providerId":"local-profile",
            "localPath":crate::storage::display_path(&target.join("documents").join(&document.id)),
            "rdfSubject":crate::rdf::document_subject(&document.id),"createdAt":created,"updatedAt":updated,
            "createdAtSource":if note.metadata.created_at.is_some(){"import-frontmatter"}else{"import"},
            "capabilities":["document.local.read","document.local.write","document.local.materialize.rdf","document.local.ydoc","document.local.tiptap-tree"],"tiptapXml":crate::crdt_engine::projection::ydoc_to_tiptap_xml(&doc),
            "tiptapJson":projection.tiptap_json,"tree":projection.tree_json,"blocks":projection.blocks_json,
            "ydocUpdateBase64":STANDARD.encode(update),
            "ydocStatePath":crate::storage::display_path(&crate::paths::document_ydoc_state_path(&target,&document.id))
        })).map_err(|e| e.to_string())?;
        crate::document_record_store::write_document_record(dir, &record)?;
        let original_dir = dir.join("documents").join(&document.id).join("original");
        crate::storage::create_dir_all(&original_dir)?;
        let filename =
            crate::ids::safe_filename(note.source_path.rsplit('/').next().unwrap_or("note.md"));
        let stored = crate::original_file_manifest_store::stored_original_filename(&filename);
        let original_path = original_dir.join(stored);
        crate::storage::write_bytes(&original_path, &note.original)?;
        let mut manifest = crate::original_file_manifest_store::write_original_manifest_hashed(
            &original_dir,
            &original_path,
            filename.clone(),
            "text/markdown",
            note.original.len(),
            Some(note.source_sha256.clone()),
        )?;
        manifest.local_path = crate::storage::display_path(
            &target
                .join("documents")
                .join(&document.id)
                .join("original")
                .join(stored),
        );
        crate::storage::write_json(&original_dir.join("manifest.json"), &manifest)
            .map_err(|e| e.to_string())?;
        let mut txn = workspace.transact_mut();
        put(
            &mut txn,
            "documents",
            &document.id,
            json!({"id":document.id,"title":document.title,"parentId":document.parent_id,"createdAt":created_ms,"updatedAt":updated_ms,"sf_storageKey":format!("local://documents/{}/original/{stored}",document.id),"sf_originalFilename":filename,"sf_mimeType":"text/markdown","sf_sizeBytes":note.original.len()}),
        );
        drop(txn);
        crate::document_meaningful_object::reconcile_document_record(&store, &record)?;
        checkpoint(plan.tags.hubs.len() + index + 1)?;
    }
    let artifacts = files::write(
        dir,
        &target,
        &staged.graph,
        plan,
        file_entries,
        read_file,
        &workspace,
        &mut |completed| checkpoint(plan.tags.hubs.len() + completed),
    )?;
    let by_entry: BTreeMap<_, _> = plan
        .documents
        .iter()
        .map(|d| (d.entry, d.id.as_str()))
        .collect();
    let image_links: BTreeMap<_, _> = plan
        .links
        .iter()
        .filter(|l| l.embedded)
        .map(|l| ((l.source, l.source_offset), l))
        .collect();
    let note_entries: BTreeMap<_, _> = plan
        .documents
        .iter()
        .map(|d| (d.id.as_str(), d.entry))
        .collect();
    let captured_entries: BTreeMap<_, _> =
        plan.capture_entries.iter().map(|e| (e.index, e)).collect();
    let mut checked_images = BTreeSet::new();
    for note in notes {
        let source = note_entries
            .get(note.document_id.as_str())
            .ok_or("image source note missing")?;
        for image in &note.images {
            match image.target_entry {
                Some(target) => {
                    let link = image_links
                        .get(&(*source, note.body_start + image.start))
                        .ok_or("image occurrence was not reviewed")?;
                    let entry = captured_entries.get(&target).ok_or("image entry missing")?;
                    let id = artifacts
                        .get(&target)
                        .ok_or("image artifact was not staged")?;
                    if link.target != Some(target)
                        || link.fragment.is_some()
                        || link.target_kind.as_deref() != Some("artifact")
                        || image.artifact_id.as_ref() != Some(id)
                        || image.src != crate::vault_note::images::src(&staged.graph.graph_id, id)
                        || image.mime.as_deref()
                            != Some(
                                crate::ids::local_upload_mime_type_for_filename(&entry.path)
                                    .as_str(),
                            )
                        || entry.bytes > crate::runtime_config::LOCAL_IMAGE_UPLOAD_MAX_BYTES as u64
                    {
                        return Err("image binding differs from staged artifact".into());
                    }
                    if checked_images.insert(target) {
                        use std::io::Read;
                        let filename = crate::ids::safe_filename(
                            entry
                                .path
                                .rsplit('/')
                                .next()
                                .ok_or("image filename missing")?,
                        );
                        let stored = crate::original_file_manifest_store::stored_original_filename(
                            &filename,
                        );
                        let path = dir.join("artifacts").join(id).join("original").join(stored);
                        let mut prefix = Vec::with_capacity(12);
                        std::fs::File::open(path)
                            .map_err(|e| e.to_string())?
                            .take(12)
                            .read_to_end(&mut prefix)
                            .map_err(|e| e.to_string())?;
                        if crate::vault_note::images::raster_mime(&prefix) != image.mime.as_deref()
                        {
                            return Err("staged image signature differs from reviewed type".into());
                        }
                    }
                }
                None => {
                    if image.artifact_id.is_some()
                        || image.mime.is_some()
                        || !image.src.starts_with("mycl-unresolved-image:")
                    {
                        return Err("unsupported image has display authority".into());
                    }
                }
            }
        }
    }
    let mut edges = BTreeSet::new();
    let mut paired = BTreeSet::new();
    let mut inline_ids = BTreeSet::new();
    let source_entries: BTreeMap<_, _> = plan
        .documents
        .iter()
        .map(|d| (d.id.as_str(), d.entry))
        .collect();
    let reviewed_links: BTreeMap<_, _> = plan
        .links
        .iter()
        .map(|l| ((l.source, l.source_offset), l))
        .collect();
    let reviewed_tags: BTreeMap<_, _> = plan
        .tags
        .references
        .iter()
        .filter_map(|t| t.range.map(|(start, _)| ((t.source, start), t)))
        .collect();
    let tag_targets: BTreeSet<_> = plan
        .documents
        .iter()
        .map(|d| d.id.as_str())
        .chain(plan.tags.hubs.iter().map(|h| h.id.as_str()))
        .collect();
    for note in notes {
        let source = *source_entries
            .get(note.document_id.as_str())
            .ok_or("inline source missing")?;
        for wire in &note.inline_wires {
            if wire.target_graph != staged.graph.graph_id || !inline_ids.insert(wire.id.clone()) {
                return Err("inline graph or identity mismatch".into());
            }
            if let Some(tag) = reviewed_tags.get(&(source, wire.source_offset)) {
                if wire.target_entry.is_some()
                    || wire.artifact
                    || wire.target_block_id.is_some()
                    || wire.target_id != tag.target_id
                    || wire.predicate != crate::vault_plan::tags::PREDICATE
                    || !tag_targets.contains(tag.target_id.as_str())
                {
                    return Err("inline tag binding mismatch".into());
                }
            } else {
                let link = reviewed_links
                    .get(&(source, wire.source_offset))
                    .ok_or("inline occurrence not reviewed")?;
                if link.target != wire.target_entry
                    || link.embedded
                    || wire.target_graph != staged.graph.graph_id
                    || wire.predicate != "relatedTo"
                {
                    return Err("inline wire binding mismatch".into());
                }
                let expected = if wire.artifact {
                    wire.target_entry
                        .and_then(|e| artifacts.get(&e))
                        .map(String::as_str)
                } else {
                    wire.target_entry.and_then(|e| by_entry.get(&e)).copied()
                };
                if expected != Some(wire.target_id.as_str())
                    || link.target_kind.as_deref()
                        != Some(if wire.artifact {
                            "artifact"
                        } else {
                            "document"
                        })
                {
                    return Err("inline target differs from staged target".into());
                }
                let expected_block = match &link.fragment {
                    Some(fragment) => {
                        if wire.artifact {
                            return Err("artifact fragment has no mapped block".into());
                        }
                        Some(
                            by_id
                                .get(wire.target_id.as_str())
                                .ok_or("fragment target note missing")?
                                .anchors
                                .resolve(fragment, link.wiki)
                                .ok_or("fragment target is not unique")?,
                        )
                    }
                    None => None,
                };
                if wire.target_block_id != expected_block
                    || wire.target_block_id.as_ref().is_some_and(|id| {
                        !staged_blocks
                            .get(wire.target_id.as_str())
                            .is_some_and(|ids| ids.contains(id))
                    })
                {
                    return Err("inline fragment differs from staged block".into());
                }
            }
            paired.insert((source, wire.source_offset));
            let mut value = json!({"id":wire.id,"graphId":staged.graph.graph_id,"sourceDocumentId":note.document_id,"sourceBlockId":wire.source_block_id,"predicate":wire.predicate,"createdAt":now_ms});
            if let Some(block) = &wire.target_block_id {
                value["targetBlockId"] = json!(block);
            }
            value[if wire.artifact {
                "targetArtifactId"
            } else {
                "targetDocumentId"
            }] = json!(wire.target_id);
            put(&mut workspace.transact_mut(), "wires", &wire.id, value);
        }
    }
    for (ordinal, tag) in plan.tags.references.iter().enumerate() {
        if let Some((offset, _)) = tag.range {
            if !paired.contains(&(tag.source, offset)) {
                return Err("reviewed inline tag was not paired".into());
            }
            continue;
        }
        if !tag_targets.contains(tag.target_id.as_str()) {
            return Err("missing tag target".into());
        }
        let source = by_entry.get(&tag.source).ok_or("missing tag source")?;
        let id = format!("{}-tag-wire-{ordinal}", plan.root_id);
        put(
            &mut workspace.transact_mut(),
            "wires",
            &id,
            json!({"id":id,"graphId":staged.graph.graph_id,"sourceDocumentId":source,
                "targetDocumentId":tag.target_id,"predicate":crate::vault_plan::tags::PREDICATE,"createdAt":now_ms}),
        );
    }
    for link in &plan.links {
        if paired.contains(&(link.source, link.source_offset)) {
            continue;
        }
        if let Some(target) = link.whole_target() {
            edges.insert((link.source, target, "relatedTo".to_string()));
        }
    }
    for wire in &plan.export_wires {
        if wire.original_source_block.is_some() || wire.original_target_block.is_some() {
            continue;
        }
        edges.insert((wire.source, wire.target, wire.predicate.clone()));
    }
    {
        let mut txn = workspace.transact_mut();
        for (index, (source, target, predicate)) in edges.into_iter().enumerate() {
            let id = format!("{}-wire-{index}", plan.root_id);
            let mut value = json!({"id":id,"graphId":staged.graph.graph_id,"sourceDocumentId":by_entry.get(&source).ok_or("missing wire source")?,"predicate":predicate,"createdAt":now_ms});
            if let Some(document) = by_entry.get(&target) {
                value["targetDocumentId"] = json!(document);
            } else if let Some(artifact) = artifacts.get(&target) {
                value["targetArtifactId"] = json!(artifact);
            } else {
                return Err("missing wire target".into());
            }
            put(&mut txn, "wires", &id, value);
        }
    }
    let snapshot = crate::crdt_engine::workspace_ops::materialize_workspace_snapshot_json(
        &staged.graph.graph_id,
        &workspace,
    )?;
    crate::storage::write_bytes(
        &crate::paths::workspace_ydoc_state_path(dir),
        &workspace
            .transact()
            .encode_state_as_update_v1(&StateVector::default()),
    )?;
    crate::storage::write_json(&crate::paths::workspace_snapshot_path(dir), &snapshot)
        .map_err(|e| e.to_string())?;
    crate::rdf_record_materializer::reconcile_graph_record(&store, &staged.graph)?;
    crate::rdf_service::reconcile_workspace_snapshot(&store, &staged.graph.graph_id, &snapshot)?;
    drop(store);
    crate::rdf_store_service::evict_graph_store(dir)?;
    Ok(staged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    #[test]
    #[ignore = "subprocess crash worker; invoked only with a parent-owned temporary profile"]
    fn crash_worker() {
        let Some(profile) = std::env::var_os("VAULT_STAGE_TEST_PROFILE") else {
            return;
        };
        let profile = std::path::PathBuf::from(profile);
        let after = std::env::var("VAULT_STAGE_TEST_PHASE").unwrap() == "after";
        let (graph, plan, notes) = fixture(&profile);
        let stage = assemble(&profile, graph, &plan, &notes, |count| {
            if !after && count == 1 {
                std::process::exit(17);
            }
            Ok(())
        })
        .unwrap();
        stage.publish(|| Ok(())).unwrap();
        std::process::exit(18);
    }

    #[test]
    fn process_exit_before_publication_stays_hidden_and_after_does_not_clobber() {
        for phase in ["before", "after"] {
            let root = tempfile::tempdir().unwrap();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "vault_graph_stage::tests::crash_worker",
                    "--ignored",
                ])
                .env("VAULT_STAGE_TEST_PROFILE", root.path())
                .env("VAULT_STAGE_TEST_PHASE", phase)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap();
            assert_eq!(status.code(), Some(if phase == "before" { 17 } else { 18 }));
            let target = root.path().join("graphs/new-vault");
            assert_eq!(target.exists(), phase == "after");
            let (graph, plan, notes) = fixture(root.path());
            let retry = assemble(root.path(), graph, &plan, &notes, |_| Ok(()));
            if phase == "before" {
                retry.unwrap().publish(|| Ok(())).unwrap();
            } else {
                assert!(retry.is_err());
            }
            assert!(target.join("graph.json").is_file());
        }
    }
    fn fixture(profile: &Path) -> (GraphRecord, Plan, Vec<PreparedNote>) {
        fixture_with_bodies(
            profile,
            [
                "---\ntitle: A\ncustom: preserved\n---\n# A\n\n[[B]]",
                "# B\n\nSecond note.",
            ],
        )
    }
    fn fixture_with_bodies(
        profile: &Path,
        bodies: [&str; 2],
    ) -> (GraphRecord, Plan, Vec<PreparedNote>) {
        std::fs::create_dir_all(profile.join("graphs")).unwrap();
        let graph = serde_json::from_value(json!({
            "graphId":"new-vault", "title":"Vault", "status":"active", "origin":"local", "providerId":"local-profile",
            "localPath":crate::storage::display_path(&profile.join("graphs/new-vault")),
            "createdAt":"1790000000000","updatedAt":"1790000000000","incarnationId":"3be81b55-2edf-4a9c-8090-e418eb1b66b7",
            "createdByOperationId":"test-vault-job","capabilities":[]
        })).unwrap();
        let entries: Vec<_> = bodies
            .iter()
            .enumerate()
            .map(|(index, body)| crate::vault_plan::Input {
                index,
                path: format!("Folder/{}.md", if index == 0 { "A" } else { "B" }),
                kind: "note".into(),
                bytes: body.len() as u64,
                sha256: format!("{:x}", Sha256::digest(body.as_bytes())),
            })
            .collect();
        let binding = crate::vault_capture::Binding {
            graph: "consent-context".into(),
            incarnation: "one".into(),
            principal: "human:owner".into(),
        };
        let plan = crate::vault_plan::build(&entries, &binding, "scan", "Imported", |i| {
            Ok(bodies[i].as_bytes().to_vec())
        })
        .unwrap();
        let notes = plan
            .documents
            .iter()
            .map(|d| {
                crate::vault_note::prepare(
                    d,
                    &entries[d.entry],
                    bodies[d.entry].as_bytes().to_vec(),
                )
                .unwrap()
            })
            .collect();
        (graph, plan, notes)
    }

    #[test]
    fn tag_hubs_publish_with_inline_and_frontmatter_wires_and_no_fake_originals() {
        let root = tempfile::tempdir().unwrap();
        let (graph, plan, mut notes) = fixture_with_bodies(
            root.path(),
            ["---\ntags: [topic, Onlyfront]\n---\n#topic [[B]] #B", "# B"],
        );
        let destination = crate::vault_inline_links::Destination {
            graph_id: graph.graph_id.clone(),
            incarnation: graph.incarnation_id.clone().unwrap(),
            operation_id: graph.created_by_operation_id.clone().unwrap(),
        };
        let resolver = crate::vault_inline_links::Resolver::new(&plan);
        for note in &mut notes {
            resolver.install(note, &destination).unwrap();
        }
        let target = root.path().join("graphs/new-vault");
        let staged = assemble(root.path(), graph, &plan, &notes, |_| Ok(())).unwrap();
        assert!(!target.exists());
        staged.publish(|| Ok(())).unwrap();
        let record =
            crate::workspace_record_store::read_workspace_record(&target, "new-vault").unwrap();
        let snapshot = record.snapshot.unwrap();
        assert_eq!(snapshot["documents"].as_array().unwrap().len(), 4);
        let wires = snapshot["wires"].as_array().unwrap();
        assert_eq!(wires.len(), 5);
        assert_eq!(
            wires
                .iter()
                .filter(|w| w["predicate"] == "exemplifies")
                .count(),
            4
        );
        assert_eq!(
            wires
                .iter()
                .filter(|w| w["sourceBlockId"].is_null())
                .count(),
            2
        );
        for hub in &plan.tags.hubs {
            let dir = target.join("documents").join(&hub.id);
            assert!(dir.join("document.json").exists());
            assert!(!dir.join("original").exists());
            let r = crate::document_record_store::read_document_record(
                &target,
                &dir.join("document.json"),
            )
            .unwrap();
            assert!(!r.ydoc_update_base64.is_empty());
        }
        let first = &notes[0];
        let r = crate::document_record_store::read_document_record(
            &target,
            &target
                .join("documents")
                .join(&first.document_id)
                .join("document.json"),
        )
        .unwrap();
        crate::vault_inline_links::validate_projection(
            r.tiptap_json.as_ref().unwrap(),
            &serde_json::to_value(&r.blocks).unwrap(),
            &first.inline_wires,
        )
        .unwrap();
    }

    #[test]
    fn unresolved_and_fragment_references_do_not_block_or_become_broad_wires() {
        let root = tempfile::tempdir().unwrap();
        let source = "[[Missing]] [[B#Heading]] [[B]]";
        let (graph, plan, mut notes) = fixture_with_bodies(root.path(), [source, "# Heading"]);
        assert!(plan.issues.iter().any(|i| i.kind == "unresolved_link"));
        assert!(plan.issues.iter().any(|i| i.kind == "fragment_reference"));
        let destination = crate::vault_inline_links::Destination {
            graph_id: graph.graph_id.clone(),
            incarnation: graph.incarnation_id.clone().unwrap(),
            operation_id: graph.created_by_operation_id.clone().unwrap(),
        };
        let resolver = crate::vault_inline_links::Resolver::new(&plan);
        for note in &mut notes {
            resolver.install(note, &destination).unwrap();
        }
        assert_eq!(notes[0].inline_wires.len(), 1);
        assert_eq!(notes[0].original, source.as_bytes());
        assemble(root.path(), graph, &plan, &notes, |_| Ok(()))
            .unwrap()
            .publish(|| Ok(()))
            .unwrap();
        let target = root.path().join("graphs/new-vault");
        let workspace =
            crate::workspace_record_store::read_workspace_record(&target, "new-vault").unwrap();
        assert_eq!(
            workspace.snapshot.unwrap()["wires"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let record = crate::document_record_store::read_document_record(
            &target,
            &target
                .join("documents")
                .join(&notes[0].document_id)
                .join("document.json"),
        )
        .unwrap();
        assert!(record.body.contains("[[Missing]]"));
        assert!(record.body.contains("[[B#Heading]]"));
    }

    #[test]
    fn exported_historical_block_wire_is_retained_not_widened() {
        let root = tempfile::tempdir().unwrap();
        let (graph, _, _) = fixture(root.path());
        let files = [
            ("A.md", "---\nid: \"a\"\nwires:\n  - predicate: \"qualifies\"\n    to: \"B\"\n    toId: \"b\"\n    block: \"old-source\"\n    toBlock: \"old-target\"\n---\nA"),
            ("B.md", "---\nid: \"b\"\n---\nB"),
            ("graph.json", "{\"format\":\"mycl-markdown-folder\",\"version\":1}"),
        ];
        let entries: Vec<_> = files
            .iter()
            .enumerate()
            .map(|(index, (path, body))| crate::vault_plan::Input {
                index,
                path: (*path).into(),
                kind: if index == 2 { "attachment" } else { "note" }.into(),
                bytes: body.len() as u64,
                sha256: format!("{:x}", Sha256::digest(body.as_bytes())),
            })
            .collect();
        let binding = crate::vault_capture::Binding {
            graph: "consent".into(),
            incarnation: "one".into(),
            principal: "human:owner".into(),
        };
        let plan = crate::vault_plan::build(&entries, &binding, "scan", "Imported", |i| {
            Ok(files[i].1.as_bytes().to_vec())
        })
        .unwrap();
        assert_eq!(plan.export_wires.len(), 1);
        assert!(plan
            .issues
            .iter()
            .any(|i| i.kind == "unmapped_export_block"));
        let notes: Vec<_> = plan
            .documents
            .iter()
            .map(|d| {
                crate::vault_note::prepare(
                    d,
                    &entries[d.entry],
                    files[d.entry].1.as_bytes().to_vec(),
                )
                .unwrap()
            })
            .collect();
        let stage = assemble_with_files(
            root.path(),
            graph,
            &plan,
            &notes,
            &entries[2..],
            |i| Ok(std::io::Cursor::new(files[i].1.as_bytes())),
            |_| Ok(()),
        )
        .unwrap();
        stage.publish(|| Ok(())).unwrap();
        let record = crate::workspace_record_store::read_workspace_record(
            &root.path().join("graphs/new-vault"),
            "new-vault",
        )
        .unwrap();
        assert!(record.snapshot.unwrap()["wires"]
            .as_array()
            .unwrap()
            .is_empty());
        assert_eq!(notes[0].original, files[0].1.as_bytes());
    }

    #[test]
    fn fragment_pairs_publish_with_real_targets_and_reject_a_different_existing_block() {
        fn setup(root: &Path) -> (GraphRecord, Plan, Vec<PreparedNote>) {
            let (graph, plan, mut notes) = fixture_with_bodies(
                root,
                ["[[B#Heading]] [[B#^mark]]", "# Heading\n\nBody ^mark"],
            );
            let destination = crate::vault_inline_links::Destination {
                graph_id: graph.graph_id.clone(),
                incarnation: graph.incarnation_id.clone().unwrap(),
                operation_id: graph.created_by_operation_id.clone().unwrap(),
            };
            let mut resolver = crate::vault_inline_links::Resolver::new(&plan);
            for note in &notes {
                resolver.register_note(note).unwrap();
            }
            for note in &mut notes {
                resolver.install(note, &destination).unwrap();
            }
            (graph, plan, notes)
        }
        let root = tempfile::tempdir().unwrap();
        let (graph, plan, notes) = setup(root.path());
        assert_eq!(notes[0].inline_wires.len(), 2);
        assemble(root.path(), graph, &plan, &notes, |_| Ok(()))
            .unwrap()
            .publish(|| Ok(()))
            .unwrap();
        let record = crate::workspace_record_store::read_workspace_record(
            &root.path().join("graphs/new-vault"),
            "new-vault",
        )
        .unwrap();
        let snapshot = record.snapshot.unwrap();
        let wires = snapshot["wires"].as_array().unwrap();
        assert_eq!(wires.len(), 2);
        for wire in &notes[0].inline_wires {
            assert!(wires
                .iter()
                .any(|w| w["id"] == wire.id && w["targetBlockId"] == json!(wire.target_block_id)));
        }
        let other = tempfile::tempdir().unwrap();
        let (graph, plan, mut notes) = setup(other.path());
        let wrong = notes[0].inline_wires[1].target_block_id.clone();
        let id = notes[0].inline_wires[0].id.clone();
        let mut content = notes[0].tiptap_json.clone();
        fn retarget(node: &mut serde_json::Value, id: &str, target: &Option<String>) {
            if node["type"] == "wikilink" && node["attrs"]["wireId"] == id {
                node["attrs"]["targetBlockId"] = json!(target);
            }
            if let Some(children) = node["content"].as_array_mut() {
                for child in children {
                    retarget(child, id, target);
                }
            }
        }
        retarget(&mut content, &id, &wrong);
        let mut wires = notes[0].inline_wires.clone();
        wires[0].target_block_id = wrong;
        notes[0]
            .install_inline_links(content, wires, vec![])
            .unwrap();
        assert!(assemble(other.path(), graph, &plan, &notes, |_| Ok(())).is_err());
        assert!(!other.path().join("graphs/new-vault").exists());
    }

    #[test]
    fn complete_notes_workspace_and_originals_are_hidden_until_publication() {
        let root = tempfile::tempdir().unwrap();
        let (graph, plan, notes) = fixture(root.path());
        let target = root.path().join("graphs/new-vault");
        let staged = assemble(root.path(), graph, &plan, &notes, |_| {
            assert!(!target.exists());
            Ok(())
        })
        .unwrap();
        assert!(!target.exists());
        staged.publish(|| Ok(())).unwrap();
        let workspace =
            crate::workspace_record_store::read_workspace_record(&target, "new-vault").unwrap();
        assert_eq!(
            workspace.snapshot.as_ref().unwrap()["documents"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            workspace.snapshot.as_ref().unwrap()["wires"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        for note in &notes {
            let path = target
                .join("documents")
                .join(&note.document_id)
                .join("document.json");
            let record =
                crate::document_record_store::read_document_record(&target, &path).unwrap();
            assert_eq!(record.title, note.title);
            assert!(record.local_path.starts_with(target.to_str().unwrap()));
            assert!(record.ydoc_state_path.starts_with(target.to_str().unwrap()));
            let original_dir = path.parent().unwrap().join("original");
            let original =
                crate::original_file_manifest_store::read_original_manifest(&original_dir).unwrap();
            assert!(original.local_path.starts_with(target.to_str().unwrap()));
            assert_eq!(std::fs::read(&original.local_path).unwrap(), note.original);
            assert!(!record.ydoc_update_base64.is_empty());
        }
    }

    #[test]
    fn cancellation_between_documents_leaves_no_visible_target() {
        let root = tempfile::tempdir().unwrap();
        let (graph, plan, notes) = fixture(root.path());
        assert!(
            assemble(root.path(), graph, &plan, &notes, |count| if count == 1 {
                Err("cancelled".into())
            } else {
                Ok(())
            })
            .is_err()
        );
        assert!(!root.path().join("graphs/new-vault").exists());
    }

    #[test]
    fn final_refusal_drops_only_hidden_stage() {
        let root = tempfile::tempdir().unwrap();
        let (graph, plan, notes) = fixture(root.path());
        let staged = assemble(root.path(), graph, &plan, &notes, |_| Ok(())).unwrap();
        assert!(staged
            .publish(|| Err::<(), _>("consent expired".into()))
            .is_err());
        assert!(!root.path().join("graphs/new-vault").exists());
    }

    #[test]
    fn retry_never_replaces_a_published_graph() {
        let root = tempfile::tempdir().unwrap();
        let (graph, plan, notes) = fixture(root.path());
        assemble(root.path(), graph.clone(), &plan, &notes, |_| Ok(()))
            .unwrap()
            .publish(|| Ok(()))
            .unwrap();
        let sentinel = root.path().join("graphs/new-vault/user-edit");
        std::fs::write(&sentinel, b"keep").unwrap();
        assert!(assemble(root.path(), graph, &plan, &notes, |_| Ok(())).is_err());
        assert_eq!(std::fs::read(sentinel).unwrap(), b"keep");
    }

    #[test]
    fn tampered_prepared_content_is_refused_before_publication() {
        let root = tempfile::tempdir().unwrap();
        let (graph, plan, mut notes) = fixture(root.path());
        notes[0].tiptap_json = json!({"type":"doc","content":[]});
        assert!(assemble(root.path(), graph, &plan, &notes, |_| Ok(())).is_err());
        assert!(!root.path().join("graphs/new-vault").exists());
    }

    fn attachment(plan: &mut Plan, body: &[u8]) -> crate::vault_plan::Input {
        plan.attachments.push(2);
        plan.links.push(crate::vault_plan::Link {
            source: 0,
            target: Some(2),
            target_kind: Some("artifact".into()),
            raw: "manifest.json".into(),
            fragment: None,
            embedded: false,
            wiki: true,
            source_offset: 0,
        });
        let entry = crate::vault_plan::Input {
            index: 2,
            path: "Folder/manifest.json".into(),
            kind: "attachment".into(),
            bytes: body.len() as u64,
            sha256: format!("{:x}", Sha256::digest(body)),
        };
        plan.capture_entries.push(entry.clone());
        entry
    }

    #[test]
    fn attachments_use_canonical_owned_records_and_artifact_wire_endpoints() {
        let root = tempfile::tempdir().unwrap();
        let (graph, mut plan, notes) = fixture(root.path());
        let body = b"original manifest bytes";
        let entry = attachment(&mut plan, body);
        let stage = assemble_with_files(
            root.path(),
            graph,
            &plan,
            &notes,
            &[entry],
            |_| Ok(std::io::Cursor::new(body)),
            |_| Ok(()),
        )
        .unwrap();
        let target = root.path().join("graphs/new-vault");
        assert!(!target.exists());
        stage.publish(|| Ok(())).unwrap();
        let snapshot = crate::workspace_record_store::read_workspace_record(&target, "new-vault")
            .unwrap()
            .snapshot
            .unwrap();
        let artifacts = snapshot["artifacts"].as_array().unwrap();
        assert_eq!(artifacts.len(), 1);
        let a = &artifacts[0];
        assert_eq!(a["mode"], "attached");
        assert_eq!(a["storage"], "owned");
        assert_eq!(a["source"]["relativePath"], "Folder/manifest.json");
        let id = a["id"].as_str().unwrap();
        let original_dir = target.join("artifacts").join(id).join("original");
        let manifest =
            crate::original_file_manifest_store::read_original_manifest(&original_dir).unwrap();
        assert!(manifest.local_path.ends_with("source-manifest.json"));
        assert_eq!(std::fs::read(&manifest.local_path).unwrap(), body);
        assert!(snapshot["wires"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["targetArtifactId"] == id
                && w.get("targetDocumentId").is_none_or(Value::is_null)));
    }

    #[test]
    fn corrupt_attachment_stream_never_publishes_graph() {
        let root = tempfile::tempdir().unwrap();
        let (graph, mut plan, notes) = fixture(root.path());
        let entry = attachment(&mut plan, b"expected");
        assert!(assemble_with_files(
            root.path(),
            graph,
            &plan,
            &notes,
            &[entry],
            |_| Ok(std::io::Cursor::new(b"changed!")),
            |_| Ok(())
        )
        .is_err());
        assert!(!root.path().join("graphs/new-vault").exists());
    }

    #[test]
    fn missing_attachment_inventory_is_refused_before_reader() {
        let root = tempfile::tempdir().unwrap();
        let (graph, mut plan, notes) = fixture(root.path());
        attachment(&mut plan, b"expected");
        assert!(assemble_with_files(
            root.path(),
            graph,
            &plan,
            &notes,
            &[],
            |_| -> Result<std::io::Cursor<Vec<u8>>, String> { panic!("must not read") },
            |_| Ok(())
        )
        .is_err());
        assert!(!root.path().join("graphs/new-vault").exists());
    }

    #[test]
    fn attachment_cancellation_before_copy_never_publishes_graph() {
        let root = tempfile::tempdir().unwrap();
        let (graph, mut plan, notes) = fixture(root.path());
        let body = b"expected";
        let entry = attachment(&mut plan, body);
        let opened = std::cell::Cell::new(false);
        assert!(assemble_with_files(
            root.path(),
            graph,
            &plan,
            &notes,
            &[entry],
            |_| {
                opened.set(true);
                Ok(std::io::Cursor::new(body))
            },
            |_| if opened.get() {
                Err("cancelled".into())
            } else {
                Ok(())
            }
        )
        .is_err());
        assert!(opened.get());
        assert!(!root.path().join("graphs/new-vault").exists());
    }

    #[test]
    fn invalid_utf8_note_is_published_as_raw_artifact_not_lossy_document() {
        let root = tempfile::tempdir().unwrap();
        let (graph, _, _) = fixture(root.path());
        let bytes = vec![0xff, 0xfe, 0x00, 0x80];
        let entry = crate::vault_plan::Input {
            index: 0,
            path: "bad.md".into(),
            kind: "note".into(),
            bytes: bytes.len() as u64,
            sha256: format!("{:x}", Sha256::digest(&bytes)),
        };
        let binding = crate::vault_capture::Binding {
            graph: "consent-context".into(),
            incarnation: "one".into(),
            principal: "human:owner".into(),
        };
        let plan = crate::vault_plan::build(&[entry.clone()], &binding, "scan", "Imported", |_| {
            Ok(bytes.clone())
        })
        .unwrap();
        assert!(plan.documents.is_empty());
        let stage = assemble_with_files(
            root.path(),
            graph,
            &plan,
            &[],
            &[entry],
            |_| Ok(std::io::Cursor::new(&bytes)),
            |_| Ok(()),
        )
        .unwrap();
        stage.publish(|| Ok(())).unwrap();
        let target = root.path().join("graphs/new-vault");
        let snapshot = crate::workspace_record_store::read_workspace_record(&target, "new-vault")
            .unwrap()
            .snapshot
            .unwrap();
        assert!(snapshot["documents"].as_array().unwrap().is_empty());
        let artifact = &snapshot["artifacts"].as_array().unwrap()[0];
        let original = target
            .join("artifacts")
            .join(artifact["id"].as_str().unwrap())
            .join("original/bad.md");
        assert_eq!(std::fs::read(original).unwrap(), bytes);
    }

    #[test]
    fn same_index_attachment_substitution_is_refused_before_reader() {
        let root = tempfile::tempdir().unwrap();
        let (graph, mut plan, notes) = fixture(root.path());
        let mut entry = attachment(&mut plan, b"expected");
        entry.path = "Folder/replacement.pdf".into();
        entry.sha256 = format!("{:x}", Sha256::digest(b"changed!"));
        assert!(assemble_with_files(
            root.path(),
            graph,
            &plan,
            &notes,
            &[entry],
            |_| -> Result<std::io::Cursor<Vec<u8>>, String> {
                panic!("must not open substituted source")
            },
            |_| Ok(())
        )
        .is_err());
        assert!(!root.path().join("graphs/new-vault").exists());
    }

    #[test]
    fn newly_prepared_different_note_cannot_reuse_old_plan() {
        let root = tempfile::tempdir().unwrap();
        let (graph, plan, mut notes) = fixture(root.path());
        let original = &notes[1].original;
        let mut changed = original.clone();
        let index = changed.iter().position(|b| *b == b'S').unwrap();
        changed[index] = b's';
        let mut entry = plan
            .capture_entries
            .iter()
            .find(|e| e.index == 1)
            .unwrap()
            .clone();
        entry.sha256 = format!("{:x}", Sha256::digest(&changed));
        notes[1] = crate::vault_note::prepare(&plan.documents[1], &entry, changed).unwrap();
        assert!(assemble(root.path(), graph, &plan, &notes, |_| Ok(())).is_err());
        assert!(!root.path().join("graphs/new-vault").exists());
    }
}
