use super::*;
use crate::vault_job_controller::tests::with_capture;
async fn ready(app: &AppHandle, binding: crate::vault_capture::Binding, scan: &str) -> Value {
    runtime(app)
        .record_chooser_selection(binding, scan.into())
        .unwrap();
    review(app.clone(), "source".into(), scan.into(), "Imported".into())
        .await
        .unwrap()
}
async fn finished(app: &AppHandle, job: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(value) = status(app.clone(), job.into()).await {
            if value["workerActive"] == false {
                return value;
            }
        }
        assert!(Instant::now() < deadline, "worker did not finish");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
#[test]
fn only_main_window_can_use_native_import_controls() {
    assert!(require_window("main").is_ok());
    for label in ["", "other", "main-child"] {
        assert_eq!(require_window(label).unwrap_err().code, "unsupported");
    }
}
#[test]
fn persisted_capture_alone_is_not_a_current_run_chooser_capability() {
    with_capture(|app, _, binding, scan| {
        crate::app_runtime::async_runtime::block_on(async {
            assert_eq!(
                review(
                    app.clone(),
                    "source".into(),
                    scan.clone(),
                    "Imported".into()
                )
                .await
                .unwrap_err()
                .code,
                "unavailable"
            );
            let preview = ready(&app, binding, &scan).await;
            assert!(preview.get("plan").is_none());
            assert!(preview.get("notes").is_none());
            assert_eq!(preview["counts"]["sourceNotes"], 1);
            assert_eq!(preview["counts"]["attachments"], 1);
            assert_eq!(preview["counts"]["wires"], 1);
            assert!(preview["expiresAt"].as_u64().unwrap() <= now().unwrap() + 300_000);
            release(&app, preview["reviewId"].as_str().unwrap()).unwrap();
            runtime(&app).inner.lock().unwrap().selections.clear();
            assert!(review(app.clone(), "source".into(), scan, "Again".into())
                .await
                .is_err());
        })
    });
}
#[test]
fn superseded_wrong_digest_and_expired_reviews_do_not_commit() {
    with_capture(|app, profile, binding, scan| {
        crate::app_runtime::async_runtime::block_on(async {
            let first = ready(&app, binding, &scan).await;
            let second = review(app.clone(), "source".into(), scan, "Again".into())
                .await
                .unwrap();
            assert!(commit(
                app.clone(),
                first["reviewId"].as_str().unwrap().into(),
                first["digest"].as_str().unwrap().into()
            )
            .await
            .is_err());
            assert_eq!(
                commit(
                    app.clone(),
                    second["reviewId"].as_str().unwrap().into(),
                    "wrong".into()
                )
                .await
                .unwrap_err()
                .code,
                "stale_source"
            );
            assert!(runtime(&app).inner.lock().unwrap().review.is_some());
            runtime(&app)
                .inner
                .lock()
                .unwrap()
                .review
                .as_mut()
                .unwrap()
                .until = Instant::now();
            assert_eq!(
                commit(
                    app.clone(),
                    second["reviewId"].as_str().unwrap().into(),
                    second["digest"].as_str().unwrap().into()
                )
                .await
                .unwrap_err()
                .code,
                "expired"
            );
            assert!(!profile
                .join("graphs")
                .join(second["destinationGraphId"].as_str().unwrap())
                .exists());
        })
    });
}
#[test]
fn public_commit_waits_for_admission_and_history_never_recreates_a_deleted_graph() {
    with_capture(|app, profile, binding, scan| {
        crate::app_runtime::async_runtime::block_on(async {
            let preview = ready(&app, binding, &scan).await;
            let id = preview["reviewId"].as_str().unwrap().to_owned();
            let digest = preview["digest"].as_str().unwrap().to_owned();
            let job = runtime(&app)
                .inner
                .lock()
                .unwrap()
                .review
                .as_ref()
                .unwrap()
                .review
                .job_id()
                .unwrap();
            let coordinator=app.state::<crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator>();
            let lease = coordinator
                .acquire_lifecycle_exclusive("source")
                .await
                .unwrap();
            drop(coordinator);
            let request = tokio::spawn(commit(app.clone(), id.clone(), digest.clone()));
            let deadline = Instant::now() + Duration::from_secs(10);
            while runtime(&app).worker(&job).unwrap().is_none() {
                assert!(Instant::now() < deadline);
                tokio::task::yield_now().await;
            }
            assert!(!request.is_finished());
            assert!(!profile
                .join(".vault-jobs")
                .join(format!("{job}.json"))
                .exists());
            assert_eq!(
                review(app.clone(), "source".into(), scan.clone(), "Busy".into())
                    .await
                    .unwrap_err()
                    .code,
                "busy"
            );
            drop(lease);
            let accepted = request.await.unwrap().unwrap();
            assert_eq!(accepted["jobId"], job);
            let done = finished(&app, &job).await;
            assert_eq!(done["phase"], "published");
            assert_eq!(done["destinationAvailable"], true);
            assert_eq!(commit(app.clone(), id, digest).await.unwrap()["jobId"], job);
            assert_eq!(
                cancel(app.clone(), job.clone()).await.unwrap()["phase"],
                "published"
            );
            assert_eq!(
                recent(app.clone(), 20).await.unwrap()["jobs"][0]["jobId"],
                job
            );
            assert_eq!(
                crate::vault_selection::status(&app, "source", &scan, 0, 1).unwrap()["jobId"],
                job
            );
            let target = profile
                .join("graphs")
                .join(done["destinationGraphId"].as_str().unwrap());
            crate::rdf_store_service::evict_graph_store(&target).unwrap();
            std::fs::remove_dir_all(&target).unwrap();
            let historical = status(app.clone(), job).await.unwrap();
            assert_eq!(historical["phase"], "published");
            assert_eq!(historical["destinationAvailable"], false);
            assert!(!target.exists());
        })
    });
}
#[test]
fn disconnecting_confirmation_does_not_cancel_the_worker() {
    with_capture(|app, _, binding, scan| {
        crate::app_runtime::async_runtime::block_on(async {
            let preview = ready(&app, binding, &scan).await;
            let job = runtime(&app)
                .inner
                .lock()
                .unwrap()
                .review
                .as_ref()
                .unwrap()
                .review
                .job_id()
                .unwrap();
            let coordinator=app.state::<crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator>();
            let lease = coordinator
                .acquire_lifecycle_exclusive(preview["destinationGraphId"].as_str().unwrap())
                .await
                .unwrap();
            drop(coordinator);
            let request = tokio::spawn(commit(
                app.clone(),
                preview["reviewId"].as_str().unwrap().into(),
                preview["digest"].as_str().unwrap().into(),
            ));
            let deadline = Instant::now() + Duration::from_secs(10);
            while runtime(&app).worker(&job).unwrap().is_none() {
                assert!(Instant::now() < deadline);
                tokio::task::yield_now().await;
            }
            // A missing durable link is only a current observation: this
            // already-scheduled worker can still admit after the gate opens.
            assert!(
                crate::vault_selection::status(&app, "source", &scan, 0, 1).unwrap()["jobId"]
                    .is_null()
            );
            request.abort();
            drop(lease);
            assert_eq!(finished(&app, &job).await["phase"], "published");
        })
    });
}
#[test]
fn interrupted_admission_is_reported_and_can_be_explicitly_discarded() {
    with_capture(|app, _, binding, scan| {
        crate::app_runtime::async_runtime::block_on(async {
            ready(&app, binding, &scan).await;
            let held = runtime(&app)
                .inner
                .lock()
                .unwrap()
                .review
                .as_ref()
                .unwrap()
                .review
                .clone();
            let job = held.job_id().unwrap();
            let result = vault_job_controller::commit_notified(
                app.clone(),
                held.clone(),
                held.digest().into(),
                Arc::new(AtomicBool::new(false)),
                Some(Box::new(|_| panic!("simulate owner loss after admission"))),
            )
            .await;
            assert!(result.is_err());
            let interrupted = status(app.clone(), job.clone()).await.unwrap();
            assert_eq!(interrupted["phase"], "accepted");
            assert_eq!(interrupted["workerActive"], false);
            assert!(interrupted["error"].is_string());
            assert_eq!(
                cancel(app.clone(), job).await.unwrap()["phase"],
                "abandoned"
            );
        })
    });
}
#[test]
fn maintenance_releases_expired_preview_and_history_limits_are_bounded() {
    with_capture(|app, _, binding, scan| {
        crate::app_runtime::async_runtime::block_on(async {
            ready(&app, binding, &scan).await;
            let ui = runtime(&app);
            ui.inner.lock().unwrap().review.as_mut().unwrap().until = Instant::now();
            ui.expire().unwrap();
            assert!(ui.inner.lock().unwrap().review.is_none());
            assert_eq!(
                recent(app.clone(), 51).await.unwrap_err().code,
                "unsupported"
            );
            assert_eq!(recent(app, 0).await.unwrap_err().code, "unsupported");
        })
    });
}

#[test]
fn abandoned_picker_callback_cannot_release_a_new_picker() {
    let latch = Arc::new(PickerLatch::default());
    let (request, late_callback) = latch.enter().unwrap();
    assert!(latch.enter().is_err());
    drop(request);
    let (next_request, next_callback) = latch.enter().unwrap();
    drop(late_callback);
    assert!(latch.enter().is_err());
    drop(next_request);
    drop(next_callback);
    assert!(latch.enter().is_ok());
}
#[test]
fn rejected_admission_does_not_leave_a_phantom_history_handle() {
    with_capture(|app, _, binding, scan| {
        crate::app_runtime::async_runtime::block_on(async {
            let preview = ready(&app, binding, &scan).await;
            let ui = runtime(&app);
            let job = ui
                .inner
                .lock()
                .unwrap()
                .review
                .as_ref()
                .unwrap()
                .review
                .job_id()
                .unwrap();
            crate::vault_selection::cancel(&app, "source", &scan).unwrap();
            let id = preview["reviewId"].as_str().unwrap().to_owned();
            let digest = preview["digest"].as_str().unwrap().to_owned();
            assert!(commit(app.clone(), id.clone(), digest.clone())
                .await
                .is_err());
            assert!(ui.worker(&job).unwrap().is_none());
            let retry = commit(app, id, digest).await.unwrap_err();
            assert!(retry.message.contains("preview"));
            assert!(!retry.message.contains("Check its status"));
        })
    });
}
