//! Boot-time care of the artifact store, per graph: recordless
//! `artifacts/{id}` directories are removed once they are old enough not
//! to be an attach in flight, and the borrowed backfill runs so an agent on
//! MCP sees legacy records without the UI listing first (owed from the step
//! 1 and 1.5 angel reviews).

use crate::app_runtime::AppHandle;
use crate::{
    artifact_storage::backfill_borrowed_records,
    graph_service::read_profile_graph_records,
    json_utils::json_string,
    paths::existing_graph_dir,
};
use std::time::{Duration, SystemTime};

/// An `artifacts/{id}` directory younger than this may be an attach whose
/// put is still queued; it is left alone.
pub(crate) const ORPHAN_GRACE: Duration = Duration::from_secs(60 * 60);
const BOOT_DELAY: Duration = Duration::from_secs(20);

pub(crate) fn start_artifact_maintenance(app: AppHandle) {
    crate::app_runtime::async_runtime::spawn(async move {
        tokio::time::sleep(BOOT_DELAY).await;
        let graphs = match read_profile_graph_records(&app, false) {
            Ok(graphs) => graphs,
            Err(error) => {
                log::warn!("artifact maintenance: could not list graphs: {}", error.message());
                return;
            }
        };
        for graph in graphs {
            if graph.status != "active" {
                continue;
            }
            let sweep_app = app.clone();
            let sweep_graph = graph.graph_id.clone();
            let sweep = crate::app_runtime::async_runtime::spawn_blocking(move || sweep_orphan_artifact_dirs(&sweep_app, &sweep_graph))
                .await.map_err(|e| e.to_string()).and_then(|result| result);
            match sweep {
                Ok(0) => {}
                Ok(removed) => log::info!(
                    "artifact maintenance: removed {removed} recordless artifact directories in {}",
                    graph.graph_id
                ),
                Err(error) => log::warn!(
                    "artifact maintenance: orphan sweep for {} failed: {error}",
                    graph.graph_id
                ),
            }
            match backfill_borrowed_records(app.clone(), &graph.graph_id).await {
                Ok(0) => {}
                Ok(put) => log::info!(
                    "artifact maintenance: {put} borrowed records backfilled in {}",
                    graph.graph_id
                ),
                Err(error) => log::warn!(
                    "artifact maintenance: backfill for {} failed: {error}",
                    graph.graph_id
                ),
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    });
}

/// Remove `artifacts/{id}` directories that no record claims and that are
/// older than the grace period. Returns how many were removed.
pub(crate) fn sweep_orphan_artifact_dirs(app: &AppHandle, graph_id: &str) -> Result<usize, String> {
    sweep_orphan_artifact_dirs_with_grace(app, graph_id, ORPHAN_GRACE)
}

pub(crate) fn sweep_orphan_artifact_dirs_with_grace(
    app: &AppHandle,
    graph_id: &str,
    grace: Duration,
) -> Result<usize, String> {
    use yrs::{types::AsPrelim, updates::decoder::Decode, Doc, In, Map, ReadTxn, Transact, Update};
    let _lease = crate::crdt_engine::persistence_coordinator::acquire_hot_write_blocking_if_managed(app, graph_id)?
        .ok_or("artifact sweep requires graph coordinator")?;
    let graph_dir = existing_graph_dir(app, graph_id)?;
    let artifacts_dir = graph_dir.join("artifacts");
    if !artifacts_dir.is_dir() {
        return Ok(0);
    }
    if !std::fs::symlink_metadata(&artifacts_dir).map_err(|e| e.to_string())?.is_dir() {
        return Err("artifact store is not a real directory".into());
    }
    // Cold snapshots can lag a committed room after projection failure.
    let bytes = std::fs::read(crate::ydoc_paths::workspace_ydoc_state_path(&graph_dir)).map_err(|e| e.to_string())?;
    let doc = Doc::new();
    doc.transact_mut().apply_update(Update::decode_v1(&bytes).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    let txn = doc.transact();
    if txn.has_missing_updates() { return Err("workspace ownership has missing updates".into()); }
    if let Some((_, root)) = txn.root_refs().find(|(name, _)| *name == "artifacts") {
        if !matches!(root.as_prelim(&txn), In::Map(_)) { return Err("invalid artifact ownership map".into()); }
    }
    let recorded: std::collections::HashSet<String> = txn.get_map("artifacts")
        .map(|map| map.keys(&txn).map(str::to_string).collect()).unwrap_or_default();
    if recorded.is_empty() {
        log::warn!("artifact maintenance: {graph_id} records no artifacts; sweep skipped");
        return Ok(0);
    }
    let pending: std::collections::HashSet<String> = crate::crdt_operation_journal::pending_crdt_operations_for_cleanup(app)?
        .into_iter().filter(|op| op.graph_id == graph_id)
        .filter_map(|op| op.document_id.or_else(|| json_string(op.payload.get("artifactId"))))
        .collect();
    let now = SystemTime::now();
    let mut removed = 0usize;
    for entry in std::fs::read_dir(&artifacts_dir).map_err(|error| format!("read artifacts dir: {error}"))? {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if !entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
            continue;
        }
        let Some(id) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if recorded.contains(id) || pending.contains(id) {
            continue;
        }
        let young = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .map(|age| age < grace)
            .unwrap_or(true);
        if young {
            continue;
        }
        let _flush_guard = crate::cell_durability::write_guard();
        if let Err(error) = std::fs::remove_dir_all(&path) {
            log::warn!("artifact maintenance: could not remove {}: {error}", path.display());
            continue;
        }
        removed += 1;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A recordless artifact directory past the grace period goes; a
    /// recorded one and a young one stay.
    #[test]
    fn the_sweep_removes_only_old_recordless_artifact_directories() {
        use std::io::Write;
        #[cfg(feature = "desktop")]
        use tauri::Manager;
        use crate::crdt_queue::{enqueue_crdt_operation, EnqueueCrdtOperationInput};
        use crate::graph_service::{create_graph_service, CreateGraphInput};
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let profile = std::env::temp_dir().join(format!("garden-sweep-{nanos}"));
        let previous_profile = std::env::var_os("GARDEN_PROFILE_DIR");
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);
        let graph_id = "sweep-src";
        create_graph_service(
            &app,
            CreateGraphInput {
                graph_id: Some(graph_id.to_string()),
                title: "Sweep".to_string(),
                description: None,
                operation_id: None,
            },
        )
        .expect("create graph");
        crate::app_runtime::async_runtime::block_on(enqueue_crdt_operation(
            app.clone(),
            EnqueueCrdtOperationInput {
                kind: "workspace.putArtifact".to_string(),
                graph_id: graph_id.to_string(),
                document_id: Some("artifact-kept".to_string()),
                payload: serde_json::json!({ "artifactId": "artifact-kept", "label": "kept.bin", "status": "ready" }),
            },
        ))
        .expect("put kept");
        let graph_dir = existing_graph_dir(&app, graph_id).unwrap();
        for id in ["artifact-kept", "artifact-orphan"] {
            std::fs::create_dir_all(graph_dir.join("artifacts").join(id).join("original")).unwrap();
        }
        // A stale cold projection must not conceal a committed hot owner.
        crate::storage::write_json(&crate::ydoc_paths::workspace_snapshot_path(&graph_dir),
            &serde_json::json!({"artifacts":[{"id":"artifact-stale"}]})).unwrap();
        // With the real grace, the orphan is too young to touch.
        assert_eq!(sweep_orphan_artifact_dirs(&app, graph_id).unwrap(), 0);
        // With no grace, the orphan goes and the recorded one stays.
        assert_eq!(
            sweep_orphan_artifact_dirs_with_grace(&app, graph_id, Duration::from_secs(0)).unwrap(),
            1
        );
        assert!(graph_dir.join("artifacts/artifact-kept").is_dir());
        assert!(!graph_dir.join("artifacts/artifact-orphan").exists());
        let uncertain = graph_dir.join("artifacts/artifact-uncertain");
        std::fs::create_dir_all(&uncertain).unwrap();
        let journal_path=crate::crdt_operation_journal::crdt_operation_journal_path(&app).unwrap();
        let original_journal=std::fs::read(&journal_path).unwrap();
        let mut operation=crate::crdt_queue::CrdtOperation {
            operation_id:"legacy-ambiguous".into(),kind:"workspace.putArtifact".into(),graph_id:graph_id.into(),
            document_id:Some("artifact-uncertain".into()),payload:serde_json::json!({"artifactId":"artifact-uncertain"}),enqueue_timestamp:"0".into(),
        };
        crate::crdt_operation_journal::record_crdt_operation_queued(&app,&operation).unwrap();
        crate::crdt_operation_journal::record_crdt_operation_queued(&app,&operation).unwrap();
        assert_eq!(crate::crdt_operation_journal::pending_crdt_operations_for_cleanup(&app).unwrap().len(),1);
        operation.document_id=Some("artifact-other".into());
        crate::crdt_operation_journal::record_crdt_operation_queued(&app,&operation).unwrap();
        assert!(sweep_orphan_artifact_dirs_with_grace(&app,graph_id,Duration::ZERO).unwrap_err().contains("conflicting queued"));
        assert!(uncertain.exists());
        std::fs::write(&journal_path,original_journal).unwrap();
        let mut journal = std::fs::OpenOptions::new().append(true)
            .open(journal_path).unwrap();
        writeln!(journal,"unreadable ownership evidence").unwrap();
        assert!(sweep_orphan_artifact_dirs_with_grace(&app,graph_id,Duration::ZERO).is_err());
        assert!(uncertain.exists(),"corrupt journal is not permission to delete");
        app.state::<crate::crdt_engine::rooms::RoomRegistry>().evict_graph(graph_id);
        let _ = std::fs::remove_dir_all(profile);
        match previous_profile { Some(p)=>std::env::set_var("GARDEN_PROFILE_DIR",p),None=>std::env::remove_var("GARDEN_PROFILE_DIR") }
    }
}
