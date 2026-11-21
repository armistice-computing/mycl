//! Internal captured-attachment bridge. No native picker or HTTP/MCP route.
//! Callers must derive Binding/scopes from authentication, never request JSON.
use crate::{
    app_runtime::AppHandle,
    crdt_engine::persistence_coordinator::GraphPersistenceCoordinator,
    crdt_queue::{
        admit_crdt_operation, AdmittedCrdtOperation, CrdtAdmissionError, CrdtOperation,
        EnqueueCrdtOperationInput, GRAPH_INCARNATION_PAYLOAD_KEY, GUARDED_ADMISSION_ID_PREFIX,
    },
    vault_adoption::{self, Claim},
    vault_capture::{Binding, CaptureStore, EntryPermit},
    vault_capture_access::CaptureAccess,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
#[cfg(test)]
use std::sync::Mutex;
use std::{
    fs::File,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};
#[cfg(feature = "desktop")]
use tauri::Manager;

pub(crate) const KIND: &str = "workspace.admitCapturedArtifact";

#[path = "vault_pending.rs"]
mod pending;

#[cfg(all(test, feature = "headless", not(feature = "desktop")))]
#[path = "vault_import_tests.rs"]
mod tests;

pub(crate) struct PreparedAttachment {
    _custody: pending::Custody,
    permit: EntryPermit,
    accepted: AcceptedAttachment,
    profile: PathBuf,
    possibly_admitted: AtomicBool,
    discarded: AtomicBool,
}

impl PreparedAttachment {
    pub(crate) fn id(&self) -> &str {
        &self.accepted.preparation
    }
    pub(crate) fn binding(&self) -> &Binding {
        &self.accepted.binding
    }
    pub(crate) fn scan(&self) -> &str {
        self.permit.scan()
    }
    pub(crate) fn authorize(&self, store: &CaptureStore) -> Result<(), String> {
        if self.discarded.load(Ordering::Acquire) {
            return Err("preparation retired".into());
        }
        store.authorize_prepared_attachment(&self.permit)
    }
    /// Called while holding the capture mutex, which serializes final admission.
    pub(crate) fn retire(&self) {
        self.discarded.store(true, Ordering::Release);
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AcceptedAttachment {
    preparation: String,
    binding: Binding,
    scan: String,
    index: usize,
    fingerprint: String,
    publication: Value,
}

impl AcceptedAttachment {
    pub(crate) fn claim(&self) -> Result<Claim, String> {
        if uuid::Uuid::parse_str(&self.preparation)
            .map_err(|_| "invalid preparation identity")?
            .to_string()
            != self.preparation
        {
            return Err("noncanonical preparation identity".into());
        }
        Claim::new(
            &self.binding.graph,
            &self.binding.incarnation,
            &self.scan,
            self.index,
            &self.fingerprint,
            &self.publication,
        )
    }
    pub(crate) fn artifact_id(&self) -> String {
        Claim::artifact_id(
            &self.binding.graph,
            &self.binding.incarnation,
            &self.scan,
            self.index,
        )
    }
    pub(crate) fn publication(&self) -> &Value {
        &self.publication
    }
    fn filename(&self) -> Result<&str, String> {
        let name = self.publication["originalFilename"]
            .as_str()
            .ok_or("missing original filename")?;
        if name.is_empty()
            || crate::ids::safe_filename(name) != name
            || Path::new(name).components().count() != 1
            || matches!(name, "." | "..")
        {
            return Err("invalid owned filename".into());
        }
        Ok(stored_filename(name))
    }
    fn size(&self) -> Result<u64, String> {
        self.publication["sizeBytes"]
            .as_u64()
            .ok_or("invalid size".into())
    }
    fn sha(&self) -> Result<&str, String> {
        self.publication["sha256"]
            .as_str()
            .ok_or("invalid hash".into())
    }
}

fn stored_filename(name: &str) -> &str {
    crate::original_file_manifest_store::stored_original_filename(name)
}

fn check_graph(app: &AppHandle, binding: &Binding) -> Result<PathBuf, String> {
    let (graph_dir, record) =
        crate::graph_record_store::read_graph_record_no_heal(app, &binding.graph)
            .map_err(|e| e.to_string())?;
    if record.status != "active" || record.incarnation_id.as_deref() != Some(&binding.incarnation) {
        return Err("capture graph incarnation is no longer active".into());
    }
    Ok(graph_dir)
}

fn private_directory(parent: &Path, name: &str) -> Result<PathBuf, String> {
    let path = parent.join(name);
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.to_string()),
    }
    if !std::fs::symlink_metadata(&path)
        .map_err(|e| e.to_string())?
        .is_dir()
    {
        return Err("owned import directory is not a real directory".into());
    }
    crate::storage_atomic::sync_parent_dir(parent)?;
    Ok(path)
}

fn pending_path(graph_dir: &Path, accepted: &AcceptedAttachment) -> PathBuf {
    graph_dir
        .join("import-pending")
        .join(&accepted.preparation)
        .join("body")
}

fn fresh_preparation_directory(root: &Path, preparation: &str) -> Result<(), String> {
    let path = root.join(preparation);
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    // Existing is a conflict, not permission to claim somebody else's bytes.
    builder.create(path).map_err(|e| e.to_string())?;
    crate::storage_atomic::sync_parent_dir(root)
}

/// The source capability and immutable metadata come from the capture store.
/// Hash/copy work is outside its cancellation mutex. The graph shared lease
/// pins this incarnation during file preparation and is released before admit.
/// Call at a root: never while holding a lease for this graph. Quota recovery
/// acquires its own hot lease after releasing the copy worker's shared lease.
pub(crate) async fn prepare_attachment(
    app: &AppHandle,
    store: &CaptureAccess,
    binding: &Binding,
    scan: &str,
    index: usize,
    mime_type: &str,
    parent_id: Option<String>,
) -> Result<PreparedAttachment, String> {
    if cfg!(feature = "frontend-crdt") || !cfg!(any(target_os = "macos", target_os = "linux")) {
        return Err("capture adoption is unsupported in this runtime".into());
    }
    if mime_type.is_empty() || mime_type.len() > 256 || mime_type.chars().any(char::is_control) {
        return Err("invalid MIME type".into());
    }
    let profile = std::fs::canonicalize(crate::profile_paths::profile_dir(app)?)
        .map_err(|e| e.to_string())?;
    let (permit, source) = {
        let mut guard = store.lock().map_err(|_| "capture store poisoned")?;
        guard.require_profile(&profile)?;
        guard.attachment_for_preparation(scan, index, binding)?
    };
    let coordinator = app
        .try_state::<GraphPersistenceCoordinator>()
        .ok_or("missing graph coordinator")?;
    let entry = permit.entry();
    let filename = crate::ids::safe_filename(entry.path.rsplit('/').next().unwrap_or_default());
    let artifact_id = Claim::artifact_id(&binding.graph, &binding.incarnation, scan, index);
    let publication = json!({
        "artifactId":artifact_id, "label":filename, "originalFilename":filename,
        "mimeType":mime_type, "fileType":crate::crdt_engine::workspace_ops::artifact_file_type(&filename),
        "mode":"attached", "storage":"owned", "status":"ready",
        "sizeBytes":entry.bytes, "sha256":entry.sha256, "parentId":parent_id,
        "order":permit.captured_at(), "updatedAt":permit.captured_at(),
        "index":crate::artifact_attach::plain_light_index(&filename,mime_type,usize::try_from(entry.bytes).map_err(|e|e.to_string())?),
        "storageKey":format!("local://artifacts/{artifact_id}/original/{}",stored_filename(&filename)),
        "source":{"kind":"import","importId":scan,"relativePath":entry.path},
    });
    let accepted = AcceptedAttachment {
        preparation: uuid::Uuid::new_v4().to_string(),
        binding: binding.clone(),
        scan: scan.into(),
        index,
        fingerprint: permit.fingerprint().into(),
        publication,
    };
    accepted.claim()?;
    accepted.filename()?;
    let mut retry = true;
    let custody = loop {
        let lease = coordinator.acquire_lifecycle_shared(&binding.graph).await?;
        let graph_dir = check_graph(app, binding)?;
        let bytes = accepted.clone();
        let mut source = source.try_clone().map_err(|e| e.to_string())?;
        let result = tokio::task::spawn_blocking(move || -> Result<pending::Custody, String> {
            // A dropped awaiting request cannot unpin the graph while this worker
            // is still creating files. Never retain this lease into admission.
            let _lease = lease;
            #[cfg(test)]
            pause_preparation_for_test(&bytes.scan);
            let _durability = crate::cell_durability::write_guard();
            let root = private_directory(&graph_dir, "import-pending")?;
            let custody = pending::reserve(&root, &bytes.preparation, bytes.size()?)?;
            vault_adoption::publish_owned_payload_guarded(
                &mut source,
                &pending_path(&graph_dir, &bytes),
                bytes.size()?,
                bytes.sha()?,
                pending::mutation_guard,
            )
            .map_err(|e| format!("prepare owned import bytes: {e:?}"))?;
            Ok(custody)
        })
        .await
        .map_err(|e| e.to_string())?;
        match result {
            Ok(custody) => break custody,
            Err(error) if retry && error == pending::QUOTA_ERROR => {
                retry = false;
                reconcile_preparations(app, binding)
                    .await
                    .map_err(|why| format!("{error}; reconciliation retained files: {why}"))?;
            }
            Err(error) => return Err(error),
        }
    };
    Ok(PreparedAttachment {
        _custody: custody,
        permit,
        accepted,
        profile,
        possibly_admitted: AtomicBool::new(false),
        discarded: AtomicBool::new(false),
    })
}

/// Refusal before any admission attempt releases this preparation's unique copy.
/// Prior/uncertain admissions retain custody; "refused this time" isn't enough.
/// Capture cancellation/revocation uses this same mutex, never the owned folder.
pub(crate) async fn admit_attachment(
    app: AppHandle,
    store: &CaptureAccess,
    prepared: &PreparedAttachment,
) -> Result<AdmittedCrdtOperation, CrdtAdmissionError> {
    let accepted = &prepared.accepted;
    let profile = crate::profile_paths::profile_dir(&app)?;
    if std::fs::canonicalize(&profile).map_err(|e| e.to_string())? != prepared.profile {
        return Err("prepared attachment belongs to a different owner profile".into());
    }
    let cleanup_app = app.clone();
    let input = EnqueueCrdtOperationInput {
        kind: KIND.into(),
        graph_id: accepted.binding.graph.clone(),
        document_id: Some(accepted.artifact_id()),
        payload: json!({"graphIncarnation":accepted.binding.incarnation,"capture":accepted}),
    };
    let result = admit_crdt_operation(app, input, |operation| {
        let guard = store.lock().map_err(|_| "capture store poisoned")?;
        guard.require_profile(&profile)?;
        guard.authorize_prepared_attachment(&prepared.permit)?;
        if prepared.discarded.load(Ordering::Acquire) {
            return Err("prepared attachment was discarded".into());
        }
        if operation.graph_id != prepared.permit.binding().graph
            || operation.payload[GRAPH_INCARNATION_PAYLOAD_KEY]
                != prepared.permit.binding().incarnation
        {
            return Err("capture admission graph mismatch".into());
        }
        prepared.possibly_admitted.store(true, Ordering::Release);
        Ok(guard)
    })
    .await;
    if matches!(&result, Err(CrdtAdmissionError::Refused(_))) {
        if let Err(error) = discard_prepared(&cleanup_app, prepared).await {
            log::warn!("unadmitted preparation cleanup deferred: {error}");
        }
    }
    result
}

/// Caller can explicitly abandon a returned preparation. This method never
/// claims cancellation of accepted work; false means ownership needs reconciliation.
pub(crate) async fn discard_prepared(
    app: &AppHandle,
    prepared: &PreparedAttachment,
) -> Result<bool, String> {
    if std::fs::canonicalize(crate::profile_paths::profile_dir(app)?).map_err(|e| e.to_string())?
        != prepared.profile
    {
        return Err("prepared attachment belongs to a different owner profile".into());
    }
    let coordinator = app
        .try_state::<GraphPersistenceCoordinator>()
        .ok_or("missing graph coordinator")?;
    let _lease = coordinator
        .acquire_hot_write(&prepared.accepted.binding.graph)
        .await?;
    let graph_dir = check_graph(app, &prepared.accepted.binding)?;
    prepared.discarded.store(true, Ordering::Release);
    if prepared.possibly_admitted.load(Ordering::Acquire) {
        return Ok(false);
    }
    remove_pending_body(&graph_dir, &prepared.accepted)?;
    Ok(true)
}

/// Only the internal guarded-admission API can mint this operation namespace.
/// Raw HTTP rejects this kind; legacy/native raw enqueue cannot submit its ID.
pub(crate) fn decode_operation(operation: &CrdtOperation) -> Result<AcceptedAttachment, String> {
    if operation.kind != KIND
        || !operation
            .operation_id
            .starts_with(GUARDED_ADMISSION_ID_PREFIX)
    {
        return Err("captured artifact requires internal guarded admission".into());
    }
    let accepted: AcceptedAttachment =
        serde_json::from_value(operation.payload["capture"].clone()).map_err(|e| e.to_string())?;
    if operation.graph_id != accepted.binding.graph
        || operation.payload[GRAPH_INCARNATION_PAYLOAD_KEY] != accepted.binding.incarnation
        || operation.document_id.as_deref() != Some(accepted.artifact_id().as_str())
    {
        return Err("captured artifact operation binding mismatch".into());
    }
    accepted.claim()?;
    accepted.filename()?;
    Ok(accepted)
}

/// Executor owns the hot graph lease. Call only AFTER checking the workspace
/// receipt; an already-applied retry must never recreate deleted original bytes.
pub(crate) async fn publish_artifact_bytes(
    app: &AppHandle,
    accepted: &AcceptedAttachment,
) -> Result<(), String> {
    let graph_dir = check_graph(app, &accepted.binding)?;
    let accepted = accepted.clone();
    tokio::task::spawn_blocking(move || -> Result<(), String> {
        let mut source = open_pending_regular(&pending_path(&graph_dir, &accepted))?;
        let _guard = crate::cell_durability::write_guard();
        let root = private_directory(&graph_dir, "artifacts")?;
        let artifact = private_directory(&root, &accepted.artifact_id())?;
        let original = private_directory(&artifact, "original")?;
        let target = original.join(accepted.filename()?);
        if original
            .join("manifest.json")
            .try_exists()
            .map_err(|e| e.to_string())?
        {
            let prior = crate::original_file_manifest_store::read_original_manifest(&original)?;
            if prior.filename != accepted.filename()?
                || prior.size_bytes as u64 != accepted.size()?
                || prior.sha256.as_deref() != Some(accepted.sha()?)
                || Some(prior.mime_type.as_str()) != accepted.publication["mimeType"].as_str()
            {
                return Err("owned artifact manifest conflicts with captured source".into());
            }
        }
        vault_adoption::publish_owned_payload(
            &mut source,
            &target,
            accepted.size()?,
            accepted.sha()?,
            || false,
        )
        .map_err(|e| format!("publish captured artifact bytes: {e:?}"))?;
        crate::original_file_manifest_store::write_original_manifest_hashed(
            &original,
            &target,
            accepted.filename()?.into(),
            accepted.publication["mimeType"]
                .as_str()
                .ok_or("missing MIME type")?,
            usize::try_from(accepted.size()?).map_err(|e| e.to_string())?,
            Some(accepted.sha()?.into()),
        )?;
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

fn open_pending_regular(path: &Path) -> Result<File, String> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        let file = options.open(path).map_err(|e| e.to_string())?;
        let meta = file.metadata().map_err(|e| e.to_string())?;
        if !meta.is_file() || meta.nlink() != 1 {
            return Err("pending import payload is not private regular data".into());
        }
        Ok(file)
    }
    #[cfg(not(unix))]
    {
        let _ = options;
        Err("pending import payload unsupported on this platform".into())
    }
}

#[cfg(test)]
static PREPARATION_PAUSE: Mutex<
    Option<(
        String,
        std::sync::mpsc::Sender<()>,
        std::sync::mpsc::Receiver<()>,
    )>,
> = Mutex::new(None);

#[cfg(test)]
fn pause_preparation_for_test(scan: &str) {
    let pause = {
        let mut slot = PREPARATION_PAUSE.lock().unwrap();
        if slot.as_ref().is_some_and(|p| p.0 == scan) {
            slot.take()
        } else {
            None
        }
    };
    if let Some((_, started, release)) = pause {
        started.send(()).unwrap();
        release
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
    }
}

/// Only after durable workspace receipt AND projection success. Never recursive.
pub(crate) fn release_prepared_bytes(
    app: &AppHandle,
    accepted: &AcceptedAttachment,
) -> Result<(), String> {
    let graph_dir = check_graph(app, &accepted.binding)?;
    remove_pending_body(&graph_dir, accepted)
}

fn remove_pending_body(graph_dir: &Path, accepted: &AcceptedAttachment) -> Result<(), String> {
    let path = pending_path(graph_dir, accepted);
    let _guard = crate::cell_durability::write_guard();
    let _custody = pending::mutation_guard()?;
    let parent = path.parent().ok_or("missing pending parent")?;
    match std::fs::symlink_metadata(parent) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.to_string()),
        Ok(meta) if !meta.is_dir() => {
            return Err("pending directory is not a real directory".into())
        }
        Ok(_) => {}
    }
    let root = parent.parent().ok_or("missing pending root")?;
    if !std::fs::symlink_metadata(root)
        .map_err(|e| e.to_string())?
        .is_dir()
    {
        return Err("pending root is not a real directory".into());
    }
    crate::storage_file_ops::remove_file_if_exists(&path)?;
    pending::remove_reservation(parent)?;
    std::fs::remove_dir(parent).map_err(|e| e.to_string())?;
    crate::storage_atomic::sync_parent_dir(root)
}

/// Internal maintenance entry, not a route. Pending operations retain custody
/// regardless of their age; missing/corrupt WAL evidence never permits deletion.
pub(crate) async fn reconcile_preparations(
    app: &AppHandle,
    binding: &Binding,
) -> Result<usize, String> {
    let graph_dir = check_graph(app, binding)?;
    if !pending::has_unheld(&graph_dir.join("import-pending"))? {
        return Ok(0);
    }
    let coordinator = app
        .try_state::<GraphPersistenceCoordinator>()
        .ok_or("missing graph coordinator")?;
    let lease = coordinator.acquire_hot_write(&binding.graph).await?;
    // This maintenance path reads the journal and only removes pending files;
    // it never mutates the workspace or RDF projections.
    lease.declare_rdf_read_only();
    let graph_dir = check_graph(app, binding)?;
    let app = app.clone();
    let binding = binding.clone();
    tokio::task::spawn_blocking(move || {
        // Own the lease in the worker even if its caller stops awaiting.
        let _lease = lease;
        let _durability = crate::cell_durability::write_guard();
        match std::fs::symlink_metadata(graph_dir.join("import-pending")) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e.to_string()),
            Ok(_) => {}
        }
        let operations = crate::crdt_operation_journal::pending_crdt_operations_for_cleanup(&app)?;
        let mut owners = std::collections::HashSet::new();
        for operation in operations {
            if operation.graph_id != binding.graph || operation.kind != KIND {
                continue;
            }
            let accepted = decode_operation(&operation)?;
            // Retain even an old-incarnation claim: maintenance is not the
            // authority which decides whether a recovered operation will run.
            owners.insert(accepted.preparation);
        }
        pending::reap(&graph_dir.join("import-pending"), &owners)
    })
    .await
    .map_err(|e| e.to_string())?
}
