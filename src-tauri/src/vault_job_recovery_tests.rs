use super::*;
use crate::vault_job::Identity;

fn fixture(profile: &Path) -> (Store, Identity, String, PathBuf) {
    let jobs = Store::open(profile).unwrap();
    let identity = Identity {
        job_id: uuid::Uuid::new_v4().to_string(),
        operation_id: "garden-admit-test".into(),
        source: crate::vault_capture::Binding {
            principal: "human:owner".into(),
            graph: "source".into(),
            incarnation: uuid::Uuid::new_v4().to_string(),
        },
        scan_id: uuid::Uuid::new_v4().to_string(),
        capture_fingerprint: "a".repeat(64),
        plan_digest: "b".repeat(64),
        preparation_digest: "c".repeat(64),
        destination_graph: "new-vault".into(),
        destination_incarnation: uuid::Uuid::new_v4().to_string(),
        consent_expires_at: 10000,
    };
    jobs.create(identity.clone(), 1).unwrap();
    let stage_id = uuid::Uuid::new_v4().to_string();
    jobs.building(&identity, &stage_id, 2).unwrap();
    let stage = profile.join(".graph-staging").join(format!(
        "new-vault-{}",
        uuid::Uuid::parse_str(&stage_id).unwrap().simple()
    ));
    fs::create_dir_all(&stage).unwrap();
    crate::storage::write_json(&stage.join("vault-job.json"),&serde_json::json!({"version":1,"stageId":stage_id,"binding":crate::vault_job_bridge::committed(&identity).unwrap()})).unwrap();
    fs::write(stage.join("payload"), b"owned bytes").unwrap();
    (jobs, identity, stage_id, stage)
}

#[test]
fn owned_interrupted_stage_is_terminalized_then_removed_and_retry_is_harmless() {
    let root = tempfile::tempdir().unwrap();
    let (jobs, id, stage_id, stage) = fixture(root.path());
    let result = recover_profile(root.path(), &id.job_id, remove).unwrap();
    assert_eq!(result.stage, "removed");
    assert!(result.logical_bytes >= 11);
    assert!(!stage.exists());
    assert_eq!(
        jobs.read(&id.job_id).unwrap().state,
        State::Abandoned {
            stage_id: Some(stage_id)
        }
    );
    assert_eq!(
        recover_profile(root.path(), &id.job_id, remove)
            .unwrap()
            .stage,
        "absent"
    );
    assert!(!root.path().join("graphs/new-vault").exists());
}

#[test]
fn missing_or_mismatched_marker_never_authorizes_nonempty_cleanup() {
    for mismatched in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let (jobs, id, stage_id, stage) = fixture(root.path());
        if mismatched {
            let mut binding = crate::vault_job_bridge::committed(&id).unwrap();
            binding.identity_digest = "d".repeat(64);
            crate::storage::write_json(
                &stage.join("vault-job.json"),
                &serde_json::json!({"version":1,"stageId":stage_id,"binding":binding}),
            )
            .unwrap();
        } else {
            fs::remove_file(stage.join("vault-job.json")).unwrap();
        }
        assert_eq!(
            recover_profile(root.path(), &id.job_id, remove)
                .unwrap()
                .stage,
            "retained"
        );
        assert_eq!(fs::read(stage.join("payload")).unwrap(), b"owned bytes");
        assert!(matches!(
            jobs.read(&id.job_id).unwrap().state,
            State::Building { .. }
        ));
    }
}

#[test]
fn publication_uncertainty_is_retained_even_when_destination_is_absent() {
    let root = tempfile::tempdir().unwrap();
    let (jobs, id, _, stage) = fixture(root.path());
    jobs.publication_intent(&id, 3).unwrap();
    let result = recover_profile(root.path(), &id.job_id, remove).unwrap();
    assert_eq!(result.stage, "retained");
    assert!(matches!(result.job.state, State::Uncertain { .. }));
    assert!(stage.join("payload").exists());
}

#[test]
fn any_existing_destination_prevents_stage_reclamation() {
    let root = tempfile::tempdir().unwrap();
    let (_, id, _, stage) = fixture(root.path());
    let target = root.path().join("graphs/new-vault");
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("precious"), b"keep").unwrap();
    assert_eq!(
        recover_profile(root.path(), &id.job_id, remove)
            .unwrap()
            .stage,
        "retained"
    );
    assert!(stage.exists());
    assert_eq!(fs::read(target.join("precious")).unwrap(), b"keep");
}

#[test]
fn partial_removal_keeps_ownership_marker_for_a_safe_retry() {
    let root = tempfile::tempdir().unwrap();
    let (jobs, id, _, stage) = fixture(root.path());
    fs::write(stage.join("a-first"), b"first").unwrap();
    let result = recover_profile(root.path(), &id.job_id, |path, is_dir| {
        if path.file_name().unwrap() == "payload" {
            Err("injected removal failure".into())
        } else {
            remove(path, is_dir)
        }
    })
    .unwrap();
    assert_eq!(result.stage, "retained");
    assert!(!stage.join("a-first").exists());
    assert!(stage.join("vault-job.json").exists());
    assert!(matches!(
        jobs.read(&id.job_id).unwrap().state,
        State::Abandoned { .. }
    ));
    assert_eq!(
        recover_profile(root.path(), &id.job_id, remove)
            .unwrap()
            .stage,
        "removed"
    );
}

#[test]
fn empty_registered_stage_without_marker_is_removable() {
    let root = tempfile::tempdir().unwrap();
    let (_, id, _, stage) = fixture(root.path());
    fs::remove_file(stage.join("payload")).unwrap();
    fs::remove_file(stage.join("vault-job.json")).unwrap();
    assert_eq!(
        recover_profile(root.path(), &id.job_id, remove)
            .unwrap()
            .stage,
        "removed"
    );
    assert!(!stage.exists());
}

#[cfg(unix)]
#[test]
fn symlinks_in_owned_stage_are_retained_without_touching_the_target() {
    let root = tempfile::tempdir().unwrap();
    let (_, id, _, stage) = fixture(root.path());
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("precious"), b"keep").unwrap();
    std::os::unix::fs::symlink(outside.path(), stage.join("escape")).unwrap();
    assert_eq!(
        recover_profile(root.path(), &id.job_id, remove)
            .unwrap()
            .stage,
        "retained"
    );
    assert_eq!(fs::read(outside.path().join("precious")).unwrap(), b"keep");
    assert!(stage.join("payload").exists());
}

#[test]
fn bounded_inventory_refuses_without_removing_anything() {
    let root = tempfile::tempdir().unwrap();
    let (_, _, _, stage) = fixture(root.path());
    assert!(inventory(&stage, 1).is_err());
    assert!(stage.join("payload").exists());
}

#[cfg(unix)]
#[test]
fn redirected_stage_root_is_never_followed() {
    let root = tempfile::tempdir().unwrap();
    let (_, id, _, stage) = fixture(root.path());
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("precious"), b"keep").unwrap();
    fs::remove_dir_all(&stage).unwrap();
    std::os::unix::fs::symlink(outside.path(), &stage).unwrap();
    assert_eq!(
        recover_profile(root.path(), &id.job_id, remove)
            .unwrap()
            .stage,
        "retained"
    );
    assert_eq!(fs::read(outside.path().join("precious")).unwrap(), b"keep");
}

#[test]
fn active_stage_manifest_conflicts_with_building_and_is_retained() {
    let root = tempfile::tempdir().unwrap();
    let (_, id, _, stage) = fixture(root.path());
    crate::storage::write_json(&stage.join("graph.json"),&serde_json::json!({"graphId":"new-vault","title":"Imported","status":"active","origin":"local","providerId":"local-profile","localPath":"unused","createdAt":"1","updatedAt":"1","capabilities":[]})).unwrap();
    assert_eq!(
        recover_profile(root.path(), &id.job_id, remove)
            .unwrap()
            .stage,
        "retained"
    );
    assert!(stage.join("payload").exists());
}

#[test]
fn recovery_waits_for_publication_lock_and_rereads_job_state() {
    let root = tempfile::tempdir().unwrap();
    let (jobs, id, _, stage) = fixture(root.path());
    let lock =
        crate::graph_duplicate_storage::acquire_graph_publication_lock(root.path(), "new-vault")
            .unwrap();
    let profile = root.path().to_path_buf();
    let job_id = id.job_id.clone();
    let (started, rx) = std::sync::mpsc::channel();
    let (done, result) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        started.send(()).unwrap();
        done.send(recover_profile(&profile, &job_id, remove))
            .unwrap();
    });
    rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    assert!(result
        .recv_timeout(std::time::Duration::from_millis(50))
        .is_err());
    jobs.publication_intent(&id, 3).unwrap();
    let target = root.path().join("graphs/new-vault");
    fs::create_dir_all(&target).unwrap();
    crate::storage::write_json(&target.join("graph.json"),&serde_json::json!({"graphId":"new-vault","title":"Imported","status":"active","origin":"local","providerId":"local-profile","localPath":target.to_str().unwrap(),"createdAt":"1","updatedAt":"1","capabilities":[],"incarnationId":id.destination_incarnation,"createdByOperationId":id.operation_id})).unwrap();
    drop(lock);
    let report = result
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap()
        .unwrap();
    worker.join().unwrap();
    assert_eq!(report.job.state, State::Published);
    assert_eq!(report.stage, "retained");
    assert!(stage.join("payload").exists());
}
