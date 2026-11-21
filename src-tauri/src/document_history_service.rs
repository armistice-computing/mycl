use crate::app_runtime::AppHandle;
pub(super) use crate::document_history_hosted::{
    document_snapshot_hosted_json, hosted_document_snapshot_count_response,
    hosted_document_snapshot_html_response, hosted_document_snapshot_list_response,
    hosted_document_snapshot_text_response,
};
use crate::{
    clock::timestamp,
    document_history_persistence::{
        collapse_document_history, read_document_history_store,
        read_document_snapshot_payload, remove_snapshot_payload, write_document_history_store,
    },
    document_history_projection::snapshot_diff_counts,
    document_history_store::{
        document_history_snapshots_dir, document_snapshot_payload_path, LocalDocumentHistoryStore,
        LocalDocumentSnapshotMeta,
        LocalDocumentSnapshotPayload,
    },
    document_projection_service::{document_blocks_for_read, document_xml},
    document_service::DocumentRecord,
    ids::validate_local_id,
    paths::{existing_document_dir, existing_graph_dir},
    storage::{create_dir_all, write_json},
};
use fs4::fs_std::FileExt;
#[cfg(test)]
use std::sync::{mpsc::Sender, Mutex, OnceLock};
use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
};
use uuid::Uuid;

struct DocumentHistoryMutationGuard {
    _file: File,
}

#[cfg(test)]
struct HistoryLockAttemptHook {
    lock_path: PathBuf,
    sender: Sender<PathBuf>,
}

#[cfg(test)]
static HISTORY_LOCK_ATTEMPT_HOOK: OnceLock<Mutex<Option<HistoryLockAttemptHook>>> = OnceLock::new();

#[cfg(test)]
fn install_history_lock_attempt_hook(lock_path: PathBuf, sender: Sender<PathBuf>) {
    *HISTORY_LOCK_ATTEMPT_HOOK
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) =
        Some(HistoryLockAttemptHook { lock_path, sender });
}

#[cfg(test)]
fn clear_history_lock_attempt_hook() {
    *HISTORY_LOCK_ATTEMPT_HOOK
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
}

#[cfg(test)]
pub(crate) fn install_document_history_lock_attempt_hook(
    graph_dir: &Path,
    document_id: &str,
    sender: Sender<PathBuf>,
) -> Result<(), String> {
    install_history_lock_attempt_hook(document_history_lock_path(graph_dir, document_id)?, sender);
    Ok(())
}

#[cfg(test)]
pub(crate) fn clear_document_history_lock_attempt_hook() {
    clear_history_lock_attempt_hook();
}

#[cfg(test)]
fn notify_history_lock_attempt(lock_path: &Path) {
    let sender = HISTORY_LOCK_ATTEMPT_HOOK
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
        .filter(|hook| hook.lock_path == lock_path)
        .map(|hook| hook.sender.clone());
    if let Some(sender) = sender {
        let _ = sender.send(lock_path.to_path_buf());
    }
}

fn document_history_lock_path(graph_dir: &Path, document_id: &str) -> Result<PathBuf, String> {
    validate_local_id(document_id, "document_id")?;
    Ok(graph_dir
        .join(".history-locks")
        .join(document_id)
        .join(".lock"))
}

fn acquire_document_history_mutation_lock(
    graph_dir: &Path,
    document_id: &str,
) -> Result<DocumentHistoryMutationGuard, String> {
    let lock_path = document_history_lock_path(graph_dir, document_id)?;
    create_dir_all(
        lock_path
            .parent()
            .ok_or_else(|| format!("history lock has no parent: {}", lock_path.display()))?,
    )?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|error| format!("open history lock {}: {error}", lock_path.display()))?;
    #[cfg(test)]
    notify_history_lock_attempt(&lock_path);
    file.lock_exclusive()
        .map_err(|error| format!("lock document history {}: {error}", lock_path.display()))?;
    Ok(DocumentHistoryMutationGuard { _file: file })
}

/// Run one synchronous operation while excluding every history
/// read-modify-write for the same document. The lock is deliberately
/// non-reentrant; `operation` must not call a document-history entrypoint for
/// this document.
pub(crate) fn with_document_history_lock<T>(
    graph_dir: &Path,
    document_id: &str,
    operation: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let _durability_guard = crate::cell_durability::write_guard();
    let _history_guard = acquire_document_history_mutation_lock(graph_dir, document_id)?;
    operation()
}

/// Graph duplication reads a document's history directory under the same
/// exclusive lock as capture/copy/delete, producing one coherent index and
/// payload view even though manual snapshots do not take the graph lease.
pub(crate) fn with_document_history_read_lock<T>(
    graph_dir: &Path,
    document_id: &str,
    read: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    with_document_history_lock(graph_dir, document_id, read)
}

pub(super) fn capture_document_snapshot(
    graph_dir: &Path,
    document: &DocumentRecord,
    is_manual: bool,
    label: Option<String>,
) -> Result<LocalDocumentSnapshotMeta, String> {
    let _durability_guard = crate::cell_durability::write_guard();
    let _history_guard = acquire_document_history_mutation_lock(graph_dir, &document.document_id)?;
    crate::document_tombstone_store::require_document_not_tombstoned(
        graph_dir,
        &document.document_id,
    )?;
    capture_document_snapshot_locked(graph_dir, document, is_manual, label)
}

/// The automatic history gate (cloud-1's rhythm): at most one new automatic
/// entry per 20 minutes. Saving to disk is unaffected; this only decides how
/// sparse history is.
const AUTO_SNAPSHOT_GATE_MILLIS: u128 = 20 * 60 * 1000;

/// The newest entry, when it is automatic and its 20-minute stretch is still
/// open at `now`: the save rolls it forward instead of adding an entry. A
/// manual snapshot taken since is a deliberate checkpoint: the next
/// automatic save opens a fresh stretch after it.
fn open_rolling_position(store: &LocalDocumentHistoryStore, now: u128) -> Option<usize> {
    let position = store.snapshots.len().checked_sub(1)?;
    let newest = &store.snapshots[position];
    if newest.is_manual || newest.tier != "20min" {
        return None;
    }
    let opened = crate::clock::parse_timestamp(newest.opened_at.as_deref().unwrap_or(&newest.created_at))?;
    // An entry opened in the future (a clock set back, imported history)
    // is not an open stretch: rolling it would hold the gate until that
    // future time, overwriting everything in between. Start a fresh one.
    let age = now.checked_sub(opened)?;
    (age < AUTO_SNAPSHOT_GATE_MILLIS).then_some(position)
}

fn capture_document_snapshot_locked(
    graph_dir: &Path,
    document: &DocumentRecord,
    is_manual: bool,
    label: Option<String>,
) -> Result<LocalDocumentSnapshotMeta, String> {
    let mut store =
        read_document_history_store(graph_dir, &document.graph_id, &document.document_id)?;
    let created_at = timestamp();
    let now = crate::clock::parse_timestamp(&created_at).unwrap_or(0);

    if !is_manual {
        // Unchanged content (a save that only bumped the revision, or a
        // repeat): the newest automatic entry already holds it, so it is
        // re-tagged with this revision and nothing new is written.
        if let Some(position) = store.snapshots.iter().rposition(|snapshot| !snapshot.is_manual) {
            let candidate = store.snapshots[position].clone();
            if matches!(try_snapshot_meta_matches_document(graph_dir, &candidate, document), Ok(true)) {
                store.snapshots[position].document_revision = Some(document.revision);
                store.latest_automatic_revision = Some(document.revision);
                store.latest_automatic_snapshot_id = Some(candidate.snapshot_id.clone());
                write_document_history_store(graph_dir, &store)?;
                return Ok(store.snapshots[position].clone());
            }
        }
    }

    let rolling = if is_manual { None } else { open_rolling_position(&store, now) };
    let blocks = document_blocks_for_read(document);
    // The diff is against the state before this entry: for a rolling entry,
    // the newest entry it does not replace.
    let previous_blocks = {
        let before = rolling.unwrap_or(store.snapshots.len());
        let mut found = None;
        for snapshot in store.snapshots[..before].iter().rev() {
            if let Ok(payload) =
                read_document_snapshot_payload(graph_dir, &store.document_id, &snapshot.snapshot_id)
            {
                found = Some(payload.blocks);
                break;
            }
        }
        found.unwrap_or_default()
    };
    let (blocks_added, blocks_removed, blocks_modified, chars_added, chars_removed) =
        snapshot_diff_counts(&previous_blocks, &blocks);
    // A rolling entry keeps its id, so a preview, a pending restore or a
    // copy that names it never finds it gone; only its content moves on.
    let snapshot_id = match rolling {
        Some(position) => store.snapshots[position].snapshot_id.clone(),
        None => format!("snapshot-{created_at}-{}", Uuid::new_v4()),
    };
    let (tier, snapshot_count, opened_at) = match rolling {
        Some(position) => {
            let open = &store.snapshots[position];
            (
                open.tier.clone(),
                open.snapshot_count,
                Some(open.opened_at.clone().unwrap_or_else(|| open.created_at.clone())),
            )
        }
        None => ("20min".to_string(), 1, (!is_manual).then(|| created_at.clone())),
    };
    let meta = LocalDocumentSnapshotMeta {
        snapshot_id: snapshot_id.clone(),
        graph_id: document.graph_id.clone(),
        document_id: document.document_id.clone(),
        is_manual,
        document_revision: Some(document.revision),
        tier,
        snapshot_count,
        chars_added,
        chars_removed,
        blocks_added,
        blocks_removed,
        blocks_modified,
        created_at: created_at.clone(),
        label: label.filter(|value| !value.trim().is_empty()),
        opened_at,
    };
    let payload = LocalDocumentSnapshotPayload {
        snapshot_id: snapshot_id.clone(),
        graph_id: document.graph_id.clone(),
        document_id: document.document_id.clone(),
        title: document.title.clone(),
        created_at,
        blocks,
        tiptap_xml: document_xml(document),
    };

    create_dir_all(&document_history_snapshots_dir(
        graph_dir,
        &document.document_id,
    ))?;
    // The payload is written (atomically) before the index. If a crash falls
    // between them on a roll, the entry's payload is newer than its tag; the
    // next tail check finds the content matches and re-tags it.
    write_json(
        &document_snapshot_payload_path(graph_dir, &document.document_id, &snapshot_id),
        &payload,
    )?;
    let obsolete_payloads = match rolling {
        Some(position) => {
            store.snapshots[position] = meta.clone();
            Vec::new()
        }
        None => {
            store.total_count = store.total_count.saturating_add(1);
            store.snapshots.push(meta.clone());
            collapse_document_history(&mut store)
        }
    };
    if !is_manual {
        store.latest_automatic_revision = Some(document.revision);
        store.latest_automatic_snapshot_id = Some(snapshot_id);
    }
    write_document_history_store(graph_dir, &store)?;
    for obsolete_snapshot_id in obsolete_payloads {
        remove_snapshot_payload(graph_dir, &document.document_id, &obsolete_snapshot_id);
    }
    Ok(meta)
}

pub(super) fn snapshot_payload_matches_document(
    payload: &LocalDocumentSnapshotPayload,
    document: &DocumentRecord,
) -> Result<bool, String> {
    let payload_blocks = serde_json::to_value(&payload.blocks)
        .map_err(|error| format!("serialize snapshot blocks: {error}"))?;
    let document_blocks = serde_json::to_value(document_blocks_for_read(document))
        .map_err(|error| format!("serialize document blocks: {error}"))?;
    Ok(payload.graph_id == document.graph_id
        && payload.document_id == document.document_id
        && payload.title == document.title
        && payload_blocks == document_blocks
        && payload.tiptap_xml == document_xml(document))
}

fn try_snapshot_meta_matches_document(
    graph_dir: &Path,
    meta: &LocalDocumentSnapshotMeta,
    document: &DocumentRecord,
) -> Result<bool, String> {
    read_document_snapshot_payload(graph_dir, &document.document_id, &meta.snapshot_id).and_then(
        |payload| {
            Ok(payload.snapshot_id == meta.snapshot_id
                && snapshot_payload_matches_document(&payload, document)?)
        },
    )
}

fn snapshot_meta_matches_document(
    graph_dir: &Path,
    meta: &LocalDocumentSnapshotMeta,
    document: &DocumentRecord,
) -> bool {
    try_snapshot_meta_matches_document(graph_dir, meta, document).unwrap_or(false)
}

/// Ensure the automatic history tail for one durable document revision.
///
/// `document.json` is written before RDF/history/graph-touch, so recovery can
/// encounter an identical cold record whose snapshot tail never committed.
/// Exact per-snapshot revision metadata makes that repair idempotent even when
/// a comments-only revision has the same title/blocks/XML as its predecessor.
pub(super) fn ensure_document_snapshot_for_revision(
    graph_dir: &Path,
    document: &DocumentRecord,
) -> Result<LocalDocumentSnapshotMeta, String> {
    ensure_document_snapshot_for_revision_with_tombstone_fence(graph_dir, document, None)
}

/// Internal recreation path: normal history writes reject every tombstoned
/// identity, while a fresh same-ID document may persist only against the exact
/// deletion UUID it prepared. A newer delete invalidates this capability.
pub(super) fn ensure_document_snapshot_for_revision_with_tombstone_fence(
    graph_dir: &Path,
    document: &DocumentRecord,
    expected_deletion_id: Option<&str>,
) -> Result<LocalDocumentSnapshotMeta, String> {
    let _durability_guard = crate::cell_durability::write_guard();
    let _history_guard = acquire_document_history_mutation_lock(graph_dir, &document.document_id)?;
    if let Some(expected_deletion_id) = expected_deletion_id {
        crate::document_tombstone_store::require_document_tombstone_matches(
            graph_dir,
            &document.document_id,
            expected_deletion_id,
        )?;
    } else {
        crate::document_tombstone_store::require_document_not_tombstoned(
            graph_dir,
            &document.document_id,
        )?;
    }
    ensure_document_snapshot_for_revision_locked(graph_dir, document)
}

fn ensure_document_snapshot_for_revision_locked(
    graph_dir: &Path,
    document: &DocumentRecord,
) -> Result<LocalDocumentSnapshotMeta, String> {
    let mut store =
        read_document_history_store(graph_dir, &document.graph_id, &document.document_id)?;

    // Schema-v3 entries name the exact document revision. A marker is usable
    // only when the referenced payload still exists, decodes, and matches all
    // durable snapshot semantics. Check newest first so a repair supersedes a
    // damaged older entry for the same revision.
    let exact = store
        .snapshots
        .iter()
        .rev()
        .filter(|snapshot| {
            !snapshot.is_manual && snapshot.document_revision == Some(document.revision)
        })
        .cloned()
        .collect::<Vec<_>>();
    let mut exact_match = None;
    let mut invalid_exact_ids = Vec::new();
    for meta in exact {
        if try_snapshot_meta_matches_document(graph_dir, &meta, document).unwrap_or(false)
            && exact_match.is_none()
        {
            exact_match = Some(meta);
        } else {
            // An entry explicitly tagged with this revision but carrying any
            // other payload is corrupt. Remove its metadata first; payload
            // cleanup happens only after the replacement index commits.
            invalid_exact_ids.push(meta.snapshot_id);
        }
    }
    if !invalid_exact_ids.is_empty() {
        store.snapshots.retain(|snapshot| {
            !invalid_exact_ids
                .iter()
                .any(|snapshot_id| snapshot_id == &snapshot.snapshot_id)
        });
        store.total_count = store
            .total_count
            .saturating_sub(invalid_exact_ids.len() as u64);
        if store
            .latest_automatic_snapshot_id
            .as_ref()
            .is_some_and(|snapshot_id| invalid_exact_ids.contains(snapshot_id))
        {
            store.latest_automatic_revision = None;
            store.latest_automatic_snapshot_id = None;
        }
    }
    if let Some(meta) = exact_match {
        let marker_changed = store.latest_automatic_revision != Some(document.revision)
            || store.latest_automatic_snapshot_id.as_deref() != Some(meta.snapshot_id.as_str());
        if marker_changed || !invalid_exact_ids.is_empty() {
            store.latest_automatic_revision = Some(document.revision);
            store.latest_automatic_snapshot_id = Some(meta.snapshot_id.clone());
            write_document_history_store(graph_dir, &store)?;
            for snapshot_id in invalid_exact_ids {
                remove_snapshot_payload(graph_dir, &document.document_id, &snapshot_id);
            }
        }
        return Ok(meta);
    }
    if !invalid_exact_ids.is_empty() {
        write_document_history_store(graph_dir, &store)?;
        for snapshot_id in invalid_exact_ids {
            remove_snapshot_payload(graph_dir, &document.document_id, &snapshot_id);
        }
    }

    // Schema-v2 stores recorded a revision but not the snapshot id/revision on
    // metadata. Adopt their referenced newest automatic snapshot only after
    // validating its payload. No timestamp ordering is accepted as coverage.
    if store.latest_automatic_revision == Some(document.revision) {
        let legacy_snapshot_id = store.latest_automatic_snapshot_id.clone().or_else(|| {
            store
                .snapshots
                .iter()
                .rev()
                .filter(|snapshot| !snapshot.is_manual)
                .next()
                .map(|snapshot| snapshot.snapshot_id.clone())
        });
        if let Some(snapshot_id) = legacy_snapshot_id {
            if let Some(position) = store
                .snapshots
                .iter()
                .position(|snapshot| snapshot.snapshot_id == snapshot_id)
            {
                let candidate = store.snapshots[position].clone();
                match try_snapshot_meta_matches_document(graph_dir, &candidate, document) {
                    Ok(true) => {
                        store.snapshots[position].document_revision = Some(document.revision);
                        store.latest_automatic_snapshot_id = Some(snapshot_id);
                        let adopted = store.snapshots[position].clone();
                        write_document_history_store(graph_dir, &store)?;
                        return Ok(adopted);
                    }
                    Ok(false) => {
                        // A valid older payload can remain in history, but it
                        // cannot cover the claimed current revision.
                    }
                    Err(_) => {
                        // Missing/undecodable referenced payload: commit the
                        // repaired index before deleting any corrupt file.
                        store.snapshots.remove(position);
                        store.total_count = store.total_count.saturating_sub(1);
                        store.latest_automatic_revision = None;
                        store.latest_automatic_snapshot_id = None;
                        write_document_history_store(graph_dir, &store)?;
                        remove_snapshot_payload(graph_dir, &document.document_id, &snapshot_id);
                    }
                }
            }
        }
    }

    // Schema-v1 has no exact revision claim. Capturing once is safer than
    // silently assigning an arbitrary old snapshot to the current record.

    capture_document_snapshot_locked(graph_dir, document, false, None)
}

pub(super) fn committed_snapshot_for_document(
    graph_dir: &Path,
    document: &DocumentRecord,
    snapshot_id: &str,
) -> Result<Option<LocalDocumentSnapshotMeta>, String> {
    let store = read_document_history_store(graph_dir, &document.graph_id, &document.document_id)?;
    let Some(meta) = store
        .snapshots
        .iter()
        .find(|snapshot| {
            snapshot.snapshot_id == snapshot_id
                && !snapshot.is_manual
                && snapshot.document_revision == Some(document.revision)
        })
        .cloned()
    else {
        return Ok(None);
    };
    if store.latest_automatic_revision != Some(document.revision)
        || store.latest_automatic_snapshot_id.as_deref() != Some(snapshot_id)
        || !snapshot_meta_matches_document(graph_dir, &meta, document)
    {
        return Ok(None);
    }
    Ok(Some(meta))
}

pub(super) fn current_document_snapshot(
    app: &AppHandle,
    graph_id: &str,
    doc_id: &str,
    is_manual: bool,
    label: Option<String>,
) -> Result<LocalDocumentSnapshotMeta, String> {
    let _lease =
        crate::crdt_engine::persistence_coordinator::acquire_hot_write_blocking_if_managed(
            app, graph_id,
        )?;
    current_document_snapshot_with_lease(app, graph_id, doc_id, is_manual, label)
}

pub(crate) fn current_document_snapshot_with_lease(
    app: &AppHandle,
    graph_id: &str,
    doc_id: &str,
    is_manual: bool,
    label: Option<String>,
) -> Result<LocalDocumentSnapshotMeta, String> {
    let graph_dir = existing_graph_dir(app, graph_id)?;
    // Cold read: snapshot capture consumes only the projected content fields
    // (title / blocks / tiptap_xml via `document_blocks_for_read` and
    // `document_xml`), never the Y.Doc update payload. With the hydrating
    // read, the time-travel interval scheduler's per-graph capture loop paid
    // O(full rewrite history) per document, every 30 minutes, on every
    // active graph — the warm-cell detonation's biggest transient.
    let document = crate::document_service::read_document_cold_with_lease(
        app.clone(),
        graph_id.to_string(),
        doc_id.to_string(),
    )?;
    capture_document_snapshot(&graph_dir, &document, is_manual, label)
}

pub(super) fn copy_document_snapshot_as_manual(
    app: &AppHandle,
    graph_id: &str,
    doc_id: &str,
    snapshot_id: &str,
    label: Option<String>,
) -> Result<LocalDocumentSnapshotMeta, String> {
    let _lease =
        crate::crdt_engine::persistence_coordinator::acquire_hot_write_blocking_if_managed(
            app, graph_id,
        )?;
    copy_document_snapshot_as_manual_with_lease(app, graph_id, doc_id, snapshot_id, label)
}

fn copy_document_snapshot_as_manual_with_lease(
    app: &AppHandle,
    graph_id: &str,
    doc_id: &str,
    snapshot_id: &str,
    label: Option<String>,
) -> Result<LocalDocumentSnapshotMeta, String> {
    let graph_dir = existing_graph_dir(app, graph_id)?;
    let _ = existing_document_dir(&graph_dir, doc_id)?;
    let _durability_guard = crate::cell_durability::write_guard();
    let _history_guard = acquire_document_history_mutation_lock(&graph_dir, doc_id)?;
    crate::document_tombstone_store::require_document_not_tombstoned(&graph_dir, doc_id)?;
    let mut store = read_document_history_store(&graph_dir, graph_id, doc_id)?;
    let source_revision = store
        .snapshots
        .iter()
        .find(|snapshot| snapshot.snapshot_id == snapshot_id)
        .and_then(|snapshot| snapshot.document_revision);
    let source = read_document_snapshot_payload(&graph_dir, doc_id, snapshot_id)?;
    let created_at = timestamp();
    let new_snapshot_id = format!("snapshot-{created_at}-{}", Uuid::new_v4());
    let meta = LocalDocumentSnapshotMeta {
        snapshot_id: new_snapshot_id.clone(),
        graph_id: graph_id.to_string(),
        document_id: doc_id.to_string(),
        is_manual: true,
        document_revision: source_revision,
        tier: "20min".to_string(),
        snapshot_count: 1,
        chars_added: 0,
        chars_removed: 0,
        blocks_added: 0,
        blocks_removed: 0,
        blocks_modified: 0,
        created_at: created_at.clone(),
        label: label.filter(|value| !value.trim().is_empty()),
        opened_at: None,
    };
    let payload = LocalDocumentSnapshotPayload {
        snapshot_id: new_snapshot_id.clone(),
        graph_id: graph_id.to_string(),
        document_id: doc_id.to_string(),
        title: source.title,
        created_at,
        blocks: source.blocks,
        tiptap_xml: source.tiptap_xml,
    };

    create_dir_all(&document_history_snapshots_dir(&graph_dir, doc_id))?;
    write_json(
        &document_snapshot_payload_path(&graph_dir, doc_id, &new_snapshot_id),
        &payload,
    )?;
    store.total_count = store.total_count.saturating_add(1);
    store.snapshots.push(meta.clone());
    let obsolete_payloads = collapse_document_history(&mut store);
    write_document_history_store(&graph_dir, &store)?;
    for obsolete_snapshot_id in obsolete_payloads {
        remove_snapshot_payload(&graph_dir, doc_id, &obsolete_snapshot_id);
    }
    Ok(meta)
}

pub(super) fn delete_manual_document_snapshot(
    app: &AppHandle,
    graph_id: &str,
    doc_id: &str,
    snapshot_id: &str,
) -> Result<(), String> {
    let _lease = crate::crdt_engine::persistence_coordinator::
        acquire_lifecycle_exclusive_blocking_if_managed(app, graph_id)?;
    delete_manual_document_snapshot_with_lease(app, graph_id, doc_id, snapshot_id)
}

fn delete_manual_document_snapshot_with_lease(
    app: &AppHandle,
    graph_id: &str,
    doc_id: &str,
    snapshot_id: &str,
) -> Result<(), String> {
    let graph_dir = existing_graph_dir(app, graph_id)?;
    let _ = existing_document_dir(&graph_dir, doc_id)?;
    let _durability_guard = crate::cell_durability::write_guard();
    let _history_guard = acquire_document_history_mutation_lock(&graph_dir, doc_id)?;
    crate::document_tombstone_store::require_document_not_tombstoned(&graph_dir, doc_id)?;
    let mut store = read_document_history_store(&graph_dir, graph_id, doc_id)?;
    let Some(position) = store
        .snapshots
        .iter()
        .position(|snapshot| snapshot.snapshot_id == snapshot_id)
    else {
        return Err(format!("snapshot not found: {snapshot_id}"));
    };
    if !store.snapshots[position].is_manual {
        return Err("Only manually saved versions can be deleted".to_string());
    }
    store.snapshots.remove(position);
    store.total_count = store.total_count.saturating_sub(1);
    write_document_history_store(&graph_dir, &store)?;
    remove_snapshot_payload(&graph_dir, doc_id, snapshot_id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        document_history_file_store::fail_next_history_store_write,
        document_history_store::{
            document_history_store_path, LocalDocumentHistoryStore, HISTORY_STORE_SCHEMA_VERSION,
        },
        paths::documents_dir,
        rdf::document_subject,
        runtime_config::{DOCUMENT_SCHEMA_VERSION, LOCAL_GRAPH_ORIGIN, LOCAL_PROVIDER_ID},
        storage::{display_path, read_json},
    };
    #[cfg(feature = "headless")]
    use crate::{
        document_record_store::{read_document_record, write_document_record},
        document_service::{create_document, CreateDocumentInput},
        graph_service::{create_graph_service, CreateGraphInput},
    };
    use std::fs;
    #[cfg(feature = "headless")]
    use std::{collections::HashSet, sync::mpsc, time::Duration};
    #[cfg(feature = "headless")]
    #[cfg(feature = "desktop")]
    use tauri::Manager;

    fn temp_graph_dir(prefix: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("{prefix}-{}", Uuid::new_v4()))
    }

    fn test_document(graph_dir: &Path, revision: u64) -> DocumentRecord {
        let document_id = "doc-a";
        DocumentRecord {
            document_id: document_id.to_string(),
            graph_id: "graph-a".to_string(),
            title: "Document A".to_string(),
            revision,
            body: String::new(),
            origin: LOCAL_GRAPH_ORIGIN.to_string(),
            provider_id: LOCAL_PROVIDER_ID.to_string(),
            local_path: display_path(&documents_dir(graph_dir).join(document_id)),
            rdf_subject: document_subject(document_id),
            created_at: "1000".to_string(),
            updated_at: "1000".to_string(),
            capabilities: Vec::new(),
            schema_version: DOCUMENT_SCHEMA_VERSION,
            tiptap_xml: "<doc></doc>".to_string(),
            tiptap_json: None,
            ydoc_update_base64: String::new(),
            ydoc_state_path: String::new(),
            tree: None,
            blocks: Vec::new(),
            rdf_triple_count: 0,
            chronicle: Default::default(),
        }
    }

    fn snapshot_meta(snapshot_id: &str, tier: &str, created_at: &str) -> LocalDocumentSnapshotMeta {
        LocalDocumentSnapshotMeta {
            snapshot_id: snapshot_id.to_string(),
            graph_id: "graph-a".to_string(),
            document_id: "doc-a".to_string(),
            is_manual: false,
            document_revision: None,
            tier: tier.to_string(),
            snapshot_count: 1,
            chars_added: 0,
            chars_removed: 0,
            blocks_added: 0,
            blocks_removed: 0,
            blocks_modified: 0,
            created_at: created_at.to_string(),
            label: None,
            opened_at: None,
        }
    }

    fn matching_payload(
        snapshot_id: &str,
        document: &DocumentRecord,
    ) -> LocalDocumentSnapshotPayload {
        LocalDocumentSnapshotPayload {
            snapshot_id: snapshot_id.to_string(),
            graph_id: document.graph_id.clone(),
            document_id: document.document_id.clone(),
            title: document.title.clone(),
            created_at: "1000".to_string(),
            blocks: document_blocks_for_read(document),
            tiptap_xml: document_xml(document),
        }
    }

    #[test]
    fn legacy_revision_marker_is_adopted_only_after_payload_validation() {
        for mode in ["valid", "missing", "corrupt"] {
            let graph_dir = temp_graph_dir(&format!("garden-history-legacy-{mode}"));
            let document = test_document(&graph_dir, 7);
            let snapshot_id = "legacy-snapshot";
            let history_dir = crate::document_history_store::document_history_dir(
                &graph_dir,
                &document.document_id,
            );
            fs::create_dir_all(&history_dir).expect("history dir");
            write_json(
                &document_history_store_path(&graph_dir, &document.document_id),
                &serde_json::json!({
                    "schemaVersion": 2,
                    "graphId": document.graph_id,
                    "documentId": document.document_id,
                    "totalCount": 1,
                    "latestAutomaticRevision": 7,
                    "snapshots": [snapshot_meta(snapshot_id, "20min", "1000")],
                }),
            )
            .expect("legacy history store");
            let payload_path =
                document_snapshot_payload_path(&graph_dir, &document.document_id, snapshot_id);
            fs::create_dir_all(payload_path.parent().expect("payload parent"))
                .expect("snapshot dir");
            match mode {
                "valid" => write_json(&payload_path, &matching_payload(snapshot_id, &document))
                    .expect("valid legacy payload"),
                "corrupt" => {
                    fs::write(&payload_path, b"{not-json").expect("corrupt legacy payload")
                }
                "missing" => {}
                _ => unreachable!(),
            }

            let ensured = ensure_document_snapshot_for_revision(&graph_dir, &document)
                .expect("ensure exact snapshot");
            let store =
                read_document_history_store(&graph_dir, &document.graph_id, &document.document_id)
                    .expect("repaired history store");
            assert_eq!(ensured.document_revision, Some(7));
            assert_eq!(store.latest_automatic_revision, Some(7));
            assert_eq!(
                store.latest_automatic_snapshot_id.as_deref(),
                Some(ensured.snapshot_id.as_str())
            );
            if mode == "valid" {
                assert_eq!(ensured.snapshot_id, snapshot_id);
                assert_eq!(store.total_count, 1);
            } else {
                assert_ne!(ensured.snapshot_id, snapshot_id);
                assert_eq!(store.total_count, 1);
                assert!(store
                    .snapshots
                    .iter()
                    .all(|snapshot| snapshot.snapshot_id != snapshot_id));
                let repaired = read_document_snapshot_payload(
                    &graph_dir,
                    &document.document_id,
                    &ensured.snapshot_id,
                )
                .expect("replacement payload");
                assert!(snapshot_payload_matches_document(&repaired, &document)
                    .expect("compare repaired payload"));
            }
            fs::remove_dir_all(graph_dir).expect("remove graph dir");
        }
    }

    #[test]
    fn failed_history_store_commit_keeps_payloads_referenced_by_old_index() {
        let graph_dir = temp_graph_dir("garden-history-collapse-store-failure");
        let document = test_document(&graph_dir, 5);
        let mut snapshots = vec![snapshot_meta("anchor", "2h", "900")];
        snapshots
            .extend((0..4).map(|index| snapshot_meta(&format!("auto-{index}"), "20min", "1000")));
        let store = LocalDocumentHistoryStore {
            schema_version: HISTORY_STORE_SCHEMA_VERSION,
            graph_id: document.graph_id.clone(),
            document_id: document.document_id.clone(),
            total_count: snapshots.len() as u64,
            latest_automatic_revision: Some(4),
            latest_automatic_snapshot_id: Some("auto-3".to_string()),
            snapshots,
        };
        for snapshot in &store.snapshots {
            let path = document_snapshot_payload_path(
                &graph_dir,
                &document.document_id,
                &snapshot.snapshot_id,
            );
            fs::create_dir_all(path.parent().expect("payload parent")).expect("snapshot dir");
            write_json(&path, &matching_payload(&snapshot.snapshot_id, &document))
                .expect("snapshot payload");
        }
        write_document_history_store(&graph_dir, &store).expect("initial history store");
        let victim_path =
            document_snapshot_payload_path(&graph_dir, &document.document_id, "auto-0");
        fail_next_history_store_write(document_history_store_path(
            &graph_dir,
            &document.document_id,
        ));

        let error = capture_document_snapshot(&graph_dir, &document, false, None)
            .expect_err("injected history index failure");
        assert!(
            error.contains("injected history store write failure"),
            "{error}"
        );
        let persisted = read_json::<LocalDocumentHistoryStore>(&document_history_store_path(
            &graph_dir,
            &document.document_id,
        ))
        .expect("old history index remains committed");
        assert!(persisted
            .snapshots
            .iter()
            .any(|snapshot| snapshot.snapshot_id == "auto-0"));
        assert!(
            victim_path.is_file(),
            "payload garbage collection must run only after history.json commits"
        );
        fs::remove_dir_all(graph_dir).expect("remove graph dir");
    }

    #[cfg(feature = "headless")]
    #[test]
    fn concurrent_automatic_manual_copy_and_delete_preserve_one_history_index() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile = temp_graph_dir("garden-history-concurrent-rmw");
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let result = std::panic::catch_unwind(|| {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let graph_id = "history-concurrent-rmw";
            let document_id = "history-document";
            create_graph_service(
                &app,
                CreateGraphInput {
                    title: "History Concurrent RMW".to_string(),
                    graph_id: Some(graph_id.to_string()),
                    description: None,
                    operation_id: None,
                },
            )
            .expect("create graph");
            create_document(
                app.clone(),
                CreateDocumentInput {
                    graph_id: graph_id.to_string(),
                    title: "History Document".to_string(),
                    document_id: Some(document_id.to_string()),
                },
            )
            .expect("create document with revision-zero automatic history");
            let graph_dir = existing_graph_dir(&app, graph_id).expect("graph dir");
            let source = current_document_snapshot(
                &app,
                graph_id,
                document_id,
                true,
                Some("copy-source".to_string()),
            )
            .expect("manual copy source");
            let deleted = current_document_snapshot(
                &app,
                graph_id,
                document_id,
                true,
                Some("delete-target".to_string()),
            )
            .expect("manual delete target");

            let manifest = documents_dir(&graph_dir)
                .join(document_id)
                .join("document.json");
            let mut revision_one =
                read_document_record(&graph_dir, &manifest).expect("revision-zero document");
            revision_one.revision = 1;
            revision_one.body = "revision one".to_string();
            revision_one.updated_at = timestamp();
            write_document_record(&graph_dir, &revision_one)
                .expect("write revision one before automatic tail");

            // Hold the exact production lock, start every RMW, and wait until
            // all four have attempted that lock before releasing it. Without
            // serialization they would all read the same history.json and
            // overwrite one another's index changes.
            let guard = acquire_document_history_mutation_lock(&graph_dir, document_id)
                .expect("hold document history lock");
            let lock_path =
                document_history_lock_path(&graph_dir, document_id).expect("history lock path");
            let (attempt_tx, attempt_rx) = mpsc::channel();
            install_history_lock_attempt_hook(lock_path.clone(), attempt_tx);

            let automatic_graph_dir = graph_dir.clone();
            let automatic_document = revision_one.clone();
            let automatic = std::thread::spawn(move || {
                ensure_document_snapshot_for_revision(&automatic_graph_dir, &automatic_document)
                    .map(|snapshot| snapshot.snapshot_id)
            });

            let manual_app = app.clone();
            let manual = std::thread::spawn(move || {
                current_document_snapshot_with_lease(
                    &manual_app,
                    graph_id,
                    document_id,
                    true,
                    Some("concurrent-manual".to_string()),
                )
                .map(|snapshot| snapshot.snapshot_id)
            });

            let copy_app = app.clone();
            let source_id = source.snapshot_id.clone();
            let copy = std::thread::spawn(move || {
                copy_document_snapshot_as_manual_with_lease(
                    &copy_app,
                    graph_id,
                    document_id,
                    &source_id,
                    Some("concurrent-copy".to_string()),
                )
                .map(|snapshot| snapshot.snapshot_id)
            });

            let delete_app = app.clone();
            let deleted_id = deleted.snapshot_id.clone();
            let delete = std::thread::spawn(move || {
                delete_manual_document_snapshot_with_lease(
                    &delete_app,
                    graph_id,
                    document_id,
                    &deleted_id,
                )
                .map(|()| deleted_id)
            });

            let mut attempt_error = None;
            for _ in 0..4 {
                match attempt_rx.recv_timeout(Duration::from_secs(5)) {
                    Ok(attempted_path) => assert_eq!(attempted_path, lock_path),
                    Err(error) => {
                        attempt_error = Some(format!("history mutation did not contend: {error}"));
                        break;
                    }
                }
            }
            clear_history_lock_attempt_hook();
            drop(guard);

            let automatic_id = automatic
                .join()
                .expect("automatic history thread")
                .expect("automatic history mutation");
            let manual_id = manual
                .join()
                .expect("manual history thread")
                .expect("manual history mutation");
            let copy_id = copy
                .join()
                .expect("copy history thread")
                .expect("copy history mutation");
            let deleted_id = delete
                .join()
                .expect("delete history thread")
                .expect("delete history mutation");
            if let Some(error) = attempt_error {
                panic!("{error}");
            }

            let store =
                read_document_history_store(&graph_dir, graph_id, document_id).expect("history");
            let indexed_ids = store
                .snapshots
                .iter()
                .map(|snapshot| snapshot.snapshot_id.clone())
                .collect::<HashSet<_>>();
            assert_eq!(store.total_count, 5);
            for expected in [
                source.snapshot_id.as_str(),
                automatic_id.as_str(),
                manual_id.as_str(),
                copy_id.as_str(),
            ] {
                assert!(
                    indexed_ids.contains(expected),
                    "lost history entry {expected}"
                );
            }
            assert!(!indexed_ids.contains(&deleted_id));

            let payload_ids = fs::read_dir(document_history_snapshots_dir(&graph_dir, document_id))
                .expect("snapshot payload directory")
                .map(|entry| {
                    entry
                        .expect("snapshot payload entry")
                        .path()
                        .file_stem()
                        .and_then(|stem| stem.to_str())
                        .expect("snapshot payload filename")
                        .to_string()
                })
                .collect::<HashSet<_>>();
            assert_eq!(payload_ids, indexed_ids, "payload/index sets diverged");
            for snapshot_id in indexed_ids {
                read_document_snapshot_payload(&graph_dir, document_id, &snapshot_id)
                    .unwrap_or_else(|error| panic!("referenced payload {snapshot_id}: {error}"));
            }
        });

        clear_history_lock_attempt_hook();
        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[cfg(feature = "headless")]
    #[test]
    fn manual_snapshot_waits_for_graph_lifecycle_lease() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile = temp_graph_dir("garden-history-graph-lease");
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);

        let result = std::panic::catch_unwind(|| {
            crate::app_runtime::async_runtime::block_on(async {
                let app = crate::tauri_runtime::build_mock_app_for_tests(true);
                let graph_id = "history-graph-lease";
                let document_id = "history-document";
                create_graph_service(
                    &app,
                    CreateGraphInput {
                        title: "History graph lease".to_string(),
                        graph_id: Some(graph_id.to_string()),
                        description: None,
                        operation_id: None,
                    },
                )
                .expect("create graph");
                create_document(
                    app.clone(),
                    CreateDocumentInput {
                        graph_id: graph_id.to_string(),
                        title: "History document".to_string(),
                        document_id: Some(document_id.to_string()),
                    },
                )
                .expect("create document");
                let coordinator = app.state::<
                    crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator,
                >();
                let lease = coordinator
                    .acquire_hot_write(graph_id)
                    .await
                    .expect("hold graph lease");
                let snapshot_app = app.clone();
                let (started_tx, started_rx) = mpsc::channel();
                let (done_tx, done_rx) = mpsc::channel();
                let snapshot = std::thread::spawn(move || {
                    started_tx.send(()).expect("snapshot started");
                    let result = current_document_snapshot(
                        &snapshot_app,
                        graph_id,
                        document_id,
                        true,
                        Some("leased snapshot".to_string()),
                    );
                    done_tx.send(result).expect("snapshot result");
                });
                started_rx
                    .recv_timeout(Duration::from_secs(1))
                    .expect("snapshot thread started");
                assert!(
                    done_rx.recv_timeout(Duration::from_millis(50)).is_err(),
                    "manual history mutation bypassed the held graph lease"
                );
                drop(lease);
                done_rx
                    .recv_timeout(Duration::from_secs(5))
                    .expect("snapshot resumed")
                    .expect("manual snapshot");
                snapshot.join().expect("snapshot thread");
            });
        });

        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = fs::remove_dir_all(&profile);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }
}

#[cfg(test)]
mod gate_tests {
    use super::{open_rolling_position, AUTO_SNAPSHOT_GATE_MILLIS};
    use crate::document_history_store::{LocalDocumentHistoryStore, LocalDocumentSnapshotMeta};

    fn entry(id: &str, created_at: u128, opened_at: Option<u128>, manual: bool, tier: &str) -> LocalDocumentSnapshotMeta {
        LocalDocumentSnapshotMeta {
            snapshot_id: id.to_string(),
            graph_id: "g".to_string(),
            document_id: "d".to_string(),
            is_manual: manual,
            document_revision: None,
            tier: tier.to_string(),
            snapshot_count: 1,
            chars_added: 0,
            chars_removed: 0,
            blocks_added: 0,
            blocks_removed: 0,
            blocks_modified: 0,
            created_at: created_at.to_string(),
            label: None,
            opened_at: opened_at.map(|value| value.to_string()),
        }
    }

    fn store(snapshots: Vec<LocalDocumentSnapshotMeta>) -> LocalDocumentHistoryStore {
        LocalDocumentHistoryStore {
            schema_version: 3,
            graph_id: "g".to_string(),
            document_id: "d".to_string(),
            total_count: snapshots.len() as u64,
            latest_automatic_revision: None,
            latest_automatic_snapshot_id: None,
            snapshots,
        }
    }

    const T: u128 = 1_792_000_000_000;

    #[test]
    fn a_save_inside_the_stretch_rolls_the_newest_entry() {
        // Opened at T, last rolled forward at T + 15 min: still open at T + 19 min.
        let history = store(vec![entry("a", T + 15 * 60_000, Some(T), false, "20min")]);
        assert_eq!(open_rolling_position(&history, T + 19 * 60_000), Some(0));
        // The stretch is measured from when it opened, not from the last roll.
        assert_eq!(open_rolling_position(&history, T + AUTO_SNAPSHOT_GATE_MILLIS), None);
    }

    #[test]
    fn an_entry_opened_in_the_future_never_rolls() {
        let ahead = store(vec![entry("a", T, Some(T + 24 * 60 * 60_000), false, "20min")]);
        assert_eq!(open_rolling_position(&ahead, T + 60_000), None);
        let legacy_ahead = store(vec![entry("a", T + 24 * 60 * 60_000, None, false, "20min")]);
        assert_eq!(open_rolling_position(&legacy_ahead, T + 60_000), None);
    }

    #[test]
    fn older_entries_without_opened_at_measure_from_created_at() {
        let history = store(vec![entry("a", T, None, false, "20min")]);
        assert_eq!(open_rolling_position(&history, T + 60_000), Some(0));
        assert_eq!(open_rolling_position(&history, T + AUTO_SNAPSHOT_GATE_MILLIS + 1), None);
    }

    #[test]
    fn a_manual_or_promoted_newest_entry_never_rolls() {
        let manual_last = store(vec![
            entry("a", T, Some(T), false, "20min"),
            entry("m", T + 60_000, None, true, "20min"),
        ]);
        assert_eq!(open_rolling_position(&manual_last, T + 2 * 60_000), None);
        let promoted = store(vec![entry("a", T, Some(T), false, "2h")]);
        assert_eq!(open_rolling_position(&promoted, T + 60_000), None);
        assert_eq!(open_rolling_position(&store(vec![]), T), None);
    }
}
