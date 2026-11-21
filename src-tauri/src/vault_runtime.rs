//! Owner-process lifecycle for Door 1. Internal only: no selection/API surface.
use crate::{
    app_runtime::AppHandle,
    vault_capture::{Binding, CaptureStore},
    vault_capture_access::CaptureAccess,
    vault_import::PreparedAttachment,
};
use std::{
    collections::HashMap,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::Duration,
};
#[cfg(feature = "desktop")]
use tauri::Manager;

const MAX_HANDLES: usize = 1024;
const MAX_GRAPHS: usize = 4096;

#[derive(Default)]
pub(crate) struct VaultRuntime {
    store: OnceLock<Result<Arc<CaptureAccess>, String>>,
    started: AtomicBool,
    prepared: Mutex<HashMap<String, PreparedAttachment>>,
}

impl VaultRuntime {
    /// The profile lock must already be held. An unavailable capture subsystem
    /// does not prevent ordinary graph use; its error remains available to callers.
    pub(crate) fn initialize(&self, profile: &Path) -> Result<(), String> {
        self.store
            .get_or_init(|| CaptureStore::open(profile).map(|s| Arc::new(CaptureAccess::new(s))))
            .as_ref()
            .map(|_| ())
            .map_err(Clone::clone)
    }

    pub(crate) fn store(&self) -> Result<Arc<CaptureAccess>, String> {
        self.store
            .get()
            .ok_or("capture runtime not initialized")?
            .clone()
    }

    /// A future preview/job owner retains by opaque ID, not by leaking handles
    /// into a UI session. Taking it transfers responsibility to one admission.
    pub(crate) fn hold(&self, prepared: PreparedAttachment) -> Result<String, String> {
        let store = self.store()?;
        let store = store.lock().map_err(|_| "capture store poisoned")?;
        prepared.authorize(&store)?;
        let mut handles = self
            .prepared
            .lock()
            .map_err(|_| "prepared registry poisoned")?;
        if handles.len() >= MAX_HANDLES {
            return Err("too many prepared import handles".into());
        }
        let id = prepared.id().to_owned();
        if handles.contains_key(&id) {
            return Err("preparation already held".into());
        }
        handles.insert(id.clone(), prepared);
        Ok(id)
    }

    pub(crate) fn take(&self, id: &str, binding: &Binding) -> Result<PreparedAttachment, String> {
        let mut handles = self
            .prepared
            .lock()
            .map_err(|_| "prepared registry poisoned")?;
        let handle = handles.get(id).ok_or("unknown or retired preparation")?;
        if handle.binding() != binding {
            return Err("unknown or retired preparation".into());
        }
        Ok(handles.remove(id).expect("checked while locked"))
    }

    fn expire_handles(&self) -> Result<usize, String> {
        let store = self.store()?;
        let empty = self
            .prepared
            .lock()
            .map_err(|_| "prepared registry poisoned")?
            .is_empty();
        if empty && !store.read(CaptureStore::has_records)? {
            return Ok(0);
        }
        let mut store = store.lock().map_err(|_| "capture store poisoned")?;
        store.reap()?;
        let mut handles = self
            .prepared
            .lock()
            .map_err(|_| "prepared registry poisoned")?;
        let before = handles.len();
        let mut consent_by_scan = HashMap::new();
        handles.retain(|_, handle| {
            // Captures are immutable. Do the receipt/fingerprint read once per
            // scan, not once per attachment. One damaged entry conservatively
            // retires its siblings; final admission rechecks each entry.
            let live = *consent_by_scan
                .entry(handle.scan().to_owned())
                .or_insert_with(|| handle.authorize(&store).is_ok());
            if live {
                true
            } else {
                // Failed consent is not inferred consent. A retired handle
                // cannot admit; accepted WAL ownership survives dropping it.
                handle.retire();
                false
            }
        });
        Ok(before - handles.len())
    }

    pub(crate) fn revoke_graph(&self, graph: &str, incarnation: &str) -> Result<(), String> {
        let Some(Ok(store)) = self.store.get() else {
            return Ok(());
        };
        // A failed optimization probe selects full reconciliation, not an
        // early return that would leave the independent handle registry live.
        let needs_revoke = store
            .read(|s| s.has_unrevoked_grant(graph, incarnation))
            .unwrap_or(true);
        // Retire handles even if durable grant revocation fails. The graph's
        // canonical tombstone independently fences all final admissions.
        let result = if needs_revoke {
            store
                .lock()
                .and_then(|mut store| store.revoke_graph(graph, incarnation))
        } else {
            Ok(())
        };
        store.read(|_| {
            self.prepared
                .lock()
                .map_err(|_| "prepared registry poisoned")?
                .retain(|_, handle| {
                    let binding = handle.binding();
                    if binding.graph == graph && binding.incarnation == incarnation {
                        handle.retire();
                        false
                    } else {
                        true
                    }
                });
            Ok(())
        })?;
        result
    }
}

/// Called after journal recovery/drain, before exposing local services. This
/// function initializes only; managed state registration itself performs no I/O.
pub(crate) async fn initialize(app: &AppHandle) -> Result<(), String> {
    if cfg!(feature = "frontend-crdt") || !cfg!(any(target_os = "macos", target_os = "linux")) {
        return Ok(());
    }
    if app
        .state::<Arc<crate::cell_graph_boundary::CellGraphBoundary>>()
        .is_enabled()
    {
        return Ok(());
    }
    let runtime = app.state::<Arc<VaultRuntime>>().inner().clone();
    let profile = crate::profile_paths::profile_dir(app)?;
    let lifecycle = app
        .state::<Arc<crate::cell_lifecycle::CellLifecycle>>()
        .inner()
        .clone();
    let lease = lifecycle
        .begin_maintenance("vault-initialize")
        .map_err(|_| "cell is draining")?;
    crate::app_runtime::async_runtime::spawn_blocking(move || {
        let _lease = lease;
        runtime.initialize(&profile)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Sleeps without an activity lease. Maintenance prevents exit while running,
/// but does not reset the headless cell's idle clock or start after drain.
pub(crate) fn start(app: AppHandle) {
    let runtime = app.state::<Arc<VaultRuntime>>().inner().clone();
    if runtime.store().is_err() || runtime.started.swap(true, Ordering::AcqRel) {
        return;
    }
    crate::app_runtime::async_runtime::spawn(async move {
        let mut tick = 0u64;
        loop {
            let lifecycle = app
                .state::<Arc<crate::cell_lifecycle::CellLifecycle>>()
                .inner()
                .clone();
            let Ok(lease) = lifecycle.begin_maintenance("vault-maintenance") else {
                break;
            };
            let worker_app = app.clone();
            // The blocking root owns the activity lease across all nested
            // workers: dropping this task cannot release it during filesystem I/O.
            let result = crate::app_runtime::async_runtime::spawn_blocking(move || {
                let _lease = lease;
                crate::app_runtime::async_runtime::block_on(maintenance_once(
                    &worker_app,
                    tick % 5 == 0,
                ))
            })
            .await;
            if let Err(error) = result.map_err(|e| e.to_string()).and_then(|r| r) {
                log::warn!("Vault maintenance retained uncertain state: {error}");
            }
            tick = tick.wrapping_add(1);
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    });
}

async fn maintenance_once(app: &AppHandle, sweep_pending: bool) -> Result<(), String> {
    app.state::<Arc<crate::vault_ui::Runtime>>().expire()?;
    let runtime = app.state::<Arc<VaultRuntime>>().inner().clone();
    // This function runs within the blocking maintenance root. No capture
    // mutex is held while waiting on any graph lease.
    let retired = runtime.expire_handles()?;
    if !sweep_pending && retired == 0 {
        return Ok(());
    }
    let root = crate::profile_paths::graphs_dir(app)?;
    let mut ids = Vec::new();
    match std::fs::read_dir(&root) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.to_string()),
        Ok(entries) => {
            for entry in entries {
                if ids.len() >= MAX_GRAPHS {
                    return Err("vault graph inventory exceeds bound".into());
                }
                let entry = entry.map_err(|e| e.to_string())?;
                if !entry.file_type().map_err(|e| e.to_string())?.is_dir() {
                    continue;
                }
                let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                ids.push(id);
            }
        }
    }
    let boundary = app.state::<Arc<crate::cell_graph_boundary::CellGraphBoundary>>();
    for id in ids {
        if boundary.authorize_graph_id(&id).is_err() {
            continue;
        }
        if crate::ids::validate_local_id(&id, "graph_id").is_err() {
            continue;
        }
        let graph: crate::graph_record_store::GraphRecord =
            match crate::storage::read_json(&root.join(&id).join("graph.json")) {
                Ok(record) => record,
                Err(error) => {
                    log::warn!("Vault graph maintenance skipped unreadable record: {error}");
                    continue;
                }
            };
        if graph.graph_id != id {
            continue;
        }
        let Some(incarnation) = graph.incarnation_id else {
            continue;
        };
        if graph.status != "active" {
            runtime.revoke_graph(&id, &incarnation)?;
            continue;
        }
        let binding = Binding {
            principal: "internal-maintenance".into(),
            graph: id,
            incarnation,
        };
        match std::fs::read_dir(root.join(&binding.graph).join("import-pending")) {
            Ok(mut entries) => {
                if entries.next().is_none() {
                    continue;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            _ => {} // reconciliation reports errors; never infer emptiness
        }
        if let Err(error) = crate::vault_import::reconcile_preparations(app, &binding).await {
            log::warn!("Vault prepared bytes retained: {error}");
        }
    }
    Ok(())
}

#[cfg(all(test, feature = "headless", not(feature = "desktop")))]
mod tests {
    use super::*;
    use std::fs::File;
    const GRAPH: &str = "runtime-vault-test";

    fn with_runtime(test: impl FnOnce(&AppHandle, &Arc<VaultRuntime>, &Binding, &str, &str)) {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var_os("GARDEN_PROFILE_DIR");
        let profile = tempfile::tempdir().unwrap();
        let source = tempfile::tempdir().unwrap();
        std::fs::write(source.path().join("asset.bin"), b"runtime bytes").unwrap();
        std::env::set_var("GARDEN_PROFILE_DIR", profile.path());
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            crate::graph_service::create_graph_service(
                &app,
                crate::graph_service::CreateGraphInput {
                    graph_id: Some(GRAPH.into()),
                    title: "Runtime".into(),
                    description: None,
                    operation_id: None,
                },
            )
            .unwrap();
            let runtime = app.state::<Arc<VaultRuntime>>().inner().clone();
            crate::app_runtime::async_runtime::block_on(initialize(&app)).unwrap();
            let binding = Binding {
                principal: "owner-test".into(),
                graph: GRAPH.into(),
                incarnation: crate::graph_record_store::ensure_graph_incarnation(&app, GRAPH)
                    .unwrap(),
            };
            let store = runtime.store().unwrap();
            let mut capture = store.lock().unwrap();
            let grant = capture
                .grant_selected(
                    binding.clone(),
                    File::open(source.path()).unwrap(),
                    crate::vault_capture::SourceKind::Folder,
                    60_000,
                )
                .unwrap();
            let mut ticket = capture.begin(&grant, &binding).unwrap();
            let snapshot = ticket.capture();
            let receipt = capture.finish(ticket, &binding, snapshot).unwrap();
            drop(capture);
            let journal = crate::crdt_operation_journal::crdt_operation_journal_path(&app).unwrap();
            std::fs::create_dir_all(journal.parent().unwrap()).unwrap();
            std::fs::write(journal, "").unwrap();
            test(&app, &runtime, &binding, &receipt.scan_id, &grant);
            app.state::<crate::crdt_engine::rooms::RoomRegistry>()
                .evict_graph(GRAPH);
        }));
        match previous {
            Some(value) => std::env::set_var("GARDEN_PROFILE_DIR", value),
            None => std::env::remove_var("GARDEN_PROFILE_DIR"),
        }
        if let Err(error) = result {
            std::panic::resume_unwind(error);
        }
    }

    async fn prepare(
        app: &AppHandle,
        runtime: &VaultRuntime,
        binding: &Binding,
        scan: &str,
    ) -> PreparedAttachment {
        crate::vault_import::prepare_attachment(
            app,
            &runtime.store().unwrap(),
            binding,
            scan,
            0,
            "application/octet-stream",
            None,
        )
        .await
        .unwrap()
    }

    #[test]
    fn managed_store_is_initialized_once_and_preparation_take_is_bound() {
        with_runtime(|app, runtime, binding, scan, _| {
            crate::app_runtime::async_runtime::block_on(async {
                let first = runtime.store().unwrap();
                initialize(app).await.unwrap();
                assert!(Arc::ptr_eq(&first, &runtime.store().unwrap()));
                let prepared = prepare(app, runtime, binding, scan).await;
                let id = runtime.hold(prepared).unwrap();
                let mut wrong = binding.clone();
                wrong.principal = "someone-else".into();
                assert!(runtime.take(&id, &wrong).is_err());
                let taken = runtime.take(&id, binding).unwrap();
                assert!(runtime.take(&id, binding).is_err());
                fn requires_send<T: Send>(_: T) {}
                requires_send(crate::vault_import::admit_attachment(
                    app.clone(),
                    &first,
                    &taken,
                ));
                crate::vault_import::admit_attachment(app.clone(), &first, &taken)
                    .await
                    .unwrap()
                    .wait()
                    .await
                    .unwrap();
            })
        });
    }

    #[test]
    fn cancelled_preview_releases_handle_and_maintenance_reaps_bytes() {
        with_runtime(|app, runtime, binding, scan, _| {
            crate::app_runtime::async_runtime::block_on(async {
                let id = runtime
                    .hold(prepare(app, runtime, binding, scan).await)
                    .unwrap();
                let body = crate::profile_paths::graphs_dir(app)
                    .unwrap()
                    .join(GRAPH)
                    .join("import-pending")
                    .join(&id)
                    .join("body");
                runtime
                    .store()
                    .unwrap()
                    .lock()
                    .unwrap()
                    .cancel(scan, binding)
                    .unwrap();
                maintenance_once(app, true).await.unwrap();
                assert!(runtime.take(&id, binding).is_err());
                assert!(!body.exists());
            })
        });
    }

    #[test]
    fn expired_grant_releases_parked_handle_without_a_client_request() {
        with_runtime(|app, runtime, binding, scan, grant| {
            crate::app_runtime::async_runtime::block_on(async {
                let id = runtime
                    .hold(prepare(app, runtime, binding, scan).await)
                    .unwrap();
                let path = crate::profile_paths::profile_dir(app)
                    .unwrap()
                    .join("vault-capture")
                    .join(format!("{grant}.grant"));
                let mut value: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                value["expires"] = serde_json::json!(0);
                std::fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
                maintenance_once(app, true).await.unwrap();
                assert!(runtime.take(&id, binding).is_err());
            })
        });
    }

    #[test]
    fn canonical_graph_deletion_revokes_grant_and_retires_handle() {
        with_runtime(|app, runtime, binding, scan, _| {
            crate::app_runtime::async_runtime::block_on(async {
                let id = runtime
                    .hold(prepare(app, runtime, binding, scan).await)
                    .unwrap();
                crate::graph_service::soft_delete_graph_service(app, GRAPH.into(), false).unwrap();
                assert!(runtime.take(&id, binding).is_err());
                assert!(runtime
                    .store()
                    .unwrap()
                    .lock()
                    .unwrap()
                    .get_ready(scan, binding)
                    .is_err());
            })
        });
    }

    #[test]
    fn unrelated_corrupt_grant_cannot_skip_deleted_graph_handle_retirement() {
        with_runtime(|app, runtime, binding, scan, grant| {
            crate::app_runtime::async_runtime::block_on(async {
                let id = runtime
                    .hold(prepare(app, runtime, binding, scan).await)
                    .unwrap();
                // No unrevoked valid grant can short-circuit the read-only
                // probe, regardless of filesystem enumeration order.
                runtime
                    .store()
                    .unwrap()
                    .lock()
                    .unwrap()
                    .revoke(grant)
                    .unwrap();
                let bad = crate::profile_paths::profile_dir(app)
                    .unwrap()
                    .join("vault-capture")
                    .join(format!("{}.grant", uuid::Uuid::new_v4()));
                std::fs::write(&bad, b"not json").unwrap();
                crate::graph_service::soft_delete_graph_service(app, GRAPH.into(), false).unwrap();
                assert!(runtime.take(&id, binding).is_err());
                assert!(
                    !bad.exists(),
                    "full revocation quarantines corrupt evidence"
                );
            })
        });
    }

    #[test]
    fn capture_failure_is_sticky_and_reported_by_store_accessor() {
        let profile = tempfile::tempdir().unwrap();
        std::fs::write(profile.path().join("vault-capture"), b"not a directory").unwrap();
        let runtime = VaultRuntime::default();
        assert!(runtime.initialize(profile.path()).is_err());
        assert!(runtime.store().is_err());
        assert!(runtime.initialize(profile.path()).is_err());
    }

    #[test]
    fn initialization_refuses_new_work_after_drain() {
        with_runtime(|app, runtime, _, _, _| {
            crate::app_runtime::async_runtime::block_on(async {
                let lifecycle = app.state::<Arc<crate::cell_lifecycle::CellLifecycle>>();
                assert!(lifecycle.begin_draining());
                assert!(initialize(app).await.unwrap_err().contains("draining"));
                assert_eq!(lifecycle.snapshot().background_leases, 0);
                assert!(runtime.store().is_ok());
            })
        });
    }

    #[test]
    fn graph_bound_cell_does_not_open_a_profile_wide_capture_store() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let app = crate::tauri_runtime::build_mock_app_for_tests(false);
        crate::tauri_runtime::register_managed_state(
            &app,
            Arc::new(crate::cell_graph_boundary::CellGraphBoundary::for_test(
                Some(GRAPH),
            )),
        );
        crate::app_runtime::async_runtime::block_on(initialize(&app)).unwrap();
        assert!(app.state::<Arc<VaultRuntime>>().store().is_err());
    }
}
