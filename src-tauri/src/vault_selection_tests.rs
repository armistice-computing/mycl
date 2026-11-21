use super::*;
use std::{
    io::Write,
    time::{Duration, Instant},
};

fn with_source(test: impl FnOnce(&AppHandle, &Path, &Binding)) {
    let _serial = crate::tauri_runtime::profile_env_serial()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let previous = std::env::var_os("GARDEN_PROFILE_DIR");
    let profile = tempfile::tempdir().unwrap();
    let sources = tempfile::tempdir().unwrap();
    let source = sources.path().join("vault");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("note.md"), "# My source\n").unwrap();
    std::fs::write(source.join("image.bin"), b"source bytes").unwrap();
    std::env::set_var("GARDEN_PROFILE_DIR", profile.path());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);
        crate::graph_service::create_graph_service(
            &app,
            crate::graph_service::CreateGraphInput {
                graph_id: Some("native-vault-test".into()),
                title: "Native vault".into(),
                description: None,
                operation_id: None,
            },
        )
        .unwrap();
        crate::app_runtime::async_runtime::block_on(crate::vault_runtime::initialize(&app))
            .unwrap();
        let binding = owner_binding(&app, "native-vault-test").unwrap();
        test(&app, &source, &binding);
        let deadline = Instant::now() + Duration::from_secs(20);
        while app
            .state::<Arc<crate::cell_lifecycle::CellLifecycle>>()
            .snapshot()
            .background_leases
            > 0
        {
            assert!(Instant::now() < deadline, "capture activity did not settle");
            std::thread::sleep(Duration::from_millis(10));
        }
        app.state::<crate::crdt_engine::rooms::RoomRegistry>()
            .evict_graph(&binding.graph);
    }));
    match previous {
        Some(value) => std::env::set_var("GARDEN_PROFILE_DIR", value),
        None => std::env::remove_var("GARDEN_PROFILE_DIR"),
    }
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

fn wait_ready(app: &AppHandle, binding: &Binding, scan: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let result = status(app, &binding.graph, scan, 0, 50).unwrap();
        if result["state"] != "capturing" {
            assert_eq!(result["state"], "ready", "{result}");
            return result;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn source_selection_excludes_profile_descendants_ancestors_and_aliases() {
    let root = tempfile::tempdir().unwrap();
    let profile = root.path().join("profile");
    std::fs::create_dir(&profile).unwrap();
    std::fs::write(profile.join("secret.zip"), b"secret").unwrap();
    assert!(open_selected_source(&profile, &profile, SourceKind::Folder).is_err());
    assert!(open_selected_source(root.path(), &profile, SourceKind::Folder).is_err());
    assert!(open_selected_source(&profile.join("secret.zip"), &profile, SourceKind::Zip).is_err());
    #[cfg(unix)]
    {
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&profile, &alias).unwrap();
        assert!(open_selected_source(&alias, &profile, SourceKind::Folder).is_err());
        let linked = root.path().join("outside.zip");
        std::fs::hard_link(profile.join("secret.zip"), &linked).unwrap();
        assert!(open_selected_source(&linked, &profile, SourceKind::Zip)
            .unwrap_err()
            .contains("hard link"));
    }
    let source = root.path().join("source");
    std::fs::create_dir(&source).unwrap();
    assert!(open_selected_source(&source, &profile, SourceKind::Folder).is_ok());
    assert!(open_selected_source(&source, &profile, SourceKind::Zip).is_err());
    assert!(kind("arbitrary-path").is_err());
}

#[test]
fn selected_folder_captures_without_graph_writes_and_pages_without_internal_paths() {
    with_source(|app, source, binding| {
        let graph_dir = crate::graph_record_store::read_graph_record_no_heal(app, &binding.graph)
            .unwrap()
            .0;
        let authority = crate::ydoc_paths::workspace_ydoc_state_path(&graph_dir);
        let before = std::fs::read(&authority).unwrap();
        let file = open_selected_source(
            source,
            &crate::profile_paths::profile_dir(app).unwrap(),
            SourceKind::Folder,
        )
        .unwrap();
        let started = crate::app_runtime::async_runtime::block_on(start_capture(
            app.clone(),
            binding.clone(),
            file,
            SourceKind::Folder,
        ))
        .unwrap();
        let scan = started["scanId"].as_str().unwrap();
        wait_ready(app, binding, scan);
        let page = status(app, &binding.graph, scan, 0, 1).unwrap();
        assert_eq!(page["totalEntries"], 2);
        assert_eq!(page["entries"].as_array().unwrap().len(), 1);
        assert_eq!(page["nextOffset"], 1);
        assert!(!page.to_string().contains(".blob"));
        assert!(!page
            .to_string()
            .contains(&source.to_string_lossy().to_string()));
        assert!(status(app, "other-graph", scan, 0, 1).is_err());
        assert!(status(app, &binding.graph, scan, 0, 0).is_err());
        assert!(status(app, &binding.graph, scan, 0, 51).is_err());
        let store = app.state::<Arc<VaultRuntime>>().store().unwrap();
        let mut stranger = binding.clone();
        stranger.principal = "agent:stranger".into();
        assert!(store.lock().unwrap().owner_status(scan, &stranger).is_err());
        assert_eq!(std::fs::read(authority).unwrap(), before);
        assert_eq!(
            std::fs::read(source.join("image.bin")).unwrap(),
            b"source bytes"
        );
        assert_eq!(
            cancel(app, &binding.graph, scan).unwrap()["state"],
            "cancelled"
        );
        assert_eq!(
            status(app, &binding.graph, scan, 0, 50).unwrap()["totalEntries"],
            0
        );
    });
}

#[test]
fn cancellation_keeps_the_worker_slot_until_worker_exits() {
    with_source(|app, source, binding| {
        let (notify, began) = std::sync::mpsc::channel();
        let (resume, wait) = std::sync::mpsc::channel();
        *CAPTURE_PAUSE.lock().unwrap() = Some((binding.graph.clone(), notify, wait));
        let started = crate::app_runtime::async_runtime::block_on(start_capture(
            app.clone(),
            binding.clone(),
            File::open(source).unwrap(),
            SourceKind::Folder,
        ))
        .unwrap();
        began.recv_timeout(Duration::from_secs(20)).unwrap();
        let scan = started["scanId"].as_str().unwrap();
        assert_eq!(
            status(app, &binding.graph, scan, 0, 50).unwrap()["state"],
            "capturing"
        );
        assert_eq!(
            cancel(app, &binding.graph, scan).unwrap()["state"],
            "cancelled"
        );
        let second = crate::app_runtime::async_runtime::block_on(start_capture(
            app.clone(),
            binding.clone(),
            File::open(source).unwrap(),
            SourceKind::Folder,
        ));
        resume.send(()).unwrap();
        assert!(second.unwrap_err().contains("another capture"));
    });
}

#[test]
fn selection_incarnation_change_refuses_before_grant_creation() {
    with_source(|app, source, binding| {
        let mut wrong = binding.clone();
        wrong.incarnation = "old-incarnation".into();
        assert!(crate::app_runtime::async_runtime::block_on(start_capture(
            app.clone(),
            wrong,
            File::open(source).unwrap(),
            SourceKind::Folder
        ))
        .unwrap_err()
        .contains("changed"));
        let capture = crate::profile_paths::profile_dir(app)
            .unwrap()
            .join("vault-capture");
        assert!(!std::fs::read_dir(capture).unwrap().any(|e| e
            .unwrap()
            .path()
            .extension()
            .is_some_and(|x| x == "grant")));
    });
}

#[test]
fn zip_capture_failure_is_reported_without_false_cancellation_success() {
    with_source(|app, source, binding| {
        let bad = source.join("not-zip.bin");
        std::fs::write(&bad, b"not a ZIP").unwrap();
        let started = crate::app_runtime::async_runtime::block_on(start_capture(
            app.clone(),
            binding.clone(),
            File::open(bad).unwrap(),
            SourceKind::Zip,
        ))
        .unwrap();
        let scan = started["scanId"].as_str().unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let result = status(app, &binding.graph, scan, 0, 50).unwrap();
            if result["state"] == "failed" {
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        let result = cancel(app, &binding.graph, scan).unwrap();
        assert_eq!(result["state"], "failed");
        assert_eq!(result["cancelled"], false);
    });
}

#[test]
fn zip_capture_uses_the_same_bounded_preview_surface() {
    with_source(|app, source, binding| {
        let zip = source.join("notes.zip");
        let mut writer = zip::ZipWriter::new(File::create(&zip).unwrap());
        writer
            .start_file("Notes/a.md", zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"# Note").unwrap();
        writer.finish().unwrap();
        let started = crate::app_runtime::async_runtime::block_on(start_capture(
            app.clone(),
            binding.clone(),
            File::open(zip).unwrap(),
            SourceKind::Zip,
        ))
        .unwrap();
        let ready = wait_ready(app, binding, started["scanId"].as_str().unwrap());
        assert!(ready["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["path"] == "Notes/a.md" && e["kind"] == "note"));
    });
}

#[test]
fn selected_descriptor_survives_rename_without_reopening_replacement() {
    with_source(|app, source, binding| {
        let file = open_selected_source(
            source,
            &crate::profile_paths::profile_dir(app).unwrap(),
            SourceKind::Folder,
        )
        .unwrap();
        let moved = source.parent().unwrap().join("moved");
        std::fs::rename(source, &moved).unwrap();
        std::fs::create_dir(source).unwrap();
        std::fs::write(source.join("replacement.md"), b"not selected").unwrap();
        let started = crate::app_runtime::async_runtime::block_on(start_capture(
            app.clone(),
            binding.clone(),
            file,
            SourceKind::Folder,
        ))
        .unwrap();
        let scan = started["scanId"].as_str().unwrap().to_owned();
        drop(started); // the worker/status are not owned by the response value
        let ready = wait_ready(app, binding, &scan);
        let entries = ready["entries"].as_array().unwrap();
        assert!(entries.iter().any(|e| e["path"] == "note.md"));
        assert!(!entries.iter().any(|e| e["path"] == "replacement.md"));
        assert_eq!(
            std::fs::read(moved.join("image.bin")).unwrap(),
            b"source bytes"
        );
    });
}

#[test]
fn graph_deletion_while_scanning_revokes_capture_and_does_not_resurrect_graph() {
    with_source(|app, source, binding| {
        let (notify, began) = std::sync::mpsc::channel();
        let (resume, wait) = std::sync::mpsc::channel();
        *CAPTURE_PAUSE.lock().unwrap() = Some((binding.graph.clone(), notify, wait));
        let started = crate::app_runtime::async_runtime::block_on(start_capture(
            app.clone(),
            binding.clone(),
            File::open(source).unwrap(),
            SourceKind::Folder,
        ))
        .unwrap();
        began.recv_timeout(Duration::from_secs(20)).unwrap();
        crate::graph_service::soft_delete_graph_service(app, binding.graph.clone(), false).unwrap();
        resume.send(()).unwrap();
        assert!(status(
            app,
            &binding.graph,
            started["scanId"].as_str().unwrap(),
            0,
            50,
        )
        .is_err());
        assert!(owner_binding(app, &binding.graph).is_err());
        assert_eq!(
            std::fs::read(source.join("image.bin")).unwrap(),
            b"source bytes"
        );
    });
}

#[test]
fn pre_open_ancestor_replacement_is_not_the_selected_capability() {
    let root = tempfile::tempdir().unwrap();
    let profile = root.path().join("profile");
    let ancestor = root.path().join("container");
    let source = ancestor.join("vault");
    std::fs::create_dir(&profile).unwrap();
    std::fs::create_dir_all(&source).unwrap();
    let opened = open_selected_source_with_hook(&source, &profile, SourceKind::Folder, || {
        std::fs::rename(&ancestor, root.path().join("old-container")).unwrap();
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("not-selected.md"), b"replacement").unwrap();
    });
    assert!(opened.unwrap_err().contains("changed before opening"));
}

#[test]
fn stale_ready_capture_is_not_visible_under_a_recreated_graph_identity() {
    with_source(|app, source, binding| {
        let started = crate::app_runtime::async_runtime::block_on(start_capture(
            app.clone(),
            binding.clone(),
            File::open(source).unwrap(),
            SourceKind::Folder,
        ))
        .unwrap();
        let scan = started["scanId"].as_str().unwrap();
        wait_ready(app, binding, scan);
        let (dir, mut graph) =
            crate::graph_record_store::read_graph_record_no_heal(app, &binding.graph).unwrap();
        // Simulate the tolerated failed-cleanup path: the old Ready receipt
        // remains intact while canonical graph identity has changed.
        graph.incarnation_id = Some(uuid::Uuid::new_v4().to_string());
        crate::graph_record_store::write_graph_record_canonical(&dir, &graph).unwrap();
        assert!(status(app, &binding.graph, scan, 0, 50).is_err());
        assert!(cancel(app, &binding.graph, scan).is_err());
        assert!(
            app.state::<Arc<VaultRuntime>>()
                .store()
                .unwrap()
                .lock()
                .unwrap()
                .get_ready(scan, binding)
                .is_ok(),
            "old receipt still exists; boundary, not cleanup, refused access"
        );
    });
}

#[test]
fn capture_walk_refuses_private_profile_by_identity_even_without_path_filter() {
    let root = tempfile::tempdir().unwrap();
    let profile = root.path().join("private-profile");
    std::fs::create_dir(&profile).unwrap();
    std::fs::write(root.path().join("ordinary.md"), b"ordinary").unwrap();
    let mut store = crate::vault_capture::CaptureStore::open(&profile).unwrap();
    let binding = Binding {
        principal: OWNER.into(),
        graph: "test".into(),
        incarnation: "test-life".into(),
    };
    let grant = store
        .grant_selected(
            binding.clone(),
            File::open(root.path()).unwrap(),
            SourceKind::Folder,
            TTL_MS,
        )
        .unwrap();
    let mut ticket = store.begin(&grant, &binding).unwrap();
    assert!(ticket.capture().unwrap_err().contains("private profile"));
    assert_eq!(
        std::fs::read(root.path().join("ordinary.md")).unwrap(),
        b"ordinary"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn apfs_physical_path_alias_cannot_select_private_profile_bytes() {
    let profile = tempfile::tempdir().unwrap();
    let canonical = std::fs::canonicalize(profile.path()).unwrap();
    std::fs::write(canonical.join("secret.zip"), b"private").unwrap();
    let alias = std::path::Path::new("/System/Volumes/Data")
        .join(canonical.strip_prefix("/").unwrap())
        .join("secret.zip");
    if !alias.exists() {
        return;
    } // older/nonstandard APFS layouts
    assert!(
        !alias.starts_with(&canonical),
        "exercise a non-prefix alias"
    );
    assert!(open_selected_source(&alias, &canonical, SourceKind::Zip)
        .unwrap_err()
        .contains("private profile"));
}
