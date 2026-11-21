use crate::app_runtime::AppHandle;
use crate::{
    app_error::{AppError, AppResult},
    clock,
    document_history_file_store::read_document_snapshot_payload,
    document_record_store::read_document_record_cold,
    document_types::DocumentRecord,
    graph_paths::existing_graph_dir,
    graph_record_store::{next_content_revision, touch_graph_content_revision, GraphRecord},
    local_jobs::{LocalJobProgress, LocalJobRegistry},
    paths::existing_document_dir,
    rdf_seed_service::ensure_graph_store_seeded,
    rdf_service::{materialize_document_record, open_graph_store},
    restore_guard::{RestoreGuardDetail, RestoreGuardState},
    storage::{read_json, write_json},
    storage_file_ops::write_bytes,
    time_travel_service::{capture_restore_point, capture_restore_point_with_lease},
    time_travel_store::{read_document_bytes, read_manifest},
    time_travel_types::{
        DocumentSnapshotRef, RestorePointManifest, RestorePointTrigger,
        RESTORE_POINT_MIN_RESTORABLE_SCHEMA_VERSION,
    },
    ydoc_paths::{document_ydoc_state_path, workspace_snapshot_path, workspace_ydoc_state_path},
};
use serde_json::{json, Value};
use std::{path::Path, sync::Arc};
#[cfg(feature = "desktop")]
use tauri::{Emitter, Manager};

const RESTORE_JOB_TYPE: &str = "graph.restore";
const RESTORE_EVENT: &str = "time-travel.graph-restored";

const PHASE_LOCKING: &str = "locking";
const PHASE_BACKING_UP: &str = "backing_up";
const PHASE_RESTORING: &str = "restoring";
const PHASE_REBUILDING: &str = "rebuilding";
const PHASE_VERIFYING: &str = "verifying";
const PHASE_SUCCEEDED: &str = "succeeded";
const PHASE_ROLLED_BACK: &str = "rolled_back";
const PHASE_FAILED: &str = "failed";

pub(crate) fn start_restore(
    app: &AppHandle,
    jobs: Arc<LocalJobRegistry>,
    graph_id: &str,
    restore_point_id: &str,
    dry_run: bool,
) -> AppResult<Value> {
    let graph_dir = existing_graph_dir(app, graph_id).map_err(AppError::not_found)?;
    let manifest = read_manifest(&graph_dir, restore_point_id)?;
    if manifest.schema_version < RESTORE_POINT_MIN_RESTORABLE_SCHEMA_VERSION {
        return Err(AppError::validation(format!(
            "restore point {} uses schema v{} which lacks Y.Doc state bytes; capture a fresh restore point (v{}+) to enable restore",
            restore_point_id,
            manifest.schema_version,
            RESTORE_POINT_MIN_RESTORABLE_SCHEMA_VERSION
        )));
    }
    let docs_without_bytes: Vec<&str> = manifest
        .documents
        .iter()
        .filter(|d| d.ydoc_bytes_path.is_none())
        .map(|d| d.document_id.as_str())
        .collect();
    if !docs_without_bytes.is_empty() {
        return Err(AppError::validation(format!(
            "restore point {} is missing Y.Doc bytes for documents: {}",
            restore_point_id,
            docs_without_bytes.join(", ")
        )));
    }

    let plan = json!({
        "restorePointId": manifest.restore_point_id,
        "graphId": manifest.graph_id,
        "documentCount": manifest.documents.len(),
        "workspaceSizeBytes": manifest.workspace.size_bytes,
        "totalDocumentSizeBytes": manifest.metadata.total_document_size_bytes,
    });

    if dry_run {
        return Ok(json!({
            "operationId": null,
            "state": "dry_run",
            "dryRun": true,
            "plan": plan,
        }));
    }

    let detail = json!({
        "graphId": graph_id,
        "graph_id": graph_id,
        "restorePointId": restore_point_id,
        "restore_point_id": restore_point_id,
        "plan": plan,
    });
    let job = jobs
        .insert_queued(RESTORE_JOB_TYPE, Some(graph_id.to_string()), detail.clone())
        .map_err(AppError::internal)?;
    let operation_id = job.job_id.clone();

    let app_clone = app.clone();
    let jobs_clone = jobs.clone();
    let graph_id_owned = graph_id.to_string();
    let rp_owned = restore_point_id.to_string();
    let op_id_for_task = operation_id.clone();
    crate::app_runtime::async_runtime::spawn(async move {
        if let Err(error) = run_restore(
            &app_clone,
            jobs_clone,
            &graph_id_owned,
            &rp_owned,
            &op_id_for_task,
        )
        .await
        {
            log::error!("restore operation {op_id_for_task} failed unexpectedly: {error}");
        }
    });

    let initial_phase = job
        .progress
        .as_ref()
        .map(|p| p.phase.clone())
        .unwrap_or_else(|| "pending".to_string());
    Ok(json!({
        "operationId": operation_id,
        "state": job_status_phase(&job.status, &initial_phase),
        "phase": initial_phase,
        "dryRun": false,
        "plan": plan,
    }))
}

pub(crate) fn get_restore_operation(
    jobs: &LocalJobRegistry,
    operation_id: &str,
) -> AppResult<Value> {
    let record = jobs
        .get(operation_id)
        .map_err(AppError::internal)?
        .ok_or_else(|| {
            AppError::not_found(format!("restore operation {operation_id} not found"))
        })?;
    Ok(restore_operation_json(&record))
}

pub(crate) fn cancel_restore_operation(
    jobs: &LocalJobRegistry,
    operation_id: &str,
) -> AppResult<Value> {
    let response = jobs.cancel(operation_id).map_err(AppError::internal)?;
    let cancelled = response.as_ref().map(|r| r.cancelled).unwrap_or(false);
    let message = response
        .as_ref()
        .map(|r| r.message.clone())
        .unwrap_or_else(|| format!("operation {operation_id} not found"));
    Ok(json!({
        "operationId": operation_id,
        "cancelled": cancelled,
        "rollbackConfirmed": false,
        "message": if cancelled { "Cancellation requested; rollback is not confirmed." } else { &message },
    }))
}

async fn run_restore(
    app: &AppHandle,
    jobs: Arc<LocalJobRegistry>,
    graph_id: &str,
    restore_point_id: &str,
    operation_id: &str,
) -> AppResult<()> {
    let _ = jobs
        .mark_running(operation_id)
        .map_err(AppError::internal)?;
    update_phase(
        &jobs,
        operation_id,
        graph_id,
        PHASE_LOCKING,
        "Acquiring workspace mutation lock",
        10,
    )?;

    // Fence new CRDT enqueues BEFORE waiting for the graph lease. Otherwise a
    // mutation can enter the queue in the acquire window, wait behind the
    // restore, and overwrite freshly restored state immediately afterward.
    // The lease owns an Arc, so it can move into the blocking worker and
    // remains armed even if the outer async join handle is dropped.
    let guard_state = app.state::<Arc<RestoreGuardState>>();
    let restore_guard = match guard_state.inner().clone().try_engage(RestoreGuardDetail {
        graph_id: graph_id.to_string(),
        operation_id: operation_id.to_string(),
        restore_point_id: restore_point_id.to_string(),
    }) {
        Ok(lease) => lease,
        Err(error) => {
            finish_failed(&jobs, operation_id, PHASE_FAILED, &error, None);
            return Err(AppError::conflict(error));
        }
    };

    // Restore rewrites the same graph files and Oxigraph projections queried
    // by external SPARQL, and graph deletion/recreation uses this coordinator
    // as its process-local incarnation fence. Acquire the SAME lease before
    // backup/apply/rebuild. Async acquire keeps a single-worker runtime
    // responsive while another graph operation owns the lease.
    let graph_lease = match app
        .try_state::<crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator>(
    ) {
        Some(coordinator) => match coordinator.acquire_hot_write(graph_id).await {
            Ok(lease) => Some(lease),
            Err(error) => {
                finish_failed(&jobs, operation_id, PHASE_FAILED, &error, None);
                return Err(AppError::storage(error));
            }
        },
        None => None,
    };

    let blocking_app = app.clone();
    let blocking_jobs = jobs.clone();
    let blocking_graph_id = graph_id.to_string();
    let blocking_restore_point_id = restore_point_id.to_string();
    let blocking_operation_id = operation_id.to_string();
    crate::app_runtime::async_runtime::spawn_blocking(move || {
        let _restore_guard = restore_guard;
        let _graph_lease = graph_lease;
        run_restore_under_graph_lease(
            &blocking_app,
            blocking_jobs,
            &blocking_graph_id,
            &blocking_restore_point_id,
            &blocking_operation_id,
        )
    })
    .await
    .map_err(|error| AppError::internal(format!("restore blocking worker failed: {error}")))?
}

fn run_restore_under_graph_lease(
    app: &AppHandle,
    jobs: Arc<LocalJobRegistry>,
    graph_id: &str,
    restore_point_id: &str,
    operation_id: &str,
) -> AppResult<()> {
    struct EvictRooms<'a>(&'a AppHandle, &'a str, &'a std::cell::Cell<bool>);
    impl Drop for EvictRooms<'_> {
        fn drop(&mut self) {
            if !self.2.get() { return; }
            if let Some(registry) = self.0.try_state::<crate::crdt_engine::rooms::RoomRegistry>() {
                registry.evict_graph(self.1);
            }
        }
    }
    let mutation_started = std::cell::Cell::new(false);
    let invalidate_rooms = EvictRooms(app, graph_id, &mutation_started);
    let outcome = run_phases(app, &jobs, graph_id, restore_point_id, operation_id, &mutation_started);

    // Restore (including rollback) replaces authority files. No cached Doc may
    // later write its pre-restore state over them. The caller still owns the
    // graph lease here; close extant rooms before releasing it or announcing success.
    // Drop also runs if a restore worker unwinds after replacing files.
    drop(invalidate_rooms);

    match outcome {
        Ok(success) => {
            finish_restore_job(&jobs, operation_id, &success)?;
            let _ = app.emit(RESTORE_EVENT, &success);
            Ok(())
        }
        Err(error) => {
            log::warn!(
                "restore operation {operation_id} failed: {message}",
                message = error.message_ref()
            );
            Err(error)
        }
    }
}

fn finish_restore_job(jobs: &LocalJobRegistry, operation_id: &str, success: &Value) -> AppResult<()> {
    let record = jobs.finish_existing(operation_id, Ok(success.clone()), "application/json")?
        .ok_or_else(|| AppError::internal("restore job disappeared before completion"))?;
    if !matches!(record.status, crate::local_jobs::LocalJobStatus::Succeeded) {
        return Err(AppError::conflict("restore job did not durably complete successfully"));
    }
    Ok(())
}

fn run_phases(
    app: &AppHandle,
    jobs: &Arc<LocalJobRegistry>,
    graph_id: &str,
    restore_point_id: &str,
    operation_id: &str,
    mutation_started: &std::cell::Cell<bool>,
) -> AppResult<Value> {
    let graph_dir = existing_graph_dir(app, graph_id).map_err(|error| {
        finish_failed(jobs, operation_id, PHASE_FAILED, &error, None);
        AppError::not_found(error)
    })?;

    update_phase(
        jobs,
        operation_id,
        graph_id,
        PHASE_BACKING_UP,
        "Capturing pre-restore backup",
        25,
    )?;
    let backup_summary = capture_restore_point_with_lease(
        app,
        graph_id,
        RestorePointTrigger::Checkpoint,
        Some(format!("auto-backup-before-restore/{restore_point_id}")),
    )
    .map_err(|error| {
        let message = format!("backup failed: {message}", message = error.message_ref());
        finish_failed(jobs, operation_id, PHASE_FAILED, &message, None);
        AppError::storage(message)
    })?;
    let backup_pointer_id = backup_summary
        .get("restorePointId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if backup_pointer_id.is_empty() {
        let error = "backup capture returned no restore point id".to_string();
        finish_failed(jobs, operation_id, PHASE_FAILED, &error, None);
        return Err(AppError::internal(error));
    }
    if check_cancelled(jobs, operation_id, &graph_dir, &backup_pointer_id)? {
        let error = "restore cancelled before apply".to_string();
        finish_rolled_back(jobs, operation_id, &error, &backup_pointer_id);
        return Err(AppError::conflict(error));
    }

    update_phase(
        jobs,
        operation_id,
        graph_id,
        PHASE_RESTORING,
        "Applying restore point bundle",
        50,
    )?;
    let manifest = read_manifest(&graph_dir, restore_point_id).map_err(|error| {
        finish_failed(
            jobs,
            operation_id,
            PHASE_FAILED,
            error.message_ref(),
            Some(&backup_pointer_id),
        );
        error
    })?;
    // Prepare preservation before ordinary writes; a preparation failure needs no rollback.
    let prepared =
        prepare_restore(&graph_dir, &manifest).map_err(|error| {
            finish_failed(
                jobs,
                operation_id,
                PHASE_FAILED,
                error.message_ref(),
                Some(&backup_pointer_id),
            );
            error
        })?;
    let derived_views_preserved: Vec<_> = prepared.derived_ids.iter().cloned().collect();
    let documents_restored = manifest
        .documents
        .iter()
        .filter(|doc| !prepared.derived_ids.contains(&doc.document_id))
        .count();
    mutation_started.set(true);
    if let Err(apply_error) = apply_prepared_restore(&graph_dir, &manifest, prepared) {
        if let Err(rollback_error) = rollback_to_backup(&graph_dir, &backup_pointer_id) {
            log::error!(
                "restore operation {operation_id} ROLLBACK failed after apply error: {message}",
                message = rollback_error.message_ref()
            );
        }
        finish_rolled_back(
            jobs,
            operation_id,
            apply_error.message_ref(),
            &backup_pointer_id,
        );
        return Err(apply_error);
    }
    if check_cancelled(jobs, operation_id, &graph_dir, &backup_pointer_id)? {
        let error = "restore cancelled after apply".to_string();
        finish_rolled_back(jobs, operation_id, &error, &backup_pointer_id);
        return Err(AppError::conflict(error));
    }

    update_phase(
        jobs,
        operation_id,
        graph_id,
        PHASE_REBUILDING,
        "Rematerializing RDF projections",
        75,
    )?;
    if let Err(error) = rebuild_projections(&graph_dir) {
        if let Err(rollback_error) = rollback_to_backup(&graph_dir, &backup_pointer_id) {
            log::error!(
                "restore operation {operation_id} ROLLBACK failed after rebuild error: {message}",
                message = rollback_error.message_ref()
            );
        }
        finish_rolled_back(jobs, operation_id, error.message_ref(), &backup_pointer_id);
        return Err(error);
    }

    update_phase(
        jobs,
        operation_id,
        graph_id,
        PHASE_VERIFYING,
        "Verifying restored state",
        90,
    )?;
    if let Err(error) = verify_restored(&graph_dir, &manifest) {
        if let Err(rollback_error) = rollback_to_backup(&graph_dir, &backup_pointer_id) {
            log::error!(
                "restore operation {operation_id} ROLLBACK failed after verify error: {message}",
                message = rollback_error.message_ref()
            );
        }
        finish_rolled_back(jobs, operation_id, error.message_ref(), &backup_pointer_id);
        return Err(error);
    }

    Ok(json!({
        "operationId": operation_id,
        "graphId": graph_id,
        "restorePointId": restore_point_id,
        "backupPointerId": backup_pointer_id,
        "documentsRestored": documents_restored,
        "derivedViewsPreserved": derived_views_preserved,
        "completedAt": clock::timestamp(),
        "state": PHASE_SUCCEEDED,
    }))
}

/// Outcome of restoring a single document's metadata. The two `SkippedNoMetadata`
/// producers in `restore_document_metadata` are the ONLY tolerated per-document
/// conditions — a document the manifest references that legitimately has no
/// metadata to restore now (missing dir / missing `document.json`), whose Y.Doc
/// bytes copy still recreates its state. Every OTHER failure is a real `Err`
/// that must fail the whole restore.
enum DocumentMetadataOutcome {
    Restored,
    SkippedNoMetadata,
}

fn apply_restore(graph_dir: &Path, manifest: &RestorePointManifest) -> AppResult<()> {
    let prepared = prepare_restore(graph_dir, manifest)?;
    apply_prepared_restore(graph_dir, manifest, prepared)
}

fn prepare_restore(graph_dir: &Path, manifest: &RestorePointManifest) -> AppResult<crate::geist_derived_restore::PreservedRestore> {
    let mut prepared = crate::geist_derived_restore::prepare(graph_dir, manifest)?;
    let current = std::fs::read(workspace_ydoc_state_path(graph_dir))
        .map_err(|e| AppError::storage(format!("read current workspace authority for import receipt history: {e}")))?;
    prepared.workspace_bytes = crate::workspace_receipt_guard::preserve_for_restore(&current, &prepared.workspace_bytes)
        .map_err(AppError::conflict)?;
    Ok(prepared)
}

fn apply_prepared_restore(
    graph_dir: &Path,
    manifest: &RestorePointManifest,
    prepared: crate::geist_derived_restore::PreservedRestore,
) -> AppResult<()> {
    // Collect per-document reconcile failures and FAIL the restore if any
    // occurred. Since round 3 `restore_document_metadata` also self-reconciles
    // each document's RDF (open_graph_store + materialize_document_record), so
    // swallowing its error (the old `if let Err(..) { warn }` blanket-catch)
    // would let a MIXED/PARTIAL restore — some documents' metadata + RDF never
    // reconciled — report SUCCESS, because verification only checks Y.Doc
    // sidecar existence. The one tolerated condition (SkippedNoMetadata) is an
    // explicit match arm, preserving exactly the pre-round-3 tolerance.
    let mut failures: Vec<String> = Vec::new();
    for doc_ref in &manifest.documents {
        if prepared.derived_ids.contains(&doc_ref.document_id) {
            continue;
        }
        // Restore document.json metadata (title, blocks, tiptap_xml) so the
        // record reflects the snapshot's content immediately, not after the
        // next user save. The blocks.json sidecar is left for the first save
        // to regenerate — Y.Doc state is the canonical source.
        match restore_document_metadata(graph_dir, doc_ref) {
            Ok(DocumentMetadataOutcome::Restored)
            | Ok(DocumentMetadataOutcome::SkippedNoMetadata) => {}
            Err(error) => {
                log::error!(
                    "metadata restore for document {} FAILED: {message}",
                    doc_ref.document_id,
                    message = error.message_ref()
                );
                failures.push(format!("{}: {}", doc_ref.document_id, error.message_ref()));
            }
        }

        let bytes =
            read_document_bytes(graph_dir, &manifest.restore_point_id, &doc_ref.document_id)?;
        let target = document_ydoc_state_path(graph_dir, &doc_ref.document_id);
        write_bytes(&target, &bytes).map_err(AppError::storage)?;
    }
    if !failures.is_empty() {
        return Err(AppError::internal(format!(
            "restore incomplete: {count} document(s) failed to reconcile: {detail}",
            count = failures.len(),
            detail = failures.join("; ")
        )));
    }
    write_bytes(
        &workspace_ydoc_state_path(graph_dir),
        &prepared.workspace_bytes,
    )
    .map_err(AppError::storage)?;
    if !prepared.workspace_snapshot.is_null() {
        write_json(
            &workspace_snapshot_path(graph_dir),
            &prepared.workspace_snapshot,
        )
        .map_err(AppError::storage)?;
    }
    Ok(())
}

fn restore_document_metadata(
    graph_dir: &Path,
    doc_ref: &DocumentSnapshotRef,
) -> AppResult<DocumentMetadataOutcome> {
    crate::geist_derived_view::require_editable(graph_dir, &doc_ref.document_id)
        .map_err(AppError::conflict)?;
    let document_dir = match existing_document_dir(graph_dir, &doc_ref.document_id) {
        Ok(dir) => dir,
        Err(error) => {
            // Document referenced by the manifest no longer has a directory
            // — either deleted post-capture or never existed. Skip metadata
            // update; the Y.Doc bytes copy will recreate the dir + state.
            // TOLERATED (pre-round-3) case, kept as an explicit outcome.
            log::warn!(
                "metadata restore for document {} skipped: document dir missing ({error})",
                doc_ref.document_id
            );
            return Ok(DocumentMetadataOutcome::SkippedNoMetadata);
        }
    };
    let manifest_path = document_dir.join("document.json");
    let mut record: DocumentRecord = if manifest_path.is_file() {
        // Cold read: this function clears `ydoc_update_base64` before writing
        // the record back anyway (the restored Y.Doc bytes are swapped in as
        // sidecar files), so hydrating the pre-restore history here was pure
        // O(history) memory per document across the whole restore sweep.
        read_document_record_cold(graph_dir, &manifest_path).map_err(AppError::storage)?
    } else {
        // Bare-bones synthesis is out of scope here (most fields would be
        // unknown). Skip metadata restore in this rare case.
        // TOLERATED (pre-round-3) case, kept as an explicit outcome.
        log::warn!(
            "metadata restore for document {} skipped: document.json missing — cannot restore metadata",
            doc_ref.document_id
        );
        return Ok(DocumentMetadataOutcome::SkippedNoMetadata);
    };

    let payload =
        read_document_snapshot_payload(graph_dir, &doc_ref.document_id, &doc_ref.snapshot_id)
            .map_err(AppError::storage)?;
    record.title = payload.title;
    record.tiptap_xml = payload.tiptap_xml;
    record.blocks = payload.blocks;
    // tree / tiptap_json are derived; clear so frontend rebuilds them on save.
    record.tree = None;
    record.tiptap_json = None;
    record.ydoc_update_base64 = String::new();
    record.revision = record.revision.saturating_add(1);
    // MONOTONIC stamp — never raw wall clock. `content_revision` advances as
    // max(now, prev+1) (`next_content_revision`), so it can run AHEAD of the
    // wall clock, and the RDF seed's incremental walk re-materializes only
    // documents stamped at/after the previous marker's revision — a
    // wall-clock stamp below the floor would make the seed skip the restored
    // document. Deriving the stamp from the graph's CURRENT
    // `content_revision` with the writers' own monotonic discipline keeps
    // the stamp at/above every marker floor recorded so far.
    let graph: GraphRecord = read_json(&graph_dir.join("graph.json"))?;
    record.updated_at = next_content_revision(graph.content_revision.as_deref())?;

    write_json(&manifest_path, &record).map_err(AppError::storage)?;

    // SELF-RECONCILE — the correctness anchor; the monotonic stamp above is
    // defense-in-depth. This read-stamp-write holds no persistence lease, and
    // the restore guard blocks only NEW enqueues (`enqueue_crdt_operation`
    // checks it) — an already-queued or journal-recovered flush can still
    // drain concurrently, advance `content_revision` (its
    // `persist_materialized_workspace` never checks the guard), and let an
    // unguarded SPARQL-triggered seed pass record a marker floor ABOVE this
    // record's stamp between our graph.json read and rebuild's seed pass —
    // reopening the stale-RDF-forever hole through a race window. Instead of
    // re-serializing that ordering, make restore satisfy the same invariant
    // as every other manifest writer (`ensure_document_persistence_tail`,
    // the title sweep, save_document): reconcile THIS document's RDF
    // directly at write time. Once the projection is reconciled here, any
    // seed pass that skips this document skips an already-correct
    // projection — the marker/floor ordering becomes irrelevant to
    // correctness. O(restored documents), an explicit bounded operation.
    let store = open_graph_store(graph_dir).map_err(AppError::rdf)?;
    materialize_document_record(&store, &record).map_err(AppError::rdf)?;
    Ok(DocumentMetadataOutcome::Restored)
}

fn rebuild_projections(graph_dir: &Path) -> AppResult<()> {
    // Restore REWRITES document/workspace files, so it is a CONTENT change: bump
    // the content-revision seed key (not just updated_at), else the content-keyed
    // seed marker is unchanged and `ensure_graph_store_seeded` skips — leaving the
    // RDF projections at pre-restore content (the dual of the reseed-storm fix: a
    // real content change MUST invalidate the seed).
    touch_graph_content_revision(graph_dir)?;
    ensure_graph_store_seeded(graph_dir).map_err(AppError::rdf)?;
    Ok(())
}

fn verify_restored(graph_dir: &Path, manifest: &RestorePointManifest) -> AppResult<()> {
    let workspace_path = workspace_ydoc_state_path(graph_dir);
    if !workspace_path.is_file() && manifest.workspace.size_bytes > 0 {
        return Err(AppError::storage(format!(
            "verification failed: workspace bytes missing at {}",
            workspace_path.to_string_lossy()
        )));
    }
    for doc_ref in &manifest.documents {
        if crate::geist_derived_view::is_derived(graph_dir, &doc_ref.document_id)
            .map_err(AppError::storage)?
        {
            continue;
        }
        let live_path = document_ydoc_state_path(graph_dir, &doc_ref.document_id);
        if !live_path.is_file() {
            return Err(AppError::storage(format!(
                "verification failed: document {} missing live Y.Doc state",
                doc_ref.document_id
            )));
        }
    }
    Ok(())
}

fn rollback_to_backup(graph_dir: &Path, backup_pointer_id: &str) -> AppResult<()> {
    let backup_manifest = read_manifest(graph_dir, backup_pointer_id)?;
    apply_restore(graph_dir, &backup_manifest)?;
    // Do NOT discard the rebuild error: a rollback whose projection rebuild
    // failed left the graph's RDF at the aborted-restore's content, not the
    // backup's — the caller must learn the rollback did not fully succeed.
    rebuild_projections(graph_dir)?;
    Ok(())
}

fn check_cancelled(
    jobs: &Arc<LocalJobRegistry>,
    operation_id: &str,
    _graph_dir: &Path,
    _backup_pointer_id: &str,
) -> AppResult<bool> {
    jobs.is_cancelled(operation_id).map_err(AppError::internal)
}

fn update_phase(
    jobs: &Arc<LocalJobRegistry>,
    operation_id: &str,
    graph_id: &str,
    phase: &str,
    message: &str,
    percent: u8,
) -> AppResult<()> {
    let progress = LocalJobProgress {
        phase: phase.to_string(),
        message: message.to_string(),
        current: percent as usize,
        total: 100,
        percent: percent as f64,
        updated_at: clock::timestamp(),
        details: json!({
            "graphId": graph_id,
            "graph_id": graph_id,
        }),
    };
    let _ = jobs
        .update_progress(operation_id, progress)
        .map_err(AppError::internal)?;
    Ok(())
}

fn finish_failed(
    jobs: &Arc<LocalJobRegistry>,
    operation_id: &str,
    state_label: &str,
    error: &str,
    backup_pointer_id: Option<&str>,
) {
    let result = json!({
        "operationId": operation_id,
        "state": state_label,
        "error": error,
        "backupPointerId": backup_pointer_id,
    });
    let _ = jobs.finish_existing(operation_id, Err(result.to_string()), "application/json");
}

fn finish_rolled_back(
    jobs: &Arc<LocalJobRegistry>,
    operation_id: &str,
    error: &str,
    backup_pointer_id: &str,
) {
    let result = json!({
        "operationId": operation_id,
        "state": PHASE_ROLLED_BACK,
        "error": error,
        "backupPointerId": backup_pointer_id,
    });
    let _ = jobs.finish_existing(operation_id, Err(result.to_string()), "application/json");
}

fn restore_operation_json(record: &crate::local_jobs::LocalJobRecord) -> Value {
    let phase = record
        .progress
        .as_ref()
        .map(|p| p.phase.clone())
        .unwrap_or_else(|| "pending".to_string());
    let message = record
        .progress
        .as_ref()
        .map(|p| p.message.clone())
        .unwrap_or_default();
    let percent = record.progress.as_ref().map(|p| p.percent).unwrap_or(0.0);
    let state = job_status_phase(&record.status, &phase);
    let succeeded = matches!(record.status, crate::local_jobs::LocalJobStatus::Succeeded);
    // Generic job cancellation is not evidence that graph rollback ran. This
    // also covers cancellation after an uncertain final completion write.
    let outcome_unknown = matches!(record.status, crate::local_jobs::LocalJobStatus::Cancelled);
    let unknown_message = "Restore cancelled; rollback is not confirmed. Inspect the graph before retrying.";
    let phase = if succeeded || phase == PHASE_SUCCEEDED { state.clone() } else { phase };
    let graph_id = crate::local_jobs::local_job_graph_id(record);
    json!({
        "operationId": record.job_id,
        "graphId": graph_id,
        "state": state,
        "phase": phase,
        "message": if outcome_unknown { unknown_message } else if succeeded { "Restore complete" } else { &message },
        "percent": if succeeded { 100.0 } else { percent },
        "outcomeUnknown": outcome_unknown,
        "submittedAt": record.submitted_at,
        "startedAt": record.started_at,
        "updatedAt": record.updated_at,
        "completedAt": record.completed_at,
        "detail": record.detail,
        "error": if outcome_unknown { json!(unknown_message) } else { json!(record.error) },
    })
}

fn job_status_phase(status: &crate::local_jobs::LocalJobStatus, phase: &str) -> String {
    use crate::local_jobs::LocalJobStatus;
    match status {
        LocalJobStatus::Queued => "pending".to_string(),
        LocalJobStatus::Running if phase == PHASE_SUCCEEDED => PHASE_VERIFYING.to_string(),
        LocalJobStatus::Running => phase.to_string(),
        LocalJobStatus::Succeeded => PHASE_SUCCEEDED.to_string(),
        LocalJobStatus::Failed => {
            if phase == PHASE_ROLLED_BACK {
                PHASE_ROLLED_BACK.to_string()
            } else {
                PHASE_FAILED.to_string()
            }
        }
        LocalJobStatus::Cancelled => PHASE_FAILED.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crdt_queue::{enqueue_crdt_operation, EnqueueCrdtOperationInput};
    use crate::graph_service::{create_graph_service, CreateGraphInput};

    #[test]
    fn receipt_history_survives_real_restore_and_old_rooms_are_evicted() {
        use crate::vault_adoption::{apply_once, Claim};
        use crate::crdt_engine::rooms::RoomRegistry;
        use yrs::{Map, MapPrelim, ReadTxn, WriteTxn, Text};
        let _serial = crate::tauri_runtime::profile_env_serial().lock().unwrap_or_else(|p| p.into_inner());
        let previous = std::env::var_os("GARDEN_PROFILE_DIR");
        let profile = tempfile::tempdir().unwrap();
        std::env::set_var("GARDEN_PROFILE_DIR", profile.path());
        let result = std::panic::catch_unwind(|| {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph = "restore-receipts";
            create_graph_service(&app, CreateGraphInput {
                title: "Receipts".into(), graph_id: Some(graph.into()), description: None, operation_id: None,
            }).unwrap();
            crate::app_runtime::async_runtime::block_on(enqueue_crdt_operation(app.clone(), EnqueueCrdtOperationInput {
                kind: "workspace.createFolder".into(), graph_id: graph.into(), document_id: None,
                payload: json!({"folderId":"folder-a", "title":"Before"}),
            })).unwrap();
            let point = capture_restore_point(&app, graph, RestorePointTrigger::Manual, None).unwrap();
            let point_id = point["restorePointId"].as_str().unwrap();
            let dir = existing_graph_dir(&app, graph).unwrap();
            let registry = app.state::<RoomRegistry>();
            let room = crate::app_runtime::async_runtime::block_on(registry.get_or_create(&format!("workspace:{graph}"), workspace_ydoc_state_path(&dir))).unwrap();
            let scan = "f7e73c3f-cc36-4e9e-9af6-e6a9df4fb654";
            let id = Claim::artifact_id(graph, "incarnation", scan, 0);
            let claim = Claim::new(graph, "incarnation", scan, 0, &"b".repeat(64), &json!({
                "artifactId":id, "mode":"attached", "storage":"owned", "sha256":"a".repeat(64), "sizeBytes":3,
                "source":{"kind":"import", "importId":scan, "relativePath":"a.png"}
            })).unwrap();
            crate::app_runtime::async_runtime::block_on(room.update_doc(|_, txn| {
                apply_once(txn, &claim, |txn| {
                    txn.get_or_insert_map("artifacts").insert(txn, id.as_str(), MapPrelim::default());
                    Ok(())
                })?;
                Ok(())
            })).unwrap();
            let jobs = Arc::new(LocalJobRegistry::new(profile.path().join("restore-jobs")).unwrap());
            let job = jobs.insert_queued(RESTORE_JOB_TYPE, Some(graph.into()), json!({"graphId":graph})).unwrap();
            crate::app_runtime::async_runtime::block_on(run_restore(&app, jobs.clone(), graph, point_id, &job.job_id)).unwrap();
            assert!(crate::app_runtime::async_runtime::block_on(room.update_doc(|_, _| Ok(()))).unwrap_err().contains("evicted"));
            let reopened = crate::app_runtime::async_runtime::block_on(registry.get_or_create(&format!("workspace:{graph}"), workspace_ydoc_state_path(&dir))).unwrap();
            crate::app_runtime::async_runtime::block_on(reopened.update_doc(|_, txn| {
                assert!(!txn.get_map("artifacts").is_some_and(|m| m.contains_key(&*txn, &id)));
                assert!(apply_once(txn, &claim, |_| panic!("restore allowed resurrection")).unwrap().replayed);
                Ok(())
            })).unwrap();
            crate::app_runtime::async_runtime::block_on(reopened.update_doc(|_, txn| {
                txn.get_or_insert_text(crate::workspace_receipt_guard::RECEIPT_MAP).insert(txn, 0, "corrupt root shape");
                Ok(())
            })).unwrap();
            let refused = jobs.insert_queued(RESTORE_JOB_TYPE, Some(graph.into()), json!({"graphId":graph})).unwrap();
            let error = crate::app_runtime::async_runtime::block_on(run_restore(&app, jobs, graph, point_id, &refused.job_id)).unwrap_err();
            assert!(error.message().contains("receipt root"));
            crate::app_runtime::async_runtime::block_on(reopened.update_doc(|_, _| Ok(()))).expect("preflight refusal must keep live rooms usable");
        });
        match previous { Some(p) => std::env::set_var("GARDEN_PROFILE_DIR", p), None => std::env::remove_var("GARDEN_PROFILE_DIR") }
        if let Err(p) = result { std::panic::resume_unwind(p); }
    }

    #[test]
    fn progress_alone_cannot_report_durable_success() {
        assert_ne!(job_status_phase(&crate::local_jobs::LocalJobStatus::Running, PHASE_SUCCEEDED), PHASE_SUCCEEDED);
        assert_eq!(job_status_phase(&crate::local_jobs::LocalJobStatus::Succeeded, PHASE_VERIFYING), PHASE_SUCCEEDED);
    }

    #[test]
    fn failed_completion_storage_never_reports_success() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("jobs");
        let jobs = LocalJobRegistry::new(path.clone()).unwrap();
        let job = jobs.insert_queued(RESTORE_JOB_TYPE, Some("g".into()), json!({"graphId":"g"})).unwrap();
        jobs.mark_running(&job.job_id).unwrap();
        std::fs::rename(&path, root.path().join("saved-jobs")).unwrap();
        std::fs::write(&path, b"injected storage obstruction").unwrap();
        assert!(finish_restore_job(&jobs, &job.job_id, &json!({"state":"succeeded"})).is_err());
        assert_ne!(get_restore_operation(&jobs, &job.job_id).unwrap()["state"], "succeeded");
        std::fs::remove_file(&path).unwrap();
        std::fs::rename(root.path().join("saved-jobs"), &path).unwrap();
        let cancelled = cancel_restore_operation(&jobs, &job.job_id).unwrap();
        assert_eq!(cancelled["rollbackConfirmed"], false);
        let unknown = get_restore_operation(&jobs, &job.job_id).unwrap();
        assert_eq!(unknown["state"], "failed");
        assert_eq!(unknown["outcomeUnknown"], true);
        assert!(finish_restore_job(&jobs, &job.job_id, &json!({"state":"succeeded"})).is_err());
        let job = jobs.insert_queued(RESTORE_JOB_TYPE, Some("g".into()), json!({"graphId":"g"})).unwrap();
        finish_restore_job(&jobs, &job.job_id, &json!({"state":"succeeded"})).unwrap();
        let status = get_restore_operation(&jobs, &job.job_id).unwrap();
        assert_eq!(status["state"], "succeeded");
        assert_eq!(status["percent"], 100.0);
    }

    #[test]
    fn ordinary_restore_preserves_legacy_workspace_bytes_without_decoding() {
        let graph_dir =
            std::env::temp_dir().join(format!("garden-ordinary-restore-{}", uuid::Uuid::new_v4()));
        let manifest = RestorePointManifest {
            schema_version: 2,
            restore_point_id: "ordinary-restore".into(),
            graph_id: "graph-a".into(),
            created_at: 0,
            trigger: RestorePointTrigger::Manual,
            label: None,
            content_hash_sha256: String::new(),
            workspace: crate::time_travel_types::WorkspaceSnapshotRef {
                bytes_path: String::new(),
                snapshot_path: String::new(),
                size_bytes: 0,
            },
            documents: Vec::new(),
            metadata: Default::default(),
        };
        let snapshot = json!({"documents": [{"id": "ordinary", "title": "Legacy"}]});
        for bytes in [b"opaque-legacy-workspace".as_slice(), b"".as_slice()] {
            // Current history must be readable even when the old snapshot is
            // opaque, otherwise absence of import receipts cannot be proven.
            use yrs::{ReadTxn, Transact};
            let current = yrs::Doc::new().transact().encode_state_as_update_v1(&yrs::StateVector::default());
            write_bytes(&workspace_ydoc_state_path(&graph_dir), &current).unwrap();
            crate::time_travel_store::write_workspace_bundle(
                &graph_dir,
                &manifest.restore_point_id,
                bytes,
                &snapshot,
            )
            .unwrap();
            let prepared = crate::geist_derived_restore::prepare(&graph_dir, &manifest).unwrap();
            assert!(prepared.derived_ids.is_empty());
            assert_eq!(prepared.workspace_bytes, bytes);
            assert_eq!(prepared.workspace_snapshot, snapshot);
            apply_restore(&graph_dir, &manifest).unwrap();
            assert_eq!(
                std::fs::read(workspace_ydoc_state_path(&graph_dir)).unwrap(),
                bytes
            );
            assert_eq!(
                read_json::<Value>(&workspace_snapshot_path(&graph_dir)).unwrap(),
                snapshot
            );
        }
        std::fs::remove_dir_all(graph_dir).unwrap();
    }

    #[test]
    fn unreadable_receipt_history_refuses_before_document_writes() {
        let directory = tempfile::tempdir().unwrap();
        let graph_dir = directory.path();
        let manifest: RestorePointManifest = serde_json::from_value(json!({
            "schema_version":2,"restore_point_id":"old","graph_id":"g","created_at":0,
            "trigger":"manual","label":null,"content_hash_sha256":"",
            "workspace":{"bytes_path":"","snapshot_path":"","size_bytes":0},
            "documents":[{"document_id":"ordinary","title":"Old","snapshot_id":"old","size_bytes":0,"block_count":0,"char_count":0}],
            "metadata": {"folder_count":0,"document_count":1,"artifact_count":0,"wire_count":0,"workspace_size_bytes":0,"total_document_size_bytes":0}
        })).unwrap();
        write_bytes(&workspace_ydoc_state_path(graph_dir), b"unreadable current history").unwrap();
        write_bytes(&document_ydoc_state_path(graph_dir, "ordinary"), b"untouched").unwrap();
        crate::time_travel_store::write_workspace_bundle(graph_dir, "old", &[], &json!({})).unwrap();
        assert!(apply_restore(graph_dir, &manifest).unwrap_err().message().contains("receipt history"));
        assert_eq!(std::fs::read(document_ydoc_state_path(graph_dir, "ordinary")).unwrap(), b"untouched");
        assert_eq!(std::fs::read(workspace_ydoc_state_path(graph_dir)).unwrap(), b"unreadable current history");
        std::fs::remove_file(workspace_ydoc_state_path(graph_dir)).unwrap();
        assert!(apply_restore(graph_dir, &manifest).unwrap_err().message().contains("receipt history"));
        assert_eq!(std::fs::read(document_ydoc_state_path(graph_dir, "ordinary")).unwrap(), b"untouched");
        assert!(!workspace_ydoc_state_path(graph_dir).exists(), "restore must not invent empty history");
    }

    #[test]
    fn graph_restore_preserves_derived_views_and_restores_ordinary_bytes() {
        use yrs::{
            updates::decoder::Decode, Doc, Map, MapPrelim, ReadTxn, StateVector, Transact, Update,
        };
        let graph_dir =
            std::env::temp_dir().join(format!("garden-derived-restore-{}", uuid::Uuid::new_v4(),));
        for id in ["derived-view", "derived-retained-parent"] {
            crate::geist_derived_view::register(&graph_dir, id).unwrap();
            write_bytes(
                &document_ydoc_state_path(&graph_dir, id),
                b"current-derived-bytes",
            )
            .unwrap();
            write_json(
                &graph_dir.join("documents").join(id).join("document.json"),
                &json!({"title": "Current projection", "capabilities": []}),
            )
            .unwrap();
        }
        let make_workspace = |current: bool| {
            let doc = Doc::new();
            let documents = doc.get_or_insert_map("documents");
            let folders = doc.get_or_insert_map("folders");
            {
                let mut txn = doc.transact_mut();
                let folder = folders.insert(&mut txn, "retained", MapPrelim::default());
                folder.insert(&mut txn, "name", "Retained");
                folder.insert(&mut txn, "section", "documents");
                let ordinary = documents.insert(&mut txn, "ordinary", MapPrelim::default());
                ordinary.insert(
                    &mut txn,
                    "title",
                    if current {
                        "Current ordinary"
                    } else {
                        "Old ordinary"
                    },
                );
                if current {
                    for (id, parent) in [
                        ("derived-view", "missing"),
                        ("derived-retained-parent", "retained"),
                    ] {
                        let row = documents.insert(&mut txn, id, MapPrelim::default());
                        row.insert(&mut txn, "title", "Current projection");
                        row.insert(&mut txn, "parentId", parent);
                        row.insert(&mut txn, "order", 7.0);
                        row.insert(&mut txn, "description", "Keep custom metadata");
                        row.insert(&mut txn, "readOnly", true);
                    }
                } else {
                    let row = documents.insert(&mut txn, "derived-view", MapPrelim::default());
                    row.insert(&mut txn, "title", "Stale projection");
                }
            }
            doc
        };
        let current = make_workspace(true);
        let target = make_workspace(false);
        let current_snapshot =
            crate::crdt_engine::workspace_ops::materialize_workspace_snapshot_json(
                "graph-a", &current,
            )
            .unwrap();
        write_bytes(
            &workspace_ydoc_state_path(&graph_dir),
            &current
                .transact()
                .encode_state_as_update_v1(&StateVector::default()),
        )
        .unwrap();
        write_json(&workspace_snapshot_path(&graph_dir), &current_snapshot).unwrap();
        crate::time_travel_store::write_workspace_bundle(
            &graph_dir,
            "restore-a",
            &target
                .transact()
                .encode_state_as_update_v1(&StateVector::default()),
            &Value::Null,
        )
        .unwrap();
        crate::time_travel_store::write_document_bytes(
            &graph_dir,
            "restore-a",
            "ordinary",
            b"old-ordinary",
        )
        .unwrap();
        crate::time_travel_store::write_document_bytes(
            &graph_dir,
            "restore-a",
            "derived-view",
            b"old-derived",
        )
        .unwrap();
        let native = graph_dir.join("narrative/song.json");
        write_bytes(&native, b"native-song-unchanged").unwrap();
        let manifest: RestorePointManifest = serde_json::from_value(json!({
            "schema_version": 2,
            "restore_point_id": "restore-a",
            "graph_id": "graph-a",
            "created_at": 0,
            "trigger": "manual",
            "label": null,
            "content_hash_sha256": "",
            "workspace": { "bytes_path": "", "snapshot_path": "", "size_bytes": 0 },
            "documents": (["ordinary", "derived-view"].map(|id| json!({
                "document_id": id, "title": id, "snapshot_id": "snapshot-a",
                "size_bytes": 0, "block_count": 0, "char_count": 0,
            }))),
            "metadata": {
                "folder_count": 0, "document_count": 2, "artifact_count": 0,
                "wire_count": 0, "workspace_size_bytes": 0, "total_document_size_bytes": 0,
            },
        }))
        .unwrap();
        let prepared = crate::geist_derived_restore::prepare(&graph_dir, &manifest).unwrap();
        assert_eq!(prepared.derived_ids.len(), 2);
        assert_eq!(
            manifest
                .documents
                .iter()
                .filter(|doc| !prepared.derived_ids.contains(&doc.document_id))
                .count(),
            1
        );
        for _ in 0..2 {
            apply_restore(&graph_dir, &manifest).expect("mixed graph remains restorable");
            assert_eq!(
                std::fs::read(document_ydoc_state_path(&graph_dir, "ordinary")).unwrap(),
                b"old-ordinary"
            );
            for id in ["derived-view", "derived-retained-parent"] {
                assert_eq!(
                    std::fs::read(document_ydoc_state_path(&graph_dir, id)).unwrap(),
                    b"current-derived-bytes"
                );
                let record: Value =
                    read_json(&graph_dir.join("documents").join(id).join("document.json")).unwrap();
                assert_eq!(
                    record,
                    json!({"title": "Current projection", "capabilities": []})
                );
            }
            assert_eq!(std::fs::read(&native).unwrap(), b"native-song-unchanged");
            let snapshot: Value = read_json(&workspace_snapshot_path(&graph_dir)).unwrap();
            let restored_doc = Doc::new();
            restored_doc
                .transact_mut()
                .apply_update(
                    Update::decode_v1(
                        &std::fs::read(workspace_ydoc_state_path(&graph_dir)).unwrap(),
                    )
                    .unwrap(),
                )
                .unwrap();
            let decoded = crate::crdt_engine::workspace_ops::materialize_workspace_snapshot_json(
                "graph-a",
                &restored_doc,
            )
            .unwrap();
            for field in ["documents", "folders", "tree", "counts"] {
                assert_eq!(snapshot[field], decoded[field]);
            }
            for id in ["derived-view", "derived-retained-parent"] {
                let original = current_snapshot["documents"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|row| row["id"] == id)
                    .unwrap();
                let mut expected = original.clone();
                if id == "derived-view" {
                    expected["parentId"] = Value::Null;
                }
                let restored = snapshot["documents"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|row| row["id"] == id)
                    .unwrap();
                assert_eq!(restored, &expected);
            }
            let ordinary = snapshot["documents"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["id"] == "ordinary")
                .unwrap();
            assert_eq!(ordinary["title"], "Old ordinary");
            assert!(restore_document_metadata(&graph_dir, &manifest.documents[1]).is_err());
        }
        // Invalid server authority fails preparation before replacing any ordinary bytes.
        write_bytes(
            &document_ydoc_state_path(&graph_dir, "ordinary"),
            b"not-yet-restored",
        )
        .unwrap();
        write_bytes(
            &graph_dir.join("narrative/derived-views/derived-view.json"),
            b"invalid",
        )
        .unwrap();
        assert!(apply_restore(&graph_dir, &manifest).is_err());
        assert_eq!(
            std::fs::read(document_ydoc_state_path(&graph_dir, "ordinary")).unwrap(),
            b"not-yet-restored"
        );
        std::fs::remove_dir_all(graph_dir).unwrap();
    }

    /// Regression for the seed-floor stale-RDF-after-restore hazard:
    /// `content_revision` is MONOTONIC (`max(now, prev+1)`) and can run ahead
    /// of the wall clock, while restore used to stamp restored records with
    /// RAW wall-clock time and reconcile no RDF of its own. A restored
    /// document stamped below the marker floor was skipped by the seed's
    /// incremental walk, the marker advanced past it, and its PRE-restore RDF
    /// was certified current forever. Real capture, real restore apply, real
    /// seed pass, real store read-back — no mocks: with the marker floor
    /// forced AHEAD of the wall clock, the restored document must (a) carry a
    /// monotonic stamp that clears the floor, (b) have its stale post-capture
    /// RDF reclaimed by restore's OWN direct reconcile (asserted before any
    /// seed pass runs — the race-window pin), and (c) still read clean after
    /// the rebuild's seed pass.
    #[test]
    fn restored_documents_rematerialize_even_when_marker_floor_ran_ahead_of_wall_clock() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let profile = std::env::temp_dir().join(format!("garden-restore-floor-{nanos}"));
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let result = std::panic::catch_unwind(|| {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "restore-floor-ahead";
            let doc_id = "restore-doc";
            create_graph_service(
                &app,
                CreateGraphInput {
                    title: "Restore Floor Ahead".to_string(),
                    graph_id: Some(graph_id.to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");
            let graph_dir = existing_graph_dir(&app, graph_id).expect("graph dir");

            let write_doc = |content: &str| {
                crate::app_runtime::async_runtime::block_on(enqueue_crdt_operation(
                    app.clone(),
                    EnqueueCrdtOperationInput {
                        kind: "document.write".to_string(),
                        graph_id: graph_id.to_string(),
                        document_id: Some(doc_id.to_string()),
                        payload: json!({
                            "documentId": doc_id,
                            "content": content,
                            "format": "markdown",
                            "title": "Restore Doc",
                        }),
                    },
                ))
                .expect("document.write drains through the real engine");
            };

            // v1 → capture → v2 (v2's save reconciles v2 RDF itself).
            write_doc("RestoreSentinelOne is the captured content.");
            let summary = capture_restore_point(&app, graph_id, RestorePointTrigger::Manual, None)
                .expect("capture restore point");
            let restore_point_id = summary["restorePointId"]
                .as_str()
                .expect("restore point id")
                .to_string();
            write_doc("RestoreSentinelTwo is the post-capture content.");

            // Force the monotonic run-ahead: content_revision (and so the
            // next marker floor) lands ten minutes PAST the wall clock.
            let graph_path = graph_dir.join("graph.json");
            let mut graph: GraphRecord = read_json(&graph_path).expect("graph record");
            let future_floor = crate::clock::epoch_millis() as i64 + 600_000;
            graph.content_revision = Some(future_floor.to_string());
            write_json(&graph_path, &graph).expect("write run-ahead revision");
            ensure_graph_store_seeded(&graph_dir).expect("seed with run-ahead marker floor");

            // The REAL restore application path (`run_restore`'s body):
            // swap bytes + metadata, then rebuild projections.
            let manifest = read_manifest(&graph_dir, &restore_point_id).expect("manifest");
            apply_restore(&graph_dir, &manifest).expect("apply restore");

            // SELF-RECONCILE observable — asserted BEFORE rebuild_projections
            // runs any seed pass: restore itself must reconcile each restored
            // document's RDF at metadata-write time, so correctness never
            // depends on the seed walk including the document. (This is the
            // race-window pin: a concurrently advanced marker floor can make
            // the seed skip the restored document entirely; the projection
            // must already be correct by then. Without the direct reconcile,
            // the stale post-capture sentinel survives until — and past — a
            // skipping walk.)
            {
                let store =
                    crate::rdf_store_service::open_graph_store(&graph_dir).expect("open store");
                let projection_graph =
                    crate::rdf_authority::document_projection_graph_iri(graph_id, doc_id);
                let stale_before_rebuild = crate::rdf_query_service::execute_sparql_query(
                    &store,
                    &format!(
                        r#"ASK {{ GRAPH <{projection_graph}> {{ ?s ?p ?o . FILTER(CONTAINS(STR(?o), "RestoreSentinelTwo")) }} }}"#
                    ),
                )
                .expect("query stale sentinel before rebuild");
                assert_eq!(
                    stale_before_rebuild.boolean,
                    Some(false),
                    "restore must reconcile the restored document's RDF ITSELF, before any seed pass"
                );
            }

            rebuild_projections(&graph_dir).expect("rebuild projections");

            // (a) The restored record's stamp cleared the run-ahead floor —
            // the monotonic discipline, not wall clock.
            let manifest_path = documents_manifest_path(&graph_dir, doc_id);
            let record: DocumentRecord =
                read_json(&manifest_path).expect("restored document record");
            let stamp: i64 = record
                .updated_at
                .parse()
                .expect("restored stamp is epoch-ms");
            assert!(
                stamp >= future_floor,
                "restored stamp {stamp} must clear the run-ahead marker floor {future_floor}"
            );
            assert_eq!(record.title, "Restore Doc");

            // (b) The rebuild's seed pass re-materialized the restored
            // document: its stale post-capture projection is GONE. (Without
            // the monotonic stamp the walk skips the document — stamped in
            // the past — and "RestoreSentinelTwo" survives as certified-fresh
            // RDF.)
            let store = crate::rdf_store_service::open_graph_store(&graph_dir).expect("open store");
            let projection_graph =
                crate::rdf_authority::document_projection_graph_iri(graph_id, doc_id);
            let stale = crate::rdf_query_service::execute_sparql_query(
                &store,
                &format!(
                    r#"ASK {{ GRAPH <{projection_graph}> {{ ?s ?p ?o . FILTER(CONTAINS(STR(?o), "RestoreSentinelTwo")) }} }}"#
                ),
            )
            .expect("query stale sentinel");
            assert_eq!(
                stale.boolean,
                Some(false),
                "the restored document's stale post-capture RDF must be reclaimed by the seed pass"
            );
        });

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    fn documents_manifest_path(graph_dir: &Path, document_id: &str) -> std::path::PathBuf {
        crate::paths::documents_dir(graph_dir)
            .join(document_id)
            .join("document.json")
    }

    /// Regression for the swallowed per-document reconcile failure: a mixed /
    /// partial restore must FAIL, never report success. `apply_restore` used
    /// to catch every `restore_document_metadata` error, log a warning, and
    /// continue — so a document whose metadata/RDF reconcile failed (since
    /// round 3 this path also self-reconciles the document's RDF) left a
    /// PARTIAL restore that still reported SUCCESS, because verification only
    /// checks Y.Doc sidecar existence. Real capture, real restore apply, real
    /// injected failure — no mocks: one document's snapshot payload is deleted
    /// so `read_document_snapshot_payload` errors (the document dir AND
    /// document.json are both still present, so this is NOT the tolerated
    /// missing-metadata skip), and `apply_restore` must return an error naming
    /// the failing document rather than succeed.
    #[test]
    fn restore_fails_when_a_document_reconcile_fails() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let profile = std::env::temp_dir().join(format!("garden-restore-reconcile-fail-{nanos}"));
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let result = std::panic::catch_unwind(|| {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "restore-reconcile-fail";
            let doc_id = "restore-doc";
            create_graph_service(
                &app,
                CreateGraphInput {
                    title: "Restore Reconcile Fail".to_string(),
                    graph_id: Some(graph_id.to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");
            let graph_dir = existing_graph_dir(&app, graph_id).expect("graph dir");

            crate::app_runtime::async_runtime::block_on(enqueue_crdt_operation(
                app.clone(),
                EnqueueCrdtOperationInput {
                    kind: "document.write".to_string(),
                    graph_id: graph_id.to_string(),
                    document_id: Some(doc_id.to_string()),
                    payload: json!({
                        "documentId": doc_id,
                        "content": "Reconcile failure sentinel content.",
                        "format": "markdown",
                        "title": "Restore Doc",
                    }),
                },
            ))
            .expect("document.write drains through the real engine");

            let summary = capture_restore_point(&app, graph_id, RestorePointTrigger::Manual, None)
                .expect("capture restore point");
            let restore_point_id = summary["restorePointId"]
                .as_str()
                .expect("restore point id")
                .to_string();

            let manifest = read_manifest(&graph_dir, &restore_point_id).expect("manifest");
            let doc_ref = manifest
                .documents
                .iter()
                .find(|d| d.document_id == doc_id)
                .expect("document present in manifest");

            // Inject a REAL, non-tolerated reconcile failure: remove the
            // snapshot payload so `read_document_snapshot_payload` errors. The
            // document dir and document.json remain, so this is NOT the
            // missing-metadata skip — it is a genuine per-document failure.
            crate::document_history_file_store::remove_snapshot_payload(
                &graph_dir,
                doc_id,
                &doc_ref.snapshot_id,
            );

            let error = apply_restore(&graph_dir, &manifest)
                .expect_err("apply_restore must FAIL when a document reconcile fails");
            assert!(
                error.message_ref().contains(doc_id),
                "restore error must name the failing document, got: {}",
                error.message_ref()
            );
        });

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }
}
