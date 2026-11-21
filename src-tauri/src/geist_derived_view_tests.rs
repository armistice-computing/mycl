//! Live derived-view regressions; never use the caller's real profile.
#![cfg(all(test, feature = "headless"))]

use crate::{
    app_runtime::{async_runtime::block_on, AppHandle},
    crdt_engine::{
        builder, projection,
        rooms::{Room, RoomRegistry},
    },
    document_service::{save_document, DocumentRecord, SaveDocumentInput},
    geist_song_service::mcp_local_sing,
    graph_service::{create_graph_service, read_graph_record, CreateGraphInput},
    paths::document_ydoc_state_path,
};
use serde_json::{json, Value};
use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};
use yrs::{
    sync::{Message, SyncMessage},
    updates::decoder::Decode,
    Doc, ReadTxn, StateVector, Transact, Update, WriteTxn,
};

const GRAPH: &str = "geist-derived-view-rehearsal";

#[test]
fn derived_access_metadata_reaches_workspace_and_all_document_read_formats() {
    isolated(|app, graph_dir| {
        sing(app, "Access metadata must reach the reader.");
        let ordinary = crate::document_service::create_document(app.clone(),
            serde_json::from_value(json!({"graphId":GRAPH,"documentId":"imported", "title":"Imported document"})).unwrap()).unwrap();
        block_on(crate::crdt_engine::workspace_ops::upsert_document_entry(app, GRAPH,
            &json!({"documentId":"imported","title":"Imported document","readOnly":true,"order":0,"updatedAt":1}),
            "access-test")).unwrap();
        let assert_access = |value: &Value, derived: bool| {
            assert_eq!(value["readOnly"], true, "{value}");
            assert_eq!(value["derived"], derived, "{value}");
            assert_eq!(value["readOnlyOwner"], if derived { json!("geist") } else { Value::Null });
        };
        let workspace = block_on(crate::workspace_projection_service::mcp_local_get_workspace(app.clone(),
            &json!({"graphId":GRAPH,"depth":0}))).unwrap();
        let docs = workspace["documents"].as_array().unwrap();
        assert_access(docs.iter().find(|d|d["id"]=="geist-song").unwrap(), true);
        assert_access(docs.iter().find(|d|d["id"]=="imported").unwrap(), false);
        assert_eq!(workspace["page"]["serializedBytes"], serde_json::to_vec(&workspace).unwrap().len());
        let listed = crate::document_mcp_service::mcp_local_list_documents(app.clone(), &json!({"graphId":GRAPH})).unwrap();
        for (id, derived) in [("geist-song",true),("imported",false)] {
            assert_access(listed["documents"].as_array().unwrap().iter().find(|d|d["documentId"]==id).unwrap(), derived);
            for format in ["markdown", "xml", "json", "ids_only"] {
                let read = crate::document_mcp_service::mcp_local_read_document(app.clone(),
                    &json!({"graphId":GRAPH,"documentId":id,"format":format})).unwrap();
                assert_access(&read, derived);
            }
            let hosted = crate::document_hosted_projection::hosted_document_response(app, GRAPH, id).unwrap();
            assert_access(&serde_json::to_value(hosted).unwrap(), derived);
            let digest = block_on(crate::document_digest_service::mcp_local_document_digest(app.clone(),
                &json!({"graphId":GRAPH,"documentId":id}))).unwrap();
            assert_access(&digest["metadata"], derived);
        }
        let records = crate::document_service::read_graph_documents_cold(graph_dir).unwrap();
        let mut forged = vec![json!({"id":"imported","readOnly":false,"derived":true,"readOnlyOwner":"geist"}),
            json!({"id":"geist-song","readOnly":false,"derived":false,"readOnlyOwner":null})];
        crate::document_access::enrich_workspace_documents(graph_dir, &mut forged, &records).unwrap();
        assert_eq!(forged[0]["derived"], false);
        assert_eq!(forged[0]["readOnly"], false);
        assert_eq!(forged[0]["readOnlyOwner"], Value::Null);
        assert_access(&forged[1], true);
        fs::remove_file(graph_dir.join("narrative/derived-views/geist-song.json")).unwrap();
        let song_record = records.iter().find(|r|r.document_id=="geist-song").unwrap();
        assert!(crate::document_access::resolve(graph_dir,"geist-song",Some(song_record),false).unwrap().derived,
            "legacy capability-only views must remain recognizable");
        assert!(!crate::document_access::resolve(graph_dir,"imported",Some(&ordinary),true).unwrap().derived);
    });
}

#[test]
fn geist_derived_archive_failure_reports_committed_memory_with_warning() {
    isolated(|app, graph_dir| {
        for content in ["first retained memory", "second retained memory"] {
            block_on(crate::geist_memory_service::mcp_local_remember(
                app.clone(),
                &json!({"graph_id":GRAPH,"content":content}),
                None,
            ))
            .unwrap();
        }
        fs::create_dir_all(graph_dir.join("narrative")).unwrap();
        fs::write(
            graph_dir.join("narrative/derived-views"),
            b"blocked publication directory",
        )
        .unwrap();
        let result = crate::geist_memory_service::mcp_local_archive_memories(
            app.clone(),
            &json!({"graph_id":GRAPH,"keep":1}),
        )
        .unwrap();
        assert_eq!(result["archived"], 1);
        assert!(!result["projection_warnings"].as_array().unwrap().is_empty());
        let store = crate::geist_memory_store::read_memory_store(graph_dir, GRAPH).unwrap();
        assert_eq!(store.memories.len(), 1);
        assert_eq!(store.archives.len(), 1);
        let archived = &store.archives[0].memories[0];
        let recalled = crate::geist_memory_recall_service::mcp_local_recall_memories(
            app.clone(),
            &json!({"graph_id":GRAPH,"number":archived.number}),
        )
        .unwrap();
        assert_eq!(recalled["memories"][0]["content"], archived.content);
        let repeat = crate::geist_memory_service::mcp_local_archive_memories(
            app.clone(),
            &json!({"graph_id":GRAPH,"keep":1}),
        )
        .unwrap();
        assert_eq!(repeat["archived"], 0);
        assert_eq!(
            crate::geist_memory_store::read_memory_store(graph_dir, GRAPH)
                .unwrap()
                .archives
                .len(),
            1
        );
    });
}

#[test]
fn geist_derived_publishers_work_through_async_mcp_dispatch() {
    isolated(|app, graph_dir| {
        let jobs = Arc::new(
            crate::local_jobs::LocalJobRegistry::new(graph_dir.join("test-jobs")).unwrap(),
        );
        let ctx = crate::mcp_dispatch_registry::McpCallCtx {
            app: app.clone(),
            jobs: &jobs,
            credential_actor: None,
            token_id: None,
        };
        block_on(async {
            let song = (crate::mcp_dispatch_registry::lookup("sing")
                .unwrap()
                .handler)(
                &ctx,
                &json!({"graph_id":GRAPH,"verse":"Published from the async MCP handler."}),
            )
            .await
            .unwrap();
            assert_eq!(song["projection_warnings"], json!([]));
            for content in ["first memory", "second memory"] {
                crate::geist_memory_service::mcp_local_remember(
                    app.clone(),
                    &json!({"graph_id":GRAPH,"content":content}),
                    None,
                )
                .await
                .unwrap();
            }
            let archive = (crate::mcp_dispatch_registry::lookup("archive_memories")
                .unwrap()
                .handler)(&ctx, &json!({"graph_id":GRAPH,"keep":1}))
            .await
            .unwrap();
            assert_eq!(archive["archived"], 1);
            assert_eq!(archive["projection_warnings"], json!([]));
        });
        assert!(crate::geist_derived_view::is_derived(graph_dir, "geist-song").unwrap());
    });
}

struct IsolatedProfile {
    path: PathBuf,
    previous: Option<OsString>,
}

impl IsolatedProfile {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("garden-geist-derived-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path).expect("reserve fresh isolated profile");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let previous = std::env::var_os("GARDEN_PROFILE_DIR");
        std::env::set_var("GARDEN_PROFILE_DIR", &path);
        Self { path, previous }
    }
}

impl Drop for IsolatedProfile {
    fn drop(&mut self) {
        // A failed queued operation may still drain: never expose the real profile.
        if std::thread::panicking() {
            eprintln!(
                "failed derived-view profile retained at {}",
                self.path.display()
            );
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
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let profile = IsolatedProfile::new();
    let app = crate::tauri_runtime::build_mock_app_for_tests(true);
    create_graph_service(
        &app,
        CreateGraphInput {
            graph_id: Some(GRAPH.into()),
            title: "Isolated derived Geist views".into(),
            description: None,
            operation_id: None,
        },
    )
    .expect("create isolated graph");
    let (graph_dir, _) = read_graph_record(&app, GRAPH).unwrap();
    assert!(graph_dir
        .canonicalize()
        .unwrap()
        .starts_with(profile.path.canonicalize().unwrap()));
    test(&app, &graph_dir);
}

fn sing(app: &AppHandle, verse: &str) {
    // The service owns block_on internally; never call it inside an async block.
    let result = mcp_local_sing(app.clone(), &json!({"graph_id": GRAPH, "verse": verse})).unwrap();
    assert_eq!(
        result["projection_warnings"],
        json!([]),
        "happy-path publication must not silently fail: {result}"
    );
}

#[test]
fn invalid_song_inputs_do_not_rotate_or_touch_authority_or_views() {
    isolated(|app, graph_dir| {
        for text in ["First voice", "Second voice", "Third voice"] {
            sing(app, text);
        }
        let store_path = graph_dir.join("narrative/song.json");
        let view_path = graph_dir.join("documents/geist-song/document.json");
        let before_store = fs::read(&store_path).unwrap();
        let before_view = fs::read(&view_path).unwrap();
        for mode in ["verse", "counterpoint", "coda"] {
            for mut args in [
                json!({"content":"A real verse under the wrong field"}),
                json!({}), json!({"verse":""}), json!({"verse":" \n\t "}),
                json!({"verse":null}), json!({"verse":42}),
                json!({"verse":true}), json!({"verse":[]}),
                json!({"verse":"Correct field", "content":"Ambiguous extra"}),
            ] {
                args["graph_id"] = json!(GRAPH);
                args["mode"] = json!(mode);
                args["verse_index"] = json!(0);
                assert!(mcp_local_sing(app.clone(), &args).is_err(), "{args}");
                assert_eq!(fs::read(&store_path).unwrap(), before_store, "{args}");
                assert_eq!(fs::read(&view_path).unwrap(), before_view, "{args}");
            }
        }
        for invalid_mode in [json!(false), json!(""), json!(" "), Value::Null] {
            assert!(mcp_local_sing(app.clone(), &json!({
                "graph_id":GRAPH,"verse":"Do not silently default an invalid mode", "mode":invalid_mode
            })).is_err());
        }
        assert_eq!(fs::read(&store_path).unwrap(), before_store);
        sing(app, "A valid fourth voice");
        let store: Value = serde_json::from_slice(&fs::read(&store_path).unwrap()).unwrap();
        assert_eq!(store["verses"][0]["text"], "A valid fourth voice");
        assert_eq!(store["archives"].as_array().unwrap().last().unwrap()["verses"][0]["text"], "First voice");
    });
}

/// Fourteen lines at most, as the Song displays them (2026-10-16): newlines
/// and `/` line marks both count; "and/or" does not. Over the limit nothing
/// changes, in every mode.
#[test]
fn a_song_part_over_fourteen_lines_is_refused_without_changing_anything() {
    isolated(|app, graph_dir| {
        sing(app, "First voice");
        let store_path = graph_dir.join("narrative/song.json");
        let before = fs::read(&store_path).unwrap();
        let fifteen_lines = (1..=15).map(|n| format!("line {n}")).collect::<Vec<_>>().join("\n");
        let fifteen_marks = (1..=15).map(|n| format!("line {n}.")).collect::<Vec<_>>().join(" / ");
        for text in [&fifteen_lines, &fifteen_marks] {
            for mode in ["verse", "counterpoint", "coda"] {
                let error = mcp_local_sing(app.clone(), &json!({
                    "graph_id": GRAPH, "verse": text, "mode": mode, "verse_index": 0
                }))
                .expect_err("over fourteen lines");
                assert!(error.contains("at most 14 lines"), "{error}");
                assert_eq!(fs::read(&store_path).unwrap(), before, "{mode}");
            }
        }
        let fourteen = (1..=14).map(|n| format!("line {n} and/or more")).collect::<Vec<_>>().join("\n");
        sing(app, &fourteen);
        let store: Value = serde_json::from_slice(&fs::read(&store_path).unwrap()).unwrap();
        assert_eq!(store["verses"][0]["text"], fourteen.as_str());
    });
}

fn registered_room(app: &AppHandle, document_id: &str) -> Arc<Room> {
    block_on(
        app.state::<RoomRegistry>()
            .peek(&format!("doc:{GRAPH}:{document_id}")),
    )
    .expect("publisher must register its room, not just write a cold sidecar")
}

fn client_from_state(bytes: &[u8]) -> Doc {
    let doc = Doc::new();
    doc.transact_mut()
        .apply_update(Update::decode_v1(bytes).unwrap())
        .unwrap();
    doc
}

fn full_state(doc: &Doc) -> Vec<u8> {
    doc.transact()
        .encode_state_as_update_v1(&StateVector::default())
}

fn content(doc: &Doc, document_id: &str) -> Value {
    projection::materialize_ydoc(doc, document_id).tiptap_json
}

fn apply_broadcast(client: &Doc, updates: &mut tokio::sync::broadcast::Receiver<Vec<u8>>) {
    let wire = updates
        .try_recv()
        .expect("synchronous publisher must emit a room update");
    let Message::Sync(SyncMessage::Update(update)) = Message::decode_v1(&wire).unwrap() else {
        panic!("expected a y-websocket Sync Update");
    };
    client
        .transact_mut()
        .apply_update(Update::decode_v1(&update).unwrap())
        .unwrap();
}

fn record(graph_dir: &Path, document_id: &str) -> DocumentRecord {
    serde_json::from_slice(
        &fs::read(
            graph_dir
                .join("documents")
                .join(document_id)
                .join("document.json"),
        )
        .unwrap(),
    )
    .unwrap()
}

fn assert_projected_content(graph_dir: &Path, document_id: &str, expected: &Value) {
    let persisted = fs::read(document_ydoc_state_path(graph_dir, document_id)).unwrap();
    let file_doc = client_from_state(&persisted);
    assert_eq!(
        &content(&file_doc, document_id),
        expected,
        "sidecar must contain latest projection"
    );
    let saved = record(graph_dir, document_id);
    assert_eq!(
        saved.tiptap_json.as_ref(),
        Some(expected),
        "record must contain latest projection"
    );
    assert_eq!(
        saved.body,
        projection::materialize_ydoc(&file_doc, document_id).body
    );
}

fn assert_read_only(error: String) {
    assert!(
        error.to_ascii_lowercase().contains("read-only"),
        "wrong rejection: {error}"
    );
}

#[test]
fn geist_derived_song_updates_same_room_and_rejects_stale_writers() {
    isolated(|app, graph_dir| {
        let id = crate::geist_song_store::song_doc_id("");
        sing(app, "First isolated verse.");
        let room = registered_room(app, &id);
        let old_state = block_on(room.encode_state_for_test());
        let viewer = client_from_state(&old_state);
        let old_client = client_from_state(&old_state);
        let stale_record = record(graph_dir, &id);
        let mut updates = room.subscribe_updates_for_test();
        room.force_projection_rebuild();
        assert!(
            room.needs_projection_flush(),
            "simulate an outstanding pre-publication flush"
        );

        sing(app, "Second isolated verse must survive.");
        assert!(
            Arc::ptr_eq(&room, &registered_room(app, &id)),
            "publication must not replace an open room"
        );
        apply_broadcast(&viewer, &mut updates);
        let latest = block_on(room.with_doc(|doc| content(doc, &id)));
        assert_eq!(
            content(&viewer, &id),
            latest,
            "old client must integrate the incremental update"
        );
        assert!(latest
            .to_string()
            .contains("Second isolated verse must survive."));
        assert_ne!(content(&old_client, &id), latest);
        let canonical = fs::read(graph_dir.join("narrative/song.json")).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&canonical).unwrap()["verses"][0]["text"],
            "Second isolated verse must survive."
        );
        let live_bytes = block_on(room.encode_state_for_test());
        let file_bytes = fs::read(document_ydoc_state_path(graph_dir, &id)).unwrap();
        let record_bytes =
            fs::read(graph_dir.join("documents").join(&id).join("document.json")).unwrap();

        assert!(!block_on(room.apply_client_update(&old_state))
            .expect("known old SyncStep2 is a legal handshake"));
        assert!(!block_on(room.apply_client_update(&full_state(&viewer)))
            .expect("current SyncStep2 is a legal handshake"));
        {
            let mut txn = old_client.transact_mut();
            let fragment = txn.get_or_insert_xml_fragment("content");
            builder::append_nodes(
                &mut txn,
                &fragment,
                &[
                    json!({"type":"paragraph", "content":[{"type":"text", "text":"Stale client must not write this."}]}),
                ],
            );
        }
        assert_read_only(
            block_on(room.apply_client_update(&full_state(&old_client)))
                .expect_err("mutated old client must be rejected"),
        );
        let mut called = false;
        assert_read_only(
            block_on(room.update_doc(|_, _| {
                called = true;
                Ok(())
            }))
            .expect_err("ordinary server mutation must be rejected"),
        );
        assert!(!called, "read-only check must precede mutation closure");
        let mut stale_input = serde_json::to_value(stale_record).unwrap();
        stale_input["force"] = json!(true);
        let stale_input: SaveDocumentInput = serde_json::from_value(stale_input).unwrap();
        assert_read_only(
            save_document(app.clone(), stale_input)
                .expect_err("force cannot bypass derived-view ownership"),
        );
        let queued = block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(5),
                crate::crdt_queue::enqueue_crdt_operation(app.clone(), crate::crdt_operation_types::EnqueueCrdtOperationInput {
                    kind: "document.write".into(), graph_id: GRAPH.into(), document_id: Some(id.clone()),
                    payload: json!({"title":"The Song","content":"Queued stale write","force":true}),
                })
            ).await.expect("read-only rejection must be terminal, not retried forever")
        });
        assert_read_only(queued.expect_err("queued writes cannot change a derived view"));

        assert_eq!(
            block_on(room.encode_state_for_test()),
            live_bytes,
            "rejected writes must leave live bytes untouched"
        );
        assert_eq!(
            fs::read(document_ydoc_state_path(graph_dir, &id)).unwrap(),
            file_bytes
        );
        assert_eq!(
            fs::read(graph_dir.join("documents").join(&id).join("document.json")).unwrap(),
            record_bytes
        );
        assert!(
            matches!(
                updates.try_recv(),
                Err(tokio::sync::broadcast::error::TryRecvError::Empty)
            ),
            "handshakes and rejected writes must not broadcast"
        );
        assert_projected_content(graph_dir, &id, &latest);

        // Force real pending work after publication too, so this cannot pass by
        // short-circuiting a clean room left by the publisher.
        room.force_projection_rebuild();
        assert!(room.needs_projection_flush());
        block_on(crate::crdt_projection_flush::flush_document_projection(
            app.clone(),
            GRAPH,
            &id,
        ))
        .expect("pending flush must accept the latest derived projection");
        assert!(!room.needs_projection_flush());
        assert!(Arc::ptr_eq(&room, &registered_room(app, &id)));
        assert_eq!(block_on(room.with_doc(|doc| content(doc, &id))), latest);
        assert_projected_content(graph_dir, &id, &latest);
        assert_eq!(
            fs::read(graph_dir.join("narrative/song.json")).unwrap(),
            canonical,
            "projection flush must not rewrite canonical Song JSON"
        );
    });
}

#[test]
fn geist_derived_song_archive_republishes_into_subscribed_room() {
    isolated(|app, graph_dir| {
        sing(app, "First archive fixture verse.");
        sing(app, "Second archive fixture verse.");
        sing(app, "Third archive fixture verse.");
        let id = crate::geist_song_store::past_songs_doc_id("");
        let room = registered_room(app, &id);
        let client = client_from_state(&block_on(room.encode_state_for_test()));
        let before = content(&client, &id);
        let mut updates = room.subscribe_updates_for_test();
        sing(app, "Fourth archive fixture verse.");
        assert!(Arc::ptr_eq(&room, &registered_room(app, &id)));
        apply_broadcast(&client, &mut updates);
        let latest = block_on(room.with_doc(|doc| content(doc, &id)));
        assert_ne!(latest, before);
        assert!(latest.to_string().contains("First archive fixture verse."));
        assert_eq!(content(&client, &id), latest);
        assert_projected_content(graph_dir, &id, &latest);
        assert_read_only(
            block_on(room.update_doc(|_, _| Ok(()))).expect_err("Song archive is server-owned"),
        );
    });
}

#[test]
fn geist_derived_memory_archive_publisher_updates_same_room() {
    isolated(|app, graph_dir| {
        let id = "geist-memory-archive-isolated";
        let mut memories = vec![crate::geist_memory_store::LocalMemoryRecord {
            anchors: Vec::new(),
            number: 1,
            block_id: "memory-one".into(),
            content: "First isolated archived memory.".into(),
            created_at: "1000".into(),
            last_active: "2000".into(),
            rememberer: None,
        }];
        crate::geist_memory_archive::write_memory_archive_document(
            app, graph_dir, GRAPH, id, "3000", 0, &memories,
        )
        .unwrap();
        let room = registered_room(app, id);
        let stale_state = block_on(room.encode_state_for_test());
        let stale_record = record(graph_dir, id);
        let client = client_from_state(&stale_state);
        let mut updates = room.subscribe_updates_for_test();
        memories.push(crate::geist_memory_store::LocalMemoryRecord {
            anchors: Vec::new(),
            number: 2,
            block_id: "memory-two".into(),
            content: "Latest isolated archived memory.".into(),
            created_at: "2000".into(),
            last_active: "3000".into(),
            rememberer: None,
        });
        crate::geist_memory_archive::write_memory_archive_document(
            app, graph_dir, GRAPH, id, "3000", 0, &memories,
        )
        .unwrap();
        assert!(Arc::ptr_eq(&room, &registered_room(app, id)));
        apply_broadcast(&client, &mut updates);
        let latest = block_on(room.with_doc(|doc| content(doc, id)));
        assert!(latest
            .to_string()
            .contains("Latest isolated archived memory."));
        assert_eq!(content(&client, id), latest);
        assert_projected_content(graph_dir, id, &latest);
        assert_read_only(
            block_on(room.update_doc(|_, _| Ok(()))).expect_err("memory archive is server-owned"),
        );
        let marker_path = graph_dir
            .join("narrative/derived-views")
            .join(format!("{id}.json"));
        let marker_before =
            fs::read(&marker_path).expect("archive has a server-owned registration");
        let live_before = block_on(room.encode_state_for_test());
        let file_before = fs::read(document_ydoc_state_path(graph_dir, id)).unwrap();
        let record_path = graph_dir.join("documents").join(id).join("document.json");
        let record_before = fs::read(&record_path).unwrap();
        let mut input = serde_json::to_value(stale_record).unwrap();
        input["force"] = json!(true);
        input["source"] = json!("ordinary-document");
        input["readOnly"] = json!(false);
        input["capabilities"] = json!(["document.local.write"]);
        assert_read_only(
            save_document(app.clone(), serde_json::from_value(input).unwrap()).expect_err(
                "caller-supplied source/capabilities/force cannot override archive registration",
            ),
        );
        use base64::Engine as _;
        assert_read_only(
            crate::document_service::save_document_ydoc_state(
                app.clone(),
                crate::document_service::SaveDocumentYDocStateInput {
                    graph_id: GRAPH.into(),
                    document_id: id.into(),
                    ydoc_update_base64: base64::engine::general_purpose::STANDARD
                        .encode(&stale_state),
                    trace_operation_id: None,
                },
            )
            .expect_err("direct sidecar save cannot replace archive authority"),
        );
        assert_eq!(fs::read(&marker_path).unwrap(), marker_before);
        assert_eq!(block_on(room.encode_state_for_test()), live_before);
        assert_eq!(
            fs::read(document_ydoc_state_path(graph_dir, id)).unwrap(),
            file_before
        );
        assert_eq!(fs::read(&record_path).unwrap(), record_before);
        assert!(matches!(
            updates.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
    });
}

#[test]
fn geist_derived_song_collision_warns_without_overwriting_ordinary_document() {
    isolated(|app, graph_dir| {
        let id = crate::geist_song_store::song_doc_id("");
        let ordinary = crate::document_service::create_document(
            app.clone(),
            crate::document_service::CreateDocumentInput {
                graph_id: GRAPH.into(),
                document_id: Some(id.clone()),
                title: "Not a Geist view".into(),
            },
        )
        .unwrap();
        let mut input = serde_json::to_value(ordinary).unwrap();
        input["body"] = json!("This ordinary document belongs to the user.");
        input["tiptapJson"] = json!({"type":"doc", "content":[{"type":"paragraph", "content":[{"type":"text", "text":"This ordinary document belongs to the user."}]}]});
        input["ydocUpdateBase64"] = Value::Null;
        input["tiptapXml"] = Value::Null;
        input["tree"] = Value::Null;
        input["blocks"] = Value::Null;
        save_document(app.clone(), serde_json::from_value(input).unwrap()).unwrap();
        let path = document_ydoc_state_path(graph_dir, &id);
        let file_before = fs::read(&path).unwrap();
        let record_path = graph_dir.join("documents").join(&id).join("document.json");
        let record_before = fs::read(&record_path).unwrap();
        let room = block_on(
            app.state::<RoomRegistry>()
                .get_or_create(&format!("doc:{GRAPH}:{id}"), path.clone()),
        )
        .unwrap();
        let live_before = block_on(room.encode_state_for_test());
        let mut updates = room.subscribe_updates_for_test();

        let result = mcp_local_sing(
            app.clone(),
            &json!({"graph_id":GRAPH,"verse":"Canonical Song commits despite a view collision."}),
        )
        .expect("canonical write succeeds independently of projection failure");
        let warnings = result["projection_warnings"]
            .as_array()
            .expect("warnings are visible in response");
        assert!(
            warnings.iter().any(|warning| warning
                .as_str()
                .is_some_and(|s| s.contains("ordinary document") && s.contains(&id))),
            "collision must be explicit: {result}"
        );
        let canonical: Value =
            serde_json::from_slice(&fs::read(graph_dir.join("narrative/song.json")).unwrap())
                .unwrap();
        assert_eq!(
            canonical["verses"][0]["text"],
            "Canonical Song commits despite a view collision."
        );
        assert_eq!(fs::read(&path).unwrap(), file_before);
        assert_eq!(fs::read(&record_path).unwrap(), record_before);
        assert_eq!(block_on(room.encode_state_for_test()), live_before);
        assert!(Arc::ptr_eq(&room, &registered_room(app, &id)));
        assert!(
            !crate::geist_derived_view::is_derived(graph_dir, &id).unwrap(),
            "collision must not claim the user's document"
        );
        assert!(matches!(
            updates.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
        block_on(room.update_doc(|_, _| Ok(()))).expect("ordinary document remains editable");
    });
}

fn hot_only_song_collision(keep_sidecar: bool) {
    isolated(|app, graph_dir| {
        let id = crate::geist_song_store::song_doc_id("");
        let path = document_ydoc_state_path(graph_dir, &id);
        let room = block_on(
            app.state::<RoomRegistry>()
                .get_or_create(&format!("doc:{GRAPH}:{id}"), path.clone()),
        )
        .unwrap();
        block_on(room.update_doc(|_, txn| {
            let fragment = txn.get_or_insert_xml_fragment("content");
            builder::append_nodes(
                txn,
                &fragment,
                &[json!({"type":"paragraph", "content":[{
                    "type":"text", "text":"Unflushed ordinary content must survive."
                }]})],
            );
            Ok(())
        }))
        .unwrap();
        let record_path = graph_dir.join("documents").join(&id).join("document.json");
        let marker_path = graph_dir
            .join("narrative/derived-views")
            .join(format!("{id}.json"));
        assert!(
            !record_path.exists(),
            "fixture has hot authority, not a cold document manifest"
        );
        assert!(!marker_path.exists());
        assert!(block_on(
            room.with_doc(|doc| !doc.transact().state_vector().is_empty())
        ));
        assert!(
            room.needs_projection_flush(),
            "ordinary content has not been projected"
        );
        let live_before = block_on(room.encode_state_for_test());
        let file_before = fs::read(&path).unwrap();
        if !keep_sidecar {
            // Remove only this fixture's sidecar to isolate the live-state guard.
            // No room operation follows until the attempted Geist publication.
            fs::remove_file(&path).unwrap();
        }
        let mut updates = room.subscribe_updates_for_test();
        let result = mcp_local_sing(
            app.clone(),
            &json!({
                "graph_id":GRAPH, "verse":"Canonical Song survives a hot-only collision."
            }),
        )
        .expect("native Song commits even when the view ID is occupied");
        let warnings = result["projection_warnings"].as_array().unwrap();
        assert!(
            warnings.iter().any(|warning| warning
                .as_str()
                .is_some_and(|s| s.contains("ordinary document") && s.contains(&id))),
            "collision warning missing: {result}"
        );
        let canonical: Value =
            serde_json::from_slice(&fs::read(graph_dir.join("narrative/song.json")).unwrap())
                .unwrap();
        assert_eq!(
            canonical["verses"][0]["text"],
            "Canonical Song survives a hot-only collision."
        );
        assert!(Arc::ptr_eq(&room, &registered_room(app, &id)));
        assert_eq!(block_on(room.encode_state_for_test()), live_before);
        if keep_sidecar {
            assert_eq!(fs::read(&path).unwrap(), file_before);
        } else {
            assert!(
                !path.exists(),
                "rejected publisher must not manufacture a sidecar"
            );
        }
        assert!(
            !record_path.exists(),
            "rejected publisher must not manufacture a document record"
        );
        assert!(
            !marker_path.exists(),
            "rejected publisher must not claim ordinary authority"
        );
        assert!(
            room.needs_projection_flush(),
            "collision must not mark ordinary pending work persisted"
        );
        assert!(matches!(
            updates.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
        block_on(room.update_doc(|_, _| Ok(()))).expect("ordinary hot room remains editable");
    });
}

#[test]
fn geist_derived_song_hot_sidecar_collision_preserves_unflushed_ordinary_document() {
    hot_only_song_collision(true);
}

#[test]
fn geist_derived_song_live_only_collision_preserves_unflushed_ordinary_document() {
    hot_only_song_collision(false);
}

#[test]
fn importance_view_quotes_valued_blocks_and_links_each_to_its_source() {
    use crate::geist_important_blocks::{rebuild_important_blocks_projection, IMPORTANT_BLOCKS_DOC_ID};
    isolated(|app, graph_dir| {
        block_on(crate::mcp_workspace_crdt_service::mcp_local_create_document(
            app.clone(),
            &json!({"graphId": GRAPH, "documentId": "source", "title": "Source notes"}),
        ))
        .unwrap();
        block_on(crate::document_mutation_service::mcp_local_write_document(
            app.clone(),
            &json!({
                "graph_id": GRAPH, "document_id": "source", "format": "xml",
                "content": "<paragraph data-block-id=\"block-keep\">The sentence that matters.</paragraph><paragraph data-block-id=\"block-pass\">An ordinary line.</paragraph>",
            }),
        ))
        .unwrap();
        crate::salience_mcp_valuation::mcp_local_value(
            app.clone(),
            &json!({"graph_id": GRAPH, "document_id": "source", "block_id": "block-keep", "importance": 5, "valence": -3}),
        )
        .unwrap();
        // A Song verse is a copy; the view must not quote copies, even valued.
        sing(app, "A verse that is only a copy.");
        let song = record(graph_dir, "geist-song");
        let verse_block = song.blocks.iter().find(|b| b.content.contains("only a copy")).expect("verse block").id.clone();
        crate::salience_mcp_valuation::mcp_local_value(
            app.clone(),
            &json!({"graph_id": GRAPH, "document_id": "geist-song", "block_id": verse_block, "importance": 5}),
        )
        .unwrap();

        assert_eq!(rebuild_important_blocks_projection(app, GRAPH).unwrap(), 1);
        // The second rebuild replaces the view in place.
        assert_eq!(rebuild_important_blocks_projection(app, GRAPH).unwrap(), 1);

        let view = record(graph_dir, IMPORTANT_BLOCKS_DOC_ID);
        assert_eq!(view.title, "What Has Become Important");
        assert!(view.tiptap_xml.contains("The sentence that matters."), "{}", view.tiptap_xml);
        assert!(!view.tiptap_xml.contains("An ordinary line."));
        assert!(!view.tiptap_xml.contains("only a copy"), "valued Song lines stay out");
        assert!(view.tiptap_xml.contains("targetDocId=\"source\""), "{}", view.tiptap_xml);
        assert!(view.tiptap_xml.contains("targetBlockId=\"block-keep\""), "{}", view.tiptap_xml);
        assert!(crate::geist_derived_view::is_derived(graph_dir, IMPORTANT_BLOCKS_DOC_ID).unwrap());
        let read = crate::document_mcp_service::mcp_local_read_document(
            app.clone(),
            &json!({"graphId": GRAPH, "documentId": IMPORTANT_BLOCKS_DOC_ID, "format": "markdown"}),
        )
        .unwrap();
        assert_eq!(read["readOnly"], true);
        assert_eq!(read["readOnlyOwner"], "geist");
    });
}

#[test]
fn get_important_blocks_reports_what_the_person_rated() {
    isolated(|app, _graph_dir| {
        block_on(crate::mcp_workspace_crdt_service::mcp_local_create_document(
            app.clone(),
            &json!({"graphId": GRAPH, "documentId": "hand", "title": "hand"}),
        ))
        .unwrap();
        block_on(crate::document_mutation_service::mcp_local_write_document(
            app.clone(),
            &json!({"graph_id": GRAPH, "document_id": "hand", "format": "xml",
                    "content": "<paragraph data-block-id=\"block-mine\">Rated by hand.</paragraph>"}),
        ))
        .unwrap();
        // The person's own rating, as the block's gutter sends it. No agent rates it.
        let request = serde_json::from_value(json!({
            "documentId": "hand", "blockId": "block-mine", "importance": 5, "valence": -4
        }))
        .unwrap();
        crate::salience_route_service::set_local_user_valuation(app.clone(), GRAPH, request).unwrap();

        let tool = crate::salience_important_blocks::mcp_local_get_important_blocks(
            app.clone(),
            &json!({"graph_id": GRAPH, "limit": 20}),
        )
        .unwrap();
        let block = tool["blocks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|block| block["blockId"] == "block-mine")
            .unwrap_or_else(|| panic!("the rated block is listed: {tool}"));
        assert!(block["importance"].as_f64().unwrap() > 0.0, "{block}");
        assert!(block["valence"].as_f64().unwrap() < 0.0, "{block}");
        assert_eq!(block["userImportance"], 5.0);
        assert_eq!(block["userValence"], -4.0);

        // The charge filter follows the person's rating too.
        let negative = crate::salience_important_blocks::mcp_local_get_important_blocks(
            app.clone(),
            &json!({"graph_id": GRAPH, "limit": 20, "valence": "negative"}),
        )
        .unwrap();
        assert!(negative["blocks"].as_array().unwrap().iter().any(|block| block["blockId"] == "block-mine"), "{negative}");
        let positive = crate::salience_important_blocks::mcp_local_get_important_blocks(
            app.clone(),
            &json!({"graph_id": GRAPH, "limit": 20, "valence": "positive"}),
        )
        .unwrap();
        assert!(!positive["blocks"].as_array().unwrap().iter().any(|block| block["blockId"] == "block-mine"), "{positive}");
    });
}

#[test]
fn importance_view_scores_every_block_as_get_important_blocks_does() {
    use crate::geist_important_blocks::important_entries;
    isolated(|app, _graph_dir| {
        for (id, content) in [
            ("source", "<paragraph data-block-id=\"block-keep\">Valued high.</paragraph><paragraph data-block-id=\"block-low\">Valued low.</paragraph><paragraph data-block-id=\"block-wired\">Wired, never valued.</paragraph>"),
            ("other", "<paragraph data-block-id=\"block-far\">The other end of the wire.</paragraph>"),
        ] {
            block_on(crate::mcp_workspace_crdt_service::mcp_local_create_document(
                app.clone(),
                &json!({"graphId": GRAPH, "documentId": id, "title": id}),
            ))
            .unwrap();
            block_on(crate::document_mutation_service::mcp_local_write_document(
                app.clone(),
                &json!({"graph_id": GRAPH, "document_id": id, "format": "xml", "content": content}),
            ))
            .unwrap();
        }
        for (block_id, importance) in [("block-keep", 5), ("block-low", 1)] {
            crate::salience_mcp_valuation::mcp_local_value(
                app.clone(),
                &json!({"graph_id": GRAPH, "document_id": "source", "block_id": block_id, "importance": importance}),
            )
            .unwrap();
        }
        // A wired block nobody valued: its recency comes from its document's
        // creation date, which is where the view could drift from the tool.
        block_on(crate::mcp_workspace_mutation_service::mcp_local_create_wires(
            app.clone(),
            &json!({"graph_id": GRAPH, "source_document_id": "source", "source_block_id": "block-wired",
                    "target_document_id": "other", "target_block_id": "block-far", "predicate": "supports"}),
        ))
        .unwrap();

        let tool = crate::salience_important_blocks::mcp_local_get_important_blocks(
            app.clone(),
            &json!({"graph_id": GRAPH, "limit": 20}),
        )
        .unwrap();
        let expected: Vec<(String, String, f64)> = tool["blocks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| (b["documentId"].as_str().unwrap().to_string(), b["blockId"].as_str().unwrap().to_string(), b["score"].as_f64().unwrap()))
            .collect();
        let view: Vec<(String, String, f64)> = important_entries(app, GRAPH)
            .unwrap()
            .into_iter()
            .map(|e| (e.document_id, e.block_id, e.score))
            .collect();
        assert!(view.iter().any(|(_, block, _)| block == "block-wired"), "the wired block ranks: {view:?}");
        // The dates come from the RDF projection, matching the records exactly.
        let rdf = crate::geist_important_blocks::rdf_created_at(_graph_dir, GRAPH).unwrap();
        for id in ["source", "other"] {
            assert_eq!(rdf.get(id), Some(&record(_graph_dir, id).created_at), "{id}: {rdf:?}");
        }
        assert_eq!(view, expected, "same blocks, same order, same scores");
    });
}
