use super::*;
use crate::{
    crdt_queue::{enqueue_crdt_operation, CrdtOperationQueue},
    vault_capture::SourceKind,
};

const GRAPH: &str = "capture-binding";

#[test]
fn quota_pressure_reclaims_abandoned_reservations_then_retries_once() {
    with_capture(|app, store, binding, scan| {
        crate::app_runtime::async_runtime::block_on(async {
            let graph = check_graph(app, binding).unwrap();
            let root = private_directory(&graph, "import-pending").unwrap();
            for _ in 0..8 {
                drop(
                    pending::reserve(&root, &uuid::Uuid::new_v4().to_string(), 256 * 1024 * 1024)
                        .unwrap(),
                );
            }
            let journal = crate::crdt_operation_journal::crdt_operation_journal_path(app).unwrap();
            std::fs::create_dir_all(journal.parent().unwrap()).unwrap();
            std::fs::write(journal, "").unwrap();
            let prepared = prepare(app, store, binding, scan).await;
            assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
            assert!(pending_path(&graph, &prepared.accepted).exists());
            discard_prepared(app, &prepared).await.unwrap();
        });
    });
}

#[test]
fn reconciliation_keeps_live_and_wal_owned_preparations() {
    with_capture(|app, store, binding, scan| {
        crate::app_runtime::async_runtime::block_on(async {
            let live = prepare(app, store, binding, scan).await;
            let orphan = prepare(app, store, binding, scan).await;
            let graph = check_graph(app, binding).unwrap();
            let orphan_path = pending_path(&graph, &orphan.accepted);
            drop(orphan);
            // Missing WAL cannot prove that the dropped preparation is unowned.
            let journal = crate::crdt_operation_journal::crdt_operation_journal_path(app).unwrap();
            std::fs::create_dir_all(journal.parent().unwrap()).unwrap();
            std::fs::write(&journal, "").unwrap();
            assert_eq!(reconcile_preparations(app, binding).await.unwrap(), 1);
            assert!(!orphan_path.exists());
            assert!(pending_path(&graph, &live.accepted).exists());

            let queue = app.state::<CrdtOperationQueue>();
            let paused = queue.lock_drain().await;
            let pending = pending_path(&graph, &live.accepted);
            let admitted = admit_attachment(app.clone(), store, &live).await.unwrap();
            let before = std::fs::read(&journal).unwrap();
            for ok in [false, true] {
                assert!(crate::crdt_queue::complete_crdt_operation(
                    app.clone(),
                    app.state::<CrdtOperationQueue>(),
                    crate::crdt_queue::CompleteCrdtOperationInput {
                        operation_id: admitted.operation_id().into(),
                        ok,
                        value: None,
                        error: None,
                        timing: None,
                    },
                )
                .unwrap_err()
                .contains("internal executor"));
            }
            assert_eq!(std::fs::read(&journal).unwrap(), before);
            drop(live);
            assert_eq!(reconcile_preparations(app, binding).await.unwrap(), 0);
            assert!(pending.exists());
            drop(paused);
            admitted.wait().await.unwrap();
            assert!(!pending.exists());
        });
    });
}

#[test]
fn unreadable_journal_retains_abandoned_preparations() {
    with_capture(|app, store, binding, scan| {
        crate::app_runtime::async_runtime::block_on(async {
            let prepared = prepare(app, store, binding, scan).await;
            let path = pending_path(&check_graph(app, binding).unwrap(), &prepared.accepted);
            drop(prepared);
            let journal = crate::crdt_operation_journal::crdt_operation_journal_path(app).unwrap();
            std::fs::create_dir_all(journal.parent().unwrap()).unwrap();
            std::fs::write(&journal, "broken journal\n").unwrap();
            assert!(reconcile_preparations(app, binding).await.is_err());
            assert!(path.exists());
            std::fs::remove_file(&journal).unwrap();
            assert!(reconcile_preparations(app, binding).await.is_err());
            assert!(path.exists());
        });
    });
}

#[test]
fn preparation_directory_must_be_new() {
    let root = tempfile::tempdir().unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    fresh_preparation_directory(root.path(), &id).unwrap();
    let body = root.path().join(&id).join("body");
    std::fs::write(&body, b"owned by first preparation").unwrap();
    assert!(fresh_preparation_directory(root.path(), &id).is_err());
    assert_eq!(std::fs::read(body).unwrap(), b"owned by first preparation");
}

fn with_capture(test: impl FnOnce(&AppHandle, &CaptureAccess, &Binding, &str)) {
    with_capture_file("asset.bin", b"captured bytes", test)
}

fn with_capture_file(
    name: &str,
    bytes: &[u8],
    test: impl FnOnce(&AppHandle, &CaptureAccess, &Binding, &str),
) {
    let _serial = crate::tauri_runtime::profile_env_serial()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let previous = std::env::var_os("GARDEN_PROFILE_DIR");
    let profile = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    std::fs::write(source.path().join(name), bytes).unwrap();
    std::env::set_var("GARDEN_PROFILE_DIR", profile.path());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);
        crate::graph_service::create_graph_service(
            &app,
            crate::graph_service::CreateGraphInput {
                graph_id: Some(GRAPH.into()),
                title: "Binding".into(),
                description: None,
                operation_id: None,
            },
        )
        .unwrap();
        let binding = Binding {
            principal: "owner-test".into(),
            graph: GRAPH.into(),
            incarnation: crate::graph_record_store::ensure_graph_incarnation(&app, GRAPH).unwrap(),
        };
        let mut store = CaptureStore::open(profile.path()).unwrap();
        let grant = store
            .grant_selected(
                binding.clone(),
                File::open(source.path()).unwrap(),
                SourceKind::Folder,
                60_000,
            )
            .unwrap();
        let mut ticket = store.begin(&grant, &binding).unwrap();
        let snapshot = ticket.capture();
        let receipt = store.finish(ticket, &binding, snapshot).unwrap();
        let store = CaptureAccess::new(store);
        test(&app, &store, &binding, &receipt.scan_id);
        app.state::<crate::crdt_engine::rooms::RoomRegistry>()
            .evict_graph(GRAPH);
    }));
    match previous {
        Some(p) => std::env::set_var("GARDEN_PROFILE_DIR", p),
        None => std::env::remove_var("GARDEN_PROFILE_DIR"),
    }
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

#[test]
fn reserved_manifest_filename_preserves_body_and_storage_metadata() {
    with_capture_file(
        "MANIFEST.JSON",
        b"{\"keep\":true}",
        |app, store, binding, scan| {
            crate::app_runtime::async_runtime::block_on(async {
                let prepared = prepare(app, store, binding, scan).await;
                admit_attachment(app.clone(), store, &prepared)
                    .await
                    .unwrap()
                    .wait()
                    .await
                    .unwrap();
                let original = crate::paths::artifact_original_dir(
                    &check_graph(app, binding).unwrap(),
                    &prepared.accepted.artifact_id(),
                )
                .unwrap();
                let manifest =
                    crate::original_file_manifest_store::read_original_manifest(&original).unwrap();
                assert_eq!(manifest.filename, "source-manifest.json");
                assert_eq!(
                    std::fs::read(original.join(&manifest.filename)).unwrap(),
                    b"{\"keep\":true}"
                );
                let record = crate::artifact_storage::artifact_record(
                    app,
                    GRAPH,
                    &prepared.accepted.artifact_id(),
                )
                .unwrap()
                .unwrap();
                assert_eq!(record["sourceFile"]["sf_originalFilename"], "MANIFEST.JSON");
                assert_eq!(record["name"], "MANIFEST.JSON");
            })
        },
    );
}

async fn prepare(
    app: &AppHandle,
    store: &CaptureAccess,
    binding: &Binding,
    scan: &str,
) -> PreparedAttachment {
    prepare_attachment(
        app,
        store,
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
fn accepted_bytes_survive_capture_cancellation_and_replay_never_resurrects() {
    with_capture(|app, store, binding, scan| {
        crate::app_runtime::async_runtime::block_on(async {
            let prepared = prepare(app, store, binding, scan).await;
            let not_admitted = prepare(app, store, binding, scan).await;
            let artifact_id = prepared.accepted.artifact_id();
            let graph_dir = check_graph(app, binding).unwrap();
            let pending = pending_path(&graph_dir, &prepared.accepted);
            let queue = app.state::<CrdtOperationQueue>();
            let paused = queue.lock_drain().await;
            let admitted = admit_attachment(app.clone(), store, &prepared)
                .await
                .unwrap();
            let operation = crate::crdt_operation_journal::recover_pending_crdt_operations(app)
                .unwrap()
                .into_iter()
                .find(|op| op.kind == KIND)
                .unwrap();
            store.lock().unwrap().cancel(scan, binding).unwrap();
            assert!(matches!(
                admit_attachment(app.clone(), store, &prepared)
                    .await
                    .err()
                    .unwrap(),
                CrdtAdmissionError::Refused(_)
            ));
            assert!(matches!(
                admit_attachment(app.clone(), store, &not_admitted)
                    .await
                    .err()
                    .unwrap(),
                CrdtAdmissionError::Refused(_)
            ));
            assert!(
                !pending_path(&graph_dir, &not_admitted.accepted).exists(),
                "a never-admitted copy is released independently"
            );
            assert!(
                pending.is_file(),
                "capture cleanup cannot touch admitted ownership"
            );
            drop(paused);
            let result = admitted.wait().await.unwrap();
            assert_eq!(result.value["artifactId"], artifact_id);
            let original = crate::paths::artifact_original_dir(&graph_dir, &artifact_id)
                .unwrap()
                .join("asset.bin");
            assert_eq!(std::fs::read(&original).unwrap(), b"captured bytes");
            assert!(
                !pending.exists(),
                "successful application releases only prepared bytes"
            );
            let record = crate::artifact_storage::artifact_record(app, GRAPH, &artifact_id)
                .unwrap()
                .unwrap();
            assert_eq!(record["mode"], "attached");
            assert_eq!(record["storage"], "owned");
            assert_eq!(record["source"]["kind"], "import");
            enqueue_crdt_operation(
                app.clone(),
                EnqueueCrdtOperationInput {
                    kind: "workspace.deleteArtifact".into(),
                    graph_id: GRAPH.into(),
                    document_id: Some(artifact_id.clone()),
                    payload: json!({}),
                },
            )
            .await
            .unwrap();
            crate::original_file_service::delete_artifact_original_files(app, GRAPH, &artifact_id)
                .unwrap();
            assert!(!original.exists());
            let replay = crate::crdt_engine::executor::apply_operation(app, &operation)
                .await
                .unwrap();
            assert_eq!(replay["replayed"], true);
            assert!(
                !original.exists(),
                "historical retry cannot republish deleted bytes"
            );
            assert!(
                crate::artifact_storage::artifact_record(app, GRAPH, &artifact_id)
                    .unwrap()
                    .is_none()
            );
        })
    });
}

#[test]
fn cancelled_or_wrong_principal_capture_cannot_be_admitted() {
    with_capture(|app, store, binding, scan| {
        crate::app_runtime::async_runtime::block_on(async {
            let mut wrong = binding.clone();
            wrong.principal = "other".into();
            assert!(prepare_attachment(
                app,
                store,
                &wrong,
                scan,
                0,
                "application/octet-stream",
                None
            )
            .await
            .is_err());
            let prepared = prepare(app, store, binding, scan).await;
            let pending = pending_path(&check_graph(app, binding).unwrap(), &prepared.accepted);
            store.lock().unwrap().cancel(scan, binding).unwrap();
            assert!(matches!(
                admit_attachment(app.clone(), store, &prepared)
                    .await
                    .err()
                    .unwrap(),
                CrdtAdmissionError::Refused(_)
            ));
            assert!(
                !crate::crdt_operation_journal::recover_pending_crdt_operations(app)
                    .unwrap()
                    .iter()
                    .any(|op| op.kind == KIND)
            );
            assert!(
                !pending.exists(),
                "definite refusal before custody transfer must release its unique prepared bytes"
            );
            assert!(!check_graph(app, binding)
                .unwrap()
                .join("artifacts")
                .join(prepared.accepted.artifact_id())
                .exists());
        })
    });
}

#[test]
fn repeat_preparation_is_frozen_and_pending_journal_protects_orphan_bytes() {
    with_capture(|app, store, binding, scan| {
        crate::app_runtime::async_runtime::block_on(async {
            enqueue_crdt_operation(
                app.clone(),
                EnqueueCrdtOperationInput {
                    kind: "workspace.putArtifact".into(),
                    graph_id: GRAPH.into(),
                    document_id: Some("artifact-kept".into()),
                    payload: json!({"artifactId":"artifact-kept","label":"Kept","status":"ready"}),
                },
            )
            .await
            .unwrap();
            let prepared = prepare(app, store, binding, scan).await;
            let again = prepare(app, store, binding, scan).await;
            assert_eq!(prepared.accepted.publication, again.accepted.publication);
            let graph_dir = check_graph(app, binding).unwrap();
            let original =
                crate::paths::artifact_original_dir(&graph_dir, &prepared.accepted.artifact_id())
                    .unwrap();
            std::fs::create_dir_all(&original).unwrap();
            let queue = app.state::<CrdtOperationQueue>();
            let paused = queue.lock_drain().await;
            let admitted = admit_attachment(app.clone(), store, &prepared)
                .await
                .unwrap();
            let sweep_app = app.clone();
            crate::app_runtime::async_runtime::spawn_blocking(move || {
                crate::artifact_maintenance::sweep_orphan_artifact_dirs_with_grace(
                    &sweep_app,
                    GRAPH,
                    std::time::Duration::ZERO,
                )
            })
            .await
            .unwrap()
            .unwrap();
            assert!(
                original.exists(),
                "pending operation protects bytes regardless of grace"
            );
            drop(paused);
            admitted.wait().await.unwrap();
            let replay = admit_attachment(app.clone(), store, &again)
                .await
                .unwrap()
                .wait()
                .await
                .unwrap();
            assert_eq!(replay.value["replayed"], true);
        })
    });
}

#[test]
fn corrupted_prepared_bytes_fail_without_creating_an_artifact() {
    with_capture(|app, store, binding, scan| {
        crate::app_runtime::async_runtime::block_on(async {
            let prepared = prepare(app, store, binding, scan).await;
            std::fs::write(
                pending_path(&check_graph(app, binding).unwrap(), &prepared.accepted),
                b"wrong bytes",
            )
            .unwrap();
            let error = admit_attachment(app.clone(), store, &prepared)
                .await
                .unwrap()
                .wait()
                .await
                .err()
                .unwrap();
            assert!(error.contains("capture") || error.contains("length"));
            assert!(crate::artifact_storage::artifact_record(
                app,
                GRAPH,
                &prepared.accepted.artifact_id()
            )
            .unwrap()
            .is_none());
        })
    });
}

#[test]
fn raw_clients_cannot_mint_internal_capture_operations() {
    assert!(crate::loopback_scopes::crdt_operation_scopes(KIND).is_none());
    with_capture(|app, store, binding, scan| {
        crate::app_runtime::async_runtime::block_on(async {
            let prepared = prepare(app, store, binding, scan).await;
            let request = EnqueueCrdtOperationInput {
                kind: KIND.into(),
                graph_id: GRAPH.into(),
                document_id: Some(prepared.accepted.artifact_id()),
                payload: json!({"capture":prepared.accepted,"graphIncarnation":binding.incarnation}),
            };
            assert!(enqueue_crdt_operation(app.clone(), request)
                .await
                .unwrap_err()
                .contains("internal guarded admission"));
            assert!(crate::artifact_storage::artifact_record(
                app,
                GRAPH,
                &prepared.accepted.artifact_id()
            )
            .unwrap()
            .is_none());
        })
    });
}

#[test]
fn collision_refuses_before_touching_an_unrelated_original_manifest() {
    with_capture(|app, store, binding, scan| {
        crate::app_runtime::async_runtime::block_on(async {
            let prepared = prepare(app, store, binding, scan).await;
            let id = prepared.accepted.artifact_id();
            let original =
                crate::paths::artifact_original_dir(&check_graph(app, binding).unwrap(), &id)
                    .unwrap();
            std::fs::create_dir_all(&original).unwrap();
            let foreign = original.join("foreign.bin");
            std::fs::write(&foreign, b"unrelated existing bytes").unwrap();
            crate::original_file_manifest_store::write_original_manifest_hashed(
                &original,
                &foreign,
                "foreign.bin".into(),
                "application/octet-stream",
                24,
                None,
            )
            .unwrap();
            let manifest = std::fs::read(original.join("manifest.json")).unwrap();
            enqueue_crdt_operation(app.clone(),EnqueueCrdtOperationInput {
            kind:"workspace.putArtifact".into(),graph_id:GRAPH.into(),document_id:Some(id.clone()),
            payload:json!({"artifactId":id,"label":"Foreign","originalFilename":"foreign.bin","mode":"attached","storage":"owned","status":"ready"}),
        }).await.unwrap();
            let error = admit_attachment(app.clone(), store, &prepared)
                .await
                .unwrap()
                .wait()
                .await
                .err()
                .unwrap();
            assert!(error.contains("already exists"));
            assert_eq!(
                std::fs::read(original.join("manifest.json")).unwrap(),
                manifest
            );
            assert_eq!(std::fs::read(foreign).unwrap(), b"unrelated existing bytes");
            assert!(!original.join("asset.bin").exists());
        })
    });
}

#[test]
fn missing_parent_refuses_before_final_file_publication() {
    with_capture(|app, store, binding, scan| {
        crate::app_runtime::async_runtime::block_on(async {
            let prepared = prepare_attachment(
                app,
                store,
                binding,
                scan,
                0,
                "application/octet-stream",
                Some("missing-folder".into()),
            )
            .await
            .unwrap();
            let error = admit_attachment(app.clone(), store, &prepared)
                .await
                .unwrap()
                .wait()
                .await
                .err()
                .unwrap();
            assert!(error.contains("parent folder"));
            assert!(!check_graph(app, binding)
                .unwrap()
                .join("artifacts")
                .join(prepared.accepted.artifact_id())
                .exists());
            assert!(
                prepare_attachment(app, store, binding, scan, 0, "bad\nmime", None)
                    .await
                    .is_err()
            );
        })
    });
}

#[test]
fn partial_original_manifest_mime_cannot_be_rewritten_by_another_plan() {
    with_capture(|app, store, binding, scan| {
        crate::app_runtime::async_runtime::block_on(async {
            let prepared = prepare(app, store, binding, scan).await;
            let original = crate::paths::artifact_original_dir(
                &check_graph(app, binding).unwrap(),
                &prepared.accepted.artifact_id(),
            )
            .unwrap();
            std::fs::create_dir_all(&original).unwrap();
            let body = original.join("asset.bin");
            std::fs::write(&body, b"captured bytes").unwrap();
            crate::original_file_manifest_store::write_original_manifest_hashed(
                &original,
                &body,
                "asset.bin".into(),
                "text/plain",
                b"captured bytes".len(),
                Some(prepared.accepted.sha().unwrap().into()),
            )
            .unwrap();
            let before = std::fs::read(original.join("manifest.json")).unwrap();
            let error = admit_attachment(app.clone(), store, &prepared)
                .await
                .unwrap()
                .wait()
                .await
                .err()
                .unwrap();
            assert!(error.contains("manifest conflicts"));
            assert_eq!(
                std::fs::read(original.join("manifest.json")).unwrap(),
                before
            );
        })
    });
}

#[test]
fn discarded_preparation_cannot_be_admitted_but_fresh_preparation_can() {
    with_capture(|app, store, binding, scan| {
        crate::app_runtime::async_runtime::block_on(async {
            let prepared = prepare(app, store, binding, scan).await;
            let path = pending_path(&check_graph(app, binding).unwrap(), &prepared.accepted);
            assert!(discard_prepared(app, &prepared).await.unwrap());
            assert!(!path.exists());
            assert!(matches!(
                admit_attachment(app.clone(), store, &prepared)
                    .await
                    .err()
                    .unwrap(),
                CrdtAdmissionError::Refused(_)
            ));
            assert!(
                crate::crdt_operation_journal::recover_pending_crdt_operations(app)
                    .unwrap()
                    .is_empty()
            );
            let fresh = prepare(app, store, binding, scan).await;
            assert_ne!(fresh.accepted.preparation, prepared.accepted.preparation);
            admit_attachment(app.clone(), store, &fresh)
                .await
                .unwrap()
                .wait()
                .await
                .unwrap();
        })
    });
}

#[test]
fn existing_record_refuses_even_when_original_bytes_and_manifest_match() {
    with_capture(|app, store, binding, scan| {
        crate::app_runtime::async_runtime::block_on(async {
            let prepared = prepare(app, store, binding, scan).await;
            let id = prepared.accepted.artifact_id();
            let original =
                crate::paths::artifact_original_dir(&check_graph(app, binding).unwrap(), &id)
                    .unwrap();
            std::fs::create_dir_all(&original).unwrap();
            let body = original.join("asset.bin");
            std::fs::write(&body, b"captured bytes").unwrap();
            let mut manifest = crate::original_file_manifest_store::write_original_manifest_hashed(
                &original,
                &body,
                "asset.bin".into(),
                "application/octet-stream",
                b"captured bytes".len(),
                Some(prepared.accepted.sha().unwrap().into()),
            )
            .unwrap();
            manifest.updated_at = "2000-01-01T00:00:00Z".into();
            crate::storage::write_json(&original.join("manifest.json"), &manifest).unwrap();
            let before = std::fs::read(original.join("manifest.json")).unwrap();
            enqueue_crdt_operation(app.clone(),EnqueueCrdtOperationInput {
            kind:"workspace.putArtifact".into(),graph_id:GRAPH.into(),document_id:Some(id.clone()),
            payload:json!({"artifactId":id,"label":"Existing owner","originalFilename":"asset.bin","mode":"attached","storage":"owned","status":"ready"}),
        }).await.unwrap();
            let error = admit_attachment(app.clone(), store, &prepared)
                .await
                .unwrap()
                .wait()
                .await
                .err()
                .unwrap();
            assert!(error.contains("already exists"));
            assert_eq!(
                std::fs::read(original.join("manifest.json")).unwrap(),
                before,
                "preflight must run before even a matching manifest rewrite"
            );
        })
    });
}

#[test]
fn fresh_runtime_finishes_admission_without_the_capture_payload() {
    with_capture(|app, store, binding, scan| {
        crate::app_runtime::async_runtime::block_on(async {
            let prepared = prepare(app, store, binding, scan).await;
            crate::crdt_operation_journal::fail_next_guarded_append_after_write_for_test();
            assert!(matches!(
                admit_attachment(app.clone(), store, &prepared)
                    .await
                    .err()
                    .unwrap(),
                CrdtAdmissionError::Uncertain { .. }
            ));
            store.lock().unwrap().cancel(scan, binding).unwrap();
            assert!(pending_path(&check_graph(app, binding).unwrap(), &prepared.accepted).exists());
            let fresh = crate::tauri_runtime::build_mock_app_for_tests(true);
            assert_eq!(
                crate::crdt_queue::recover_crdt_operations(fresh.clone()).unwrap(),
                1
            );
            crate::crdt_engine::executor::drain_queue(fresh.clone()).await;
            let original = crate::paths::artifact_original_dir(
                &check_graph(&fresh, binding).unwrap(),
                &prepared.accepted.artifact_id(),
            )
            .unwrap()
            .join("asset.bin");
            assert_eq!(std::fs::read(original).unwrap(), b"captured bytes");
            assert!(
                crate::crdt_operation_journal::recover_pending_crdt_operations(&fresh)
                    .unwrap()
                    .is_empty()
            );
            fresh
                .state::<crate::crdt_engine::rooms::RoomRegistry>()
                .evict_graph(GRAPH);
        })
    });
}

#[test]
fn capture_store_cannot_cross_owner_profiles() {
    with_capture(|app, store, binding, scan| {
        crate::app_runtime::async_runtime::block_on(async {
            let other = tempfile::tempdir().unwrap();
            let wrong = CaptureAccess::new(CaptureStore::open(other.path()).unwrap());
            assert!(prepare_attachment(
                app,
                &wrong,
                binding,
                scan,
                0,
                "application/octet-stream",
                None
            )
            .await
            .err()
            .unwrap()
            .contains("different owner profile"));
            let prepared = prepare(app, store, binding, scan).await;
            let error = admit_attachment(app.clone(), &wrong, &prepared)
                .await
                .err()
                .unwrap()
                .into_message();
            assert!(error.contains("different owner profile"));
        })
    });
}

#[test]
fn dropped_preparation_awaiter_keeps_graph_pinned_until_worker_finishes() {
    with_capture(|app, store, binding, scan| {
        crate::app_runtime::async_runtime::block_on(async {
            let (started_tx, started_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            *PREPARATION_PAUSE.lock().unwrap() = Some((scan.into(), started_tx, release_rx));
            let mut preparing = Box::pin(prepare_attachment(
                app,
                store,
                binding,
                scan,
                0,
                "application/octet-stream",
                None,
            ));
            tokio::select! {
                _ = &mut preparing => panic!("worker should be paused"),
                started = tokio::task::spawn_blocking(move || started_rx.recv_timeout(std::time::Duration::from_secs(3))) => { started.unwrap().unwrap(); },
            }
            drop(preparing);
            let coordinator = app.state::<GraphPersistenceCoordinator>();
            assert!(tokio::time::timeout(
                std::time::Duration::from_millis(25),
                coordinator.acquire_lifecycle_exclusive(GRAPH)
            )
            .await
            .is_err());
            release_tx.send(()).unwrap();
            let _exclusive = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                coordinator.acquire_lifecycle_exclusive(GRAPH),
            )
            .await
            .unwrap()
            .unwrap();
        })
    });
}
