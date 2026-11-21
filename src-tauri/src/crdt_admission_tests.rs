use super::*;
use crate::crdt_engine::{executor::drain_queue, rooms::RoomRegistry};
use serde_json::json;

const GRAPH: &str = "guarded-admission";

fn with_app(test: impl FnOnce(&AppHandle)) {
    let _serial = crate::tauri_runtime::profile_env_serial()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let previous = std::env::var_os("GARDEN_PROFILE_DIR");
    let profile = tempfile::tempdir().unwrap();
    std::env::set_var("GARDEN_PROFILE_DIR", profile.path());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);
        crate::graph_service::create_graph_service(
            &app,
            crate::graph_service::CreateGraphInput {
                title: "Admission".into(),
                graph_id: Some(GRAPH.into()),
                description: None,
                operation_id: None,
            },
        )
        .unwrap();
        test(&app);
        app.state::<RoomRegistry>().evict_graph(GRAPH);
    }));
    match previous {
        Some(value) => std::env::set_var("GARDEN_PROFILE_DIR", value),
        None => std::env::remove_var("GARDEN_PROFILE_DIR"),
    }
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

fn input(id: &str) -> EnqueueCrdtOperationInput {
    EnqueueCrdtOperationInput {
        kind: "workspace.createFolder".into(),
        graph_id: GRAPH.into(),
        document_id: None,
        payload: json!({"folderId":id,"name":id,"operationId":format!("{GUARDED_ADMISSION_ID_PREFIX}{id}")}),
    }
}

fn folder_count(app: &AppHandle) -> usize {
    let path = crate::ydoc_paths::workspace_snapshot_path(
        &crate::graph_paths::existing_graph_dir(app, GRAPH).unwrap(),
    );
    let snapshot: serde_json::Value = crate::storage::read_json(&path).unwrap();
    snapshot["folders"].as_array().unwrap().len()
}

#[test]
fn refused_consent_is_not_journaled_or_marked_queued() {
    with_app(|app| {
        crate::app_runtime::async_runtime::block_on(async {
            let queued = Arc::new(AtomicBool::new(false));
            let result = admit_crdt_operation_inner(
                app.clone(),
                input("refused"),
                Some(queued.clone()),
                true,
                |op| {
                    assert!(op.payload[GRAPH_INCARNATION_PAYLOAD_KEY].is_string());
                    Err::<(), _>("consent revoked".to_string())
                },
            )
            .await;
            assert_eq!(
                result.err().unwrap(),
                CrdtAdmissionError::Refused("consent revoked".into())
            );
            assert!(!queued.load(Ordering::Acquire));
            assert!(recover_pending_crdt_operations(app).unwrap().is_empty());
            assert!(app
                .state::<CrdtOperationQueue>()
                .poll(None)
                .unwrap()
                .is_empty());
        })
    });
}

#[test]
fn consent_guard_is_released_before_waiting_for_executor() {
    with_app(|app| {
        crate::app_runtime::async_runtime::block_on(async {
            let consent = std::sync::Mutex::new(true);
            let future = admit_crdt_operation(app.clone(), input("guarded"), |op| {
                assert_eq!(op.operation_id, "garden-admit-guarded");
                let guard = consent.lock().unwrap();
                if !*guard {
                    return Err("revoked".into());
                }
                Ok(guard)
            });
            fn assert_send<T: Send>(_: &T) {}
            assert_send(&future);
            let admitted = future.await.expect("admitted");
            assert_eq!(admitted.operation_id(), "garden-admit-guarded");
            *consent
                .try_lock()
                .expect("no consent lock during completion wait") = false;
            admitted.wait().await.unwrap();
            assert_eq!(folder_count(app), 1);
        })
    });
}

#[test]
fn failed_append_after_write_is_uncertain_and_recovers_from_journal() {
    with_app(|app| {
        crate::app_runtime::async_runtime::block_on(async {
            let queued = Arc::new(AtomicBool::new(false));
            crate::crdt_operation_journal::fail_next_queued_append_after_write_for_test(
                "garden-admit-uncertain",
            );
            let error = admit_crdt_operation_inner(
                app.clone(),
                input("uncertain"),
                Some(queued.clone()),
                true,
                |_| Ok(()),
            )
            .await
            .err()
            .unwrap();
            assert!(
                matches!(&error, CrdtAdmissionError::Uncertain { operation_id, .. } if operation_id == "garden-admit-uncertain")
            );
            assert!(operation_may_still_run(&error.into_message()));
            assert!(
                !queued.load(Ordering::Acquire),
                "flush marker still means actual queue insertion"
            );
            assert_eq!(recover_pending_crdt_operations(app).unwrap().len(), 1);
            assert!(app
                .state::<CrdtOperationQueue>()
                .poll(None)
                .unwrap()
                .is_empty());
            // Fresh queue/registry, same durable profile: no retransmitted request.
            let recovered = crate::tauri_runtime::build_mock_app_for_tests(true);
            assert_eq!(recover_crdt_operations(recovered.clone()).unwrap(), 1);
            drain_queue(recovered.clone()).await;
            assert_eq!(folder_count(&recovered), 1);
            assert!(recover_pending_crdt_operations(&recovered)
                .unwrap()
                .is_empty());
            recovered.state::<RoomRegistry>().evict_graph(GRAPH);
        })
    });
}

#[test]
fn dropping_completion_wait_does_not_cancel_admitted_work() {
    with_app(|app| {
        crate::app_runtime::async_runtime::block_on(async {
            let queue = app.state::<CrdtOperationQueue>();
            let pause = queue.lock_drain().await;
            let admitted = admit_crdt_operation(app.clone(), input("detached"), |_| Ok(()))
                .await
                .unwrap();
            assert_eq!(recover_pending_crdt_operations(app).unwrap().len(), 1);
            drop(admitted);
            drop(pause);
            drain_queue(app.clone()).await;
            assert_eq!(folder_count(app), 1);
            assert!(recover_pending_crdt_operations(app).unwrap().is_empty());
            let audit =
                std::fs::read_to_string(crate::paths::loopback_audit_log_path(app).unwrap())
                    .unwrap();
            assert!(
                audit
                    .lines()
                    .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                    .any(|event| event["targetId"] == "garden-admit-detached"
                        && event["outcome"] == "admitted"),
                "dropping the wait must not erase admission audit evidence"
            );
        })
    });
}

#[test]
fn lifecycle_refusal_after_journaling_is_not_safe_input_cleanup() {
    with_app(|app| {
        crate::app_runtime::async_runtime::block_on(async {
            app.state::<Arc<crate::cell_lifecycle::CellLifecycle>>()
                .begin_draining();
            let error = admit_crdt_operation(app.clone(), input("draining"), |_| Ok(()))
                .await
                .err()
                .unwrap();
            assert!(matches!(error, CrdtAdmissionError::Uncertain { .. }));
            assert!(operation_may_still_run(&error.into_message()));
            assert_eq!(recover_pending_crdt_operations(app).unwrap().len(), 1);
            assert!(app
                .state::<CrdtOperationQueue>()
                .poll(None)
                .unwrap()
                .is_empty());
            let fresh = crate::tauri_runtime::build_mock_app_for_tests(true);
            recover_crdt_operations(fresh.clone()).unwrap();
            drain_queue(fresh.clone()).await;
            assert_eq!(folder_count(&fresh), 1);
            fresh.state::<RoomRegistry>().evict_graph(GRAPH);
        })
    });
}

#[test]
fn cancellation_during_graph_wait_cannot_journal_later() {
    with_app(|app| {
        crate::app_runtime::async_runtime::block_on(async {
            let coordinator = app.state::<crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator>();
            let lease = coordinator.acquire_hot_write(GRAPH).await.unwrap();
            let checked = AtomicBool::new(false);
            let pending =
                admit_crdt_operation(app.clone(), input("cancel-before-admission"), |_| {
                    checked.store(true, Ordering::Release);
                    Ok(())
                });
            assert!(tokio::time::timeout(Duration::from_millis(20), pending)
                .await
                .is_err());
            drop(lease);
            assert!(!checked.load(Ordering::Acquire));
            assert!(recover_pending_crdt_operations(app).unwrap().is_empty());
        })
    });
}

#[test]
fn consent_is_rechecked_after_waiting_for_graph_lease() {
    with_app(|app| {
        crate::app_runtime::async_runtime::block_on(async {
            let coordinator = app.state::<crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator>();
            let lease = coordinator.acquire_hot_write(GRAPH).await.unwrap();
            let consent = std::sync::Mutex::new(true);
            let pending = admit_crdt_operation(app.clone(), input("revoked-while-waiting"), |_| {
                let guard = consent.lock().unwrap();
                if !*guard {
                    return Err("revoked while waiting".into());
                }
                Ok(guard)
            });
            tokio::pin!(pending);
            tokio::select! {
                _ = &mut pending => panic!("admitted through held graph lease"),
                _ = tokio::time::sleep(Duration::from_millis(20)) => {},
            }
            *consent.lock().unwrap() = false;
            drop(lease);
            assert_eq!(
                pending.await.err().unwrap(),
                CrdtAdmissionError::Refused("revoked while waiting".into())
            );
            assert!(recover_pending_crdt_operations(app).unwrap().is_empty());
        })
    });
}

#[test]
fn duplicate_guarded_id_cannot_replace_payload_journal_or_original_waiter() {
    with_app(|app| {
        crate::app_runtime::async_runtime::block_on(async {
            let queue = app.state::<CrdtOperationQueue>();
            let pause = queue.lock_drain().await;
            let first = admit_crdt_operation(app.clone(), input("unique"), |_| Ok(()))
                .await
                .unwrap();
            let before = std::fs::read(
                crate::crdt_operation_journal::crdt_operation_journal_path(app).unwrap(),
            )
            .unwrap();
            for changed in [false, true] {
                let mut duplicate = input("unique");
                if changed {
                    duplicate.payload["folderId"] = json!("other-payload");
                }
                let error = admit_crdt_operation(app.clone(), duplicate, |_| Ok(()))
                    .await
                    .err()
                    .unwrap();
                assert!(matches!(error, CrdtAdmissionError::Existing { .. }));
            }
            let legacy_error = enqueue_crdt_operation(app.clone(), input("unique"))
                .await
                .unwrap_err();
            assert!(operation_may_still_run(&legacy_error));
            assert_eq!(
                std::fs::read(
                    crate::crdt_operation_journal::crdt_operation_journal_path(app).unwrap()
                )
                .unwrap(),
                before
            );
            let pending = recover_pending_crdt_operations(app).unwrap();
            assert_eq!(pending.len(), 1);
            assert_eq!(pending[0].payload["folderId"], "unique");
            drop(pause);
            first
                .wait()
                .await
                .expect("original waiter must not be displaced");
            assert_eq!(folder_count(app), 1);
            // Completion does not reopen the ID; late retries must reconcile.
            assert!(matches!(
                admit_crdt_operation(app.clone(), input("unique"), |_| Ok(()))
                    .await
                    .err()
                    .unwrap(),
                CrdtAdmissionError::Existing { .. }
            ));
        })
    });
}

#[test]
fn wal_only_admission_cannot_be_overwritten_before_recovery() {
    with_app(|app| {
        crate::app_runtime::async_runtime::block_on(async {
            crate::crdt_operation_journal::fail_next_queued_append_after_write_for_test(
                "garden-admit-wal-only",
            );
            assert!(matches!(
                admit_crdt_operation(app.clone(), input("wal-only"), |_| Ok(()))
                    .await
                    .err()
                    .unwrap(),
                CrdtAdmissionError::Uncertain { .. }
            ));
            let before = std::fs::read(
                crate::crdt_operation_journal::crdt_operation_journal_path(app).unwrap(),
            )
            .unwrap();
            let fresh = crate::tauri_runtime::build_mock_app_for_tests(true);
            let mut changed = input("wal-only");
            changed.payload["folderId"] = json!("wrong");
            assert!(matches!(
                admit_crdt_operation(fresh.clone(), changed, |_| Ok(()))
                    .await
                    .err()
                    .unwrap(),
                CrdtAdmissionError::Existing { .. }
            ));
            assert_eq!(
                std::fs::read(
                    crate::crdt_operation_journal::crdt_operation_journal_path(app).unwrap()
                )
                .unwrap(),
                before
            );
            assert_eq!(
                recover_pending_crdt_operations(&fresh).unwrap()[0].payload["folderId"],
                "wal-only"
            );
        })
    });
}

#[test]
fn global_identity_admission_is_atomic_across_graphs() {
    with_app(|app| {
        crate::graph_service::create_graph_service(
            app,
            crate::graph_service::CreateGraphInput {
                title: "Other".into(),
                graph_id: Some("other".into()),
                description: None,
                operation_id: None,
            },
        )
        .unwrap();
        let queue = app.state::<CrdtOperationQueue>();
        let pause = crate::app_runtime::async_runtime::block_on(queue.lock_drain());
        let a = input("cross-graph");
        let mut b = input("cross-graph");
        b.graph_id = "other".into();
        let barrier = std::sync::Barrier::new(2);
        // Separate OS threads, synchronized after distinct graph leases:
        // a serial poll of two ready futures would not exercise this race.
        let (a, b) = std::thread::scope(|scope| {
            let run = |input| {
                crate::app_runtime::async_runtime::block_on(admit_crdt_operation(
                    app.clone(),
                    input,
                    |_| {
                        barrier.wait();
                        Ok(())
                    },
                ))
            };
            let a = scope.spawn(move || run(a));
            let b = scope.spawn(move || run(b));
            (a.join().unwrap(), b.join().unwrap())
        });
        let accepted = match (a, b) {
            (Ok(accepted), Err(CrdtAdmissionError::Existing { .. }))
            | (Err(CrdtAdmissionError::Existing { .. }), Ok(accepted)) => accepted,
            _ => panic!("exactly one graph may own the admission ID"),
        };
        assert_eq!(recover_pending_crdt_operations(app).unwrap().len(), 1);
        drop(pause);
        crate::app_runtime::async_runtime::block_on(accepted.wait()).unwrap();
        app.state::<RoomRegistry>().evict_graph("other");
    });
}
