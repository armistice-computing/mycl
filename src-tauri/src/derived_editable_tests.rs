//! Workspace editability must not transfer ownership of server-derived views.
#![cfg(all(test, feature = "headless"))]

use crate::{
    app_runtime::{async_runtime::block_on, AppHandle},
    crdt_engine::rooms::RoomRegistry,
    crdt_queue::{enqueue_crdt_operation, CrdtOperationQueue, EnqueueCrdtOperationInput},
};
use serde_json::{json, Value};
use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};

const GRAPH: &str = "derived-editable-tests";

struct Profile {
    path: PathBuf,
    previous: Option<OsString>,
}

impl Drop for Profile {
    fn drop(&mut self) {
        // Never restore the real profile while a failed test may still drain.
        if std::thread::panicking() {
            return;
        }
        match &self.previous {
            Some(value) => std::env::set_var("GARDEN_PROFILE_DIR", value),
            None => std::env::remove_var("GARDEN_PROFILE_DIR"),
        }
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn isolated(test: impl FnOnce(&AppHandle, &Path)) {
    let _serial = crate::tauri_runtime::profile_env_serial()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let profile = Profile {
        path: std::env::temp_dir()
            .join(format!("garden-derived-editable-{}", uuid::Uuid::new_v4())),
        previous: std::env::var_os("GARDEN_PROFILE_DIR"),
    };
    fs::create_dir(&profile.path).unwrap();
    std::env::set_var("GARDEN_PROFILE_DIR", &profile.path);
    let app = crate::tauri_runtime::build_mock_app_for_tests(true);
    crate::graph_service::create_graph_service(
        &app,
        crate::graph_service::CreateGraphInput {
            graph_id: Some(GRAPH.into()),
            title: "Derived editability tests".into(),
            description: None,
            operation_id: None,
        },
    )
    .unwrap();
    let (dir, _) = crate::graph_service::read_graph_record(&app, GRAPH).unwrap();
    assert!(dir
        .canonicalize()
        .unwrap()
        .starts_with(profile.path.canonicalize().unwrap()));
    test(&app, &dir);
}

fn queue(
    app: &AppHandle,
    kind: &str,
    document_id: Option<&str>,
    payload: Value,
) -> Result<Value, String> {
    block_on(async {
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            enqueue_crdt_operation(
                app.clone(),
                EnqueueCrdtOperationInput {
                    kind: kind.into(),
                    graph_id: GRAPH.into(),
                    document_id: document_id.map(str::to_owned),
                    payload,
                },
            ),
        )
        .await
        .expect("ownership rejection must be terminal, not retried")
    })
}

fn publish(app: &AppHandle) -> String {
    let result = crate::geist_song_service::mcp_local_sing(
        app.clone(),
        &json!({
            "graph_id": GRAPH, "verse": "The native store keeps its authority."
        }),
    )
    .unwrap();
    assert_eq!(result["projection_warnings"], json!([]));
    crate::geist_song_store::song_doc_id("")
}

fn rejected(result: Result<Value, String>) {
    let error = result.expect_err("derived ownership must reject editability");
    assert!(error.contains("read-only"), "unexpected error: {error}");
}

fn journal(app: &AppHandle) -> Vec<u8> {
    fs::read(crate::crdt_operation_journal::crdt_operation_journal_path(app).unwrap())
        .unwrap_or_default()
}

#[derive(Debug, PartialEq)]
struct Snapshot {
    live: Vec<u8>,
    files: Vec<Option<Vec<u8>>>,
    rdf: Vec<String>,
}

fn snapshot(app: &AppHandle, dir: &Path, id: &str) -> Snapshot {
    let room = block_on(
        app.state::<RoomRegistry>()
            .peek(&format!("workspace:{GRAPH}")),
    )
    .unwrap();
    let mut rdf: Vec<_> = crate::rdf_store_service::open_graph_store(dir)
        .unwrap()
        .iter()
        .map(|quad| quad.unwrap().to_string())
        .collect();
    rdf.sort();
    Snapshot {
        live: block_on(room.encode_state_for_test()),
        files: [
            crate::paths::workspace_ydoc_state_path(dir),
            crate::paths::workspace_snapshot_path(dir),
            crate::paths::document_ydoc_state_path(dir, id),
            dir.join("documents").join(id).join("document.json"),
            dir.join("narrative/song.json"),
        ]
        .iter()
        .map(|path| fs::read(path).ok())
        .collect(),
        rdf,
    }
}

fn workspace_entry(dir: &Path, id: &str) -> Value {
    let snapshot: Value =
        crate::storage::read_json(&crate::paths::workspace_snapshot_path(dir)).unwrap();
    crate::workspace_entity_projection::workspace_document_entity(Some(&snapshot), id).unwrap()
}

#[test]
fn derived_make_editable_rejects_before_enqueue_without_changing_flags_or_rdf() {
    isolated(|app, dir| {
        let id = publish(app);
        assert_eq!(workspace_entry(dir, &id)["readOnly"], true);
        let before = snapshot(app, dir, &id);
        let journal_before = journal(app);
        for args in [
            json!({"graph_id": GRAPH, "document_id": id, "force": true}),
            json!({"graphId": GRAPH, "documentId": id, "force": true}),
        ] {
            rejected(block_on(
                crate::mcp_workspace_crdt_service::mcp_local_make_document_editable(
                    app.clone(),
                    &args,
                ),
            ));
            assert_eq!(
                journal(app),
                journal_before,
                "MCP rejection must precede enqueue"
            );
            assert_eq!(snapshot(app, dir, &id), before);
        }
    });
}

#[test]
fn derived_workspace_queue_rejects_false_aliases_and_upserts_before_writes() {
    isolated(|app, dir| {
        let id = publish(app);
        let before = snapshot(app, dir, &id);
        for kind in ["workspace.updateDocument", "workspace.createDocument"] {
            for flag in [
                json!(false),
                json!("false"),
                json!("0"),
                json!("no"),
                Value::Null,
                json!(0),
                json!("invalid"),
            ] {
                for key in ["readOnly", "read_only"] {
                    let mut payload =
                        json!({"documentId": id, "title": "Must not change", "force": true});
                    payload[key] = flag.clone();
                    let journal_before = journal(app).len();
                    rejected(queue(app, kind, None, payload));
                    assert_eq!(snapshot(app, dir, &id), before);
                    let after = journal(app);
                    let events: Vec<Value> = std::str::from_utf8(&after[journal_before..])
                        .unwrap()
                        .lines()
                        .map(|line| serde_json::from_str(line).unwrap())
                        .collect();
                    assert_eq!(events.len(), 2, "one enqueue and one terminal completion");
                    assert_eq!(events[0]["status"], "queued");
                    assert_eq!(events[1]["status"], "failed");
                    let queue = app.state::<CrdtOperationQueue>();
                    assert_eq!(queue.counts_for_test().unwrap(), (0, 0));
                    assert_eq!(queue.retry_wait_count_for_test().unwrap(), 0);
                }
            }
        }
        // Envelope precedence for updates; payload precedence for creates.
        rejected(queue(
            app,
            "workspace.updateDocument",
            Some(&id),
            json!({
                "documentId": "ordinary-decoy", "readOnly": false, "force": true
            }),
        ));
        for payload in [
            json!({"documentId": id}),
            json!({"documentId": id, "readOnly": true}),
            json!({"documentId": id, "readOnly": true, "read_only": false}),
        ] {
            rejected(queue(
                app,
                "workspace.createDocument",
                Some("ordinary-decoy"),
                payload,
            ));
        }
        rejected(queue(
            app,
            "workspace.updateDocument",
            None,
            json!({
                "documentId": id, "readOnly": true, "read_only": false
            }),
        ));
        assert_eq!(snapshot(app, dir, &id), before);
    });
}

#[test]
fn derived_workspace_metadata_updates_and_explicit_true_remain_legal() {
    isolated(|app, dir| {
        let id = publish(app);
        let title = workspace_entry(dir, &id)["title"].clone();
        queue(
            app,
            "workspace.createFolder",
            None,
            json!({"folderId": "destination", "name": "Destination"}),
        )
        .unwrap();
        queue(
            app,
            "workspace.updateDocument",
            Some(&id),
            json!({
                "parent_id": "destination", "order": 7
            }),
        )
        .unwrap();
        for echoed_title in [
            title.clone(),
            json!(format!("  {}  ", title.as_str().unwrap())),
        ] {
            queue(
                app,
                "workspace.updateDocument",
                None,
                json!({
                    "documentId": id, "title": echoed_title, "order": 7
                }),
            )
            .unwrap();
        }
        for key in ["readOnly", "read_only"] {
            for flag in [json!(true), json!("true"), json!("1"), json!(" YES ")] {
                let mut payload = json!({"documentId": id});
                payload[key] = flag;
                queue(app, "workspace.updateDocument", None, payload).unwrap();
            }
        }
        let entry = workspace_entry(dir, &id);
        assert_eq!(entry["title"], title);
        assert_eq!(entry["parentId"], "destination");
        assert_eq!(entry["order"], 7);
        assert_eq!(entry["readOnly"], true);
        queue(
            app,
            "workspace.moveDocuments",
            None,
            json!({"documentIds": [id], "parentId": null, "order": 9}),
        )
        .unwrap();
        assert_eq!(workspace_entry(dir, &id)["readOnly"], true);
    });
}

#[test]
fn derived_changed_title_rejects_before_hot_commit_and_leaves_snapshot_unchanged() {
    isolated(|app, dir| {
        let id = publish(app);
        let before = snapshot(app, dir, &id);
        for title in [
            json!("Renamed view"),
            Value::Null,
            json!(""),
            json!("  "),
            json!(false),
            json!(42),
            json!(42.0),
            json!({}),
            json!([]),
        ] {
            let journal_before = journal(app).len();
            rejected(queue(
                app,
                "workspace.updateDocument",
                Some(&id),
                json!({
                    "documentId": "decoy", "title": title, "order": 99,
                    "parentId": null, "readOnly": true, "force": true
                }),
            ));
            assert_eq!(snapshot(app, dir, &id), before);
            let after = journal(app);
            let events: Vec<Value> = std::str::from_utf8(&after[journal_before..])
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(events.len(), 2);
            assert_eq!(events[0]["status"], "queued");
            assert_eq!(events[1]["status"], "failed");
            assert_eq!(
                app.state::<CrdtOperationQueue>()
                    .retry_wait_count_for_test()
                    .unwrap(),
                0
            );
        }
    });
}

#[test]
fn derived_same_title_echo_uses_workspace_coercion_and_default_semantics() {
    isolated(|app, dir| {
        for (id, title, echoes) in [
            (
                "default-title-view",
                "Untitled",
                vec![Value::Null, json!(""), json!("  "), json!({}), json!([])],
            ),
            (
                "numeric-title-view",
                "42",
                vec![json!(42), json!(42.0), json!(" 42 ")],
            ),
            ("boolean-title-view", "false", vec![json!(false)]),
        ] {
            queue(
                app,
                "workspace.createDocument",
                None,
                json!({
                    "documentId": id, "title": title, "readOnly": true
                }),
            )
            .unwrap();
            crate::geist_derived_view::register(dir, id).unwrap();
            let manifest = dir.join("documents").join(id).join("document.json");
            let record_before = fs::read(&manifest).unwrap();
            for echo in echoes {
                queue(
                    app,
                    "workspace.updateDocument",
                    Some(id),
                    json!({
                        "title": echo, "order": 3, "readOnly": true
                    }),
                )
                .unwrap();
                assert_eq!(workspace_entry(dir, id)["title"], title);
                assert_eq!(workspace_entry(dir, id)["readOnly"], true);
                assert_eq!(fs::read(&manifest).unwrap(), record_before);
            }
        }
    });
}

#[test]
fn ordinary_read_only_document_can_become_editable() {
    isolated(|app, dir| {
        let id = "ordinary-read-only";
        queue(
            app,
            "workspace.createDocument",
            None,
            json!({"documentId": id, "title": "Ordinary", "readOnly": true}),
        )
        .unwrap();
        assert_eq!(workspace_entry(dir, id)["readOnly"], true);
        let result = block_on(
            crate::mcp_workspace_crdt_service::mcp_local_make_document_editable(
                app.clone(),
                &json!({
                    "graph_id": GRAPH, "document_id": id
                }),
            ),
        )
        .unwrap();
        assert_eq!(result["success"], true);
        assert_eq!(workspace_entry(dir, id)["readOnly"], false);
    });
}

#[test]
fn derived_replayed_create_aliases_cannot_bypass_guard() {
    isolated(|app, dir| {
        let id = publish(app);
        let before = snapshot(app, dir, &id);
        // Live enqueue fills documentId; replay/legacy operations can still
        // contain the aliases the underlying upsert accepts.
        for key in ["documentId", "document_id", "id"] {
            let mut payload = json!({"documentId": null, "read_only": false,
                "title": "Must not write", "order": 1, "updatedAt": 1, "force": true});
            payload[key] = json!(id);
            rejected(block_on(crate::crdt_engine::executor::apply_operation(
                app,
                &crate::crdt_queue::CrdtOperation {
                    operation_id: format!("alias-{key}"),
                    kind: "workspace.createDocument".into(),
                    graph_id: GRAPH.into(),
                    document_id: Some("decoy".into()),
                    payload,
                    enqueue_timestamp: crate::clock::timestamp(),
                },
            )));
            assert_eq!(snapshot(app, dir, &id), before);
        }
    });
}

#[test]
fn derived_batch_register_rejects_before_any_entry_is_written() {
    isolated(|app, dir| {
        let id = publish(app);
        queue(
            app,
            "workspace.createDocument",
            None,
            json!({
                "documentId": "ordinary-batch-entry", "title": "Ordinary", "readOnly": true
            }),
        )
        .unwrap();
        let prepared = queue(
            app,
            "document.batchPrepare",
            None,
            json!({
                "clientBatchKey": "derived-batch-test", "folderPaths": []
            }),
        )
        .unwrap();
        let before = snapshot(app, dir, &id);
        for key in ["readOnly", "read_only"] {
            let mut derived = json!({"document_id": id, "relative_path": "song.md", "force": true});
            derived[key] = json!("false");
            rejected(queue(
                app,
                "document.batchRegister",
                None,
                json!({
                    "batchId": prepared["batchId"], "documents": [
                        {"documentId": "ordinary-batch-entry", "relativePath": "ordinary.md", "readOnly": false},
                        derived
                    ]
                }),
            ));
            assert_eq!(snapshot(app, dir, &id), before);
            assert_eq!(
                workspace_entry(dir, "ordinary-batch-entry")["readOnly"],
                true
            );
            assert_eq!(
                app.state::<CrdtOperationQueue>()
                    .retry_wait_count_for_test()
                    .unwrap(),
                0
            );
        }
    });
}

#[test]
fn derived_exponent_form_id_cannot_bypass_update_or_batch_register_guard() {
    isolated(|app, dir| {
        let id = "0.0000001";
        queue(
            app,
            "workspace.createDocument",
            None,
            json!({
                "documentId": id, "title": "Exponent ID view", "readOnly": true
            }),
        )
        .unwrap();
        crate::geist_derived_view::register(dir, id).unwrap();
        let prepared = queue(
            app,
            "document.batchPrepare",
            None,
            json!({
                "clientBatchKey": "exponent-id-batch", "folders": []
            }),
        )
        .unwrap();
        let before = snapshot(app, dir, id);
        let exponent_id: Value = serde_json::from_str("1e-7").unwrap();
        for flag_key in ["readOnly", "read_only"] {
            let mut update = json!({"documentId": exponent_id, "order": 99, "force": true});
            update[flag_key] = json!(false);
            rejected(queue(app, "workspace.updateDocument", None, update));
            assert_eq!(snapshot(app, dir, id), before);
            for id_key in ["documentId", "document_id"] {
                let mut entry = json!({"relativePath": "view.md", "force": true});
                entry[id_key] = exponent_id.clone();
                entry[flag_key] = json!(false);
                rejected(queue(
                    app,
                    "document.batchRegister",
                    None,
                    json!({
                        "batchId": prepared["batchId"], "documents": [entry]
                    }),
                ));
                assert_eq!(snapshot(app, dir, id), before);
            }
        }
        assert_eq!(workspace_entry(dir, id)["readOnly"], true);
        let queue = app.state::<CrdtOperationQueue>();
        assert_eq!(queue.counts_for_test().unwrap(), (0, 0));
        assert_eq!(queue.retry_wait_count_for_test().unwrap(), 0);
    });
}

#[test]
fn legacy_derived_capability_without_marker_still_rejects_editability() {
    isolated(|app, dir| {
        let id = publish(app);
        fs::remove_file(
            dir.join("narrative/derived-views")
                .join(format!("{id}.json")),
        )
        .unwrap();
        assert!(crate::geist_derived_view::is_derived(dir, &id).unwrap());
        let before = snapshot(app, dir, &id);
        let journal_before = journal(app);
        rejected(block_on(
            crate::mcp_workspace_crdt_service::mcp_local_make_document_editable(
                app.clone(),
                &json!({"graph_id": GRAPH, "document_id": id, "force": true}),
            ),
        ));
        assert_eq!(journal(app), journal_before);
        rejected(queue(
            app,
            "workspace.updateDocument",
            Some(&id),
            json!({"read_only": false}),
        ));
        assert_eq!(snapshot(app, dir, &id), before);
    });
}
