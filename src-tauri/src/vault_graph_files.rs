//! Attach captured byte streams as owned files in an unpublished graph.
use super::*;
use crate::vault_plan::Input;
use sha2::{Digest, Sha256};

pub(crate) fn artifact_id(
    graph: &str,
    incarnation: &str,
    operation: &str,
    entry: &Input,
) -> Result<String, String> {
    Ok(format!(
        "artifact-{:x}",
        Sha256::digest(
            serde_json::to_vec(&(
                graph,
                incarnation,
                operation,
                entry.index,
                &entry.path,
                &entry.sha256
            ))
            .map_err(|e| e.to_string())?
        )
    ))
}

pub(super) fn validate(plan: &Plan, entries: &[Input]) -> Result<(), String> {
    let captured: BTreeMap<_, _> = plan.capture_entries.iter().map(|e| (e.index, e)).collect();
    if captured.len() != plan.capture_entries.len()
        || entries
            .iter()
            .any(|e| captured.get(&e.index).copied() != Some(e))
    {
        return Err("attachment differs from reviewed capture inventory".into());
    }
    let expected: BTreeSet<_> = plan.attachments.iter().copied().collect();
    let supplied: BTreeSet<_> = entries.iter().map(|e| e.index).collect();
    if expected.len() != plan.attachments.len()
        || supplied.len() != entries.len()
        || expected != supplied
    {
        return Err("captured attachment inventory does not match plan".into());
    }
    if plan.documents.iter().any(|d| expected.contains(&d.entry)) {
        return Err("entry is both a document and an artifact".into());
    }
    let mut admission =
        crate::vault_preflight::Admission::new(crate::vault_preflight::Limits::default());
    for doc in &plan.documents {
        admission.admit(&doc.path, false)?;
    }
    for entry in entries {
        let (_, kind) = admission.admit(&entry.path, false)?;
        let expected_kind = if kind == crate::vault_preflight::Kind::Note {
            "note"
        } else {
            "attachment"
        };
        if entry.kind != expected_kind
            || entry.bytes > 256 * 1024 * 1024
            || entry.sha256.len() != 64
            || !entry
                .sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("invalid captured attachment metadata".into());
        }
        let mut used = 0;
        admission.account(
            kind,
            &mut used,
            usize::try_from(entry.bytes).map_err(|_| "attachment size overflow")?,
        )?;
    }
    Ok(())
}

pub(super) fn write<R: std::io::Read + std::io::Seek>(
    stage: &Path,
    target: &Path,
    graph: &GraphRecord,
    plan: &Plan,
    entries: &[Input],
    mut read_file: impl FnMut(usize) -> Result<R, String>,
    workspace: &yrs::Doc,
    checkpoint: &mut impl FnMut(usize) -> Result<(), String>,
) -> Result<BTreeMap<usize, String>, String> {
    let folders: BTreeMap<_, _> = plan
        .folders
        .iter()
        .map(|f| (f.path.as_str(), f.id.as_str()))
        .collect();
    let mut ids = BTreeMap::new();
    let now: u64 = graph
        .created_at
        .parse()
        .map_err(|_| "invalid graph creation clock")?;
    for (ordinal, entry) in entries.iter().enumerate() {
        let completed = plan.documents.len() + ordinal;
        checkpoint(completed)?;
        let parent_path = entry.path.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
        let parent = if parent_path.is_empty() {
            plan.root_id.as_str()
        } else {
            *folders
                .get(parent_path)
                .ok_or("missing attachment parent")?
        };
        let id = artifact_id(
            &graph.graph_id,
            graph
                .incarnation_id
                .as_deref()
                .ok_or("missing incarnation")?,
            graph
                .created_by_operation_id
                .as_deref()
                .ok_or("missing operation")?,
            entry,
        )?;
        let filename = crate::ids::safe_filename(
            entry
                .path
                .rsplit('/')
                .next()
                .ok_or("missing attachment filename")?,
        );
        let stored = crate::original_file_manifest_store::stored_original_filename(&filename);
        let mime = crate::ids::local_upload_mime_type_for_filename(&filename);
        let original_dir = stage.join("artifacts").join(&id).join("original");
        crate::storage::create_dir_all(&original_dir)?;
        let body = original_dir.join(stored);
        let mut source = read_file(entry.index)?;
        let cancellation = std::cell::RefCell::new(&mut *checkpoint);
        crate::vault_adoption::publish_owned_payload(
            &mut source,
            &body,
            entry.bytes,
            &entry.sha256,
            || cancellation.borrow_mut()(completed).is_err(),
        )
        .map_err(|e| format!("stage attachment {}: {e:?}", entry.path))?;
        let mut manifest = crate::original_file_manifest_store::write_original_manifest_hashed(
            &original_dir,
            &body,
            filename.clone(),
            &mime,
            entry.bytes as usize,
            Some(entry.sha256.clone()),
        )?;
        manifest.local_path = crate::storage::display_path(
            &target
                .join("artifacts")
                .join(&id)
                .join("original")
                .join(stored),
        );
        crate::storage::write_json(&original_dir.join("manifest.json"), &manifest)
            .map_err(|e| e.to_string())?;
        let value = json!({
            "label":filename,"originalFilename":filename,"mimeType":mime,"fileType":crate::crdt_engine::workspace_ops::artifact_file_type(&filename),
            "mode":"attached","storage":"owned","status":"ready","sizeBytes":entry.bytes,"sha256":entry.sha256,
            "parentId":parent,"order":entry.index,"updatedAt":now,
            "storageKey":format!("local://artifacts/{id}/original/{stored}"),
            "source":{"kind":"import","importId":graph.created_by_operation_id,"relativePath":entry.path},
            "index":crate::artifact_attach::plain_light_index(&filename,&mime,entry.bytes as usize),
        });
        crate::crdt_engine::workspace_ops::put_workspace_artifact(
            &mut workspace.transact_mut(),
            &graph.graph_id,
            &id,
            value.as_object().unwrap(),
        )?;
        ids.insert(entry.index, id);
        checkpoint(completed + 1)?;
    }
    Ok(ids)
}
