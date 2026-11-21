use super::*;
static FAILURE: std::sync::Mutex<Option<&'static str>> = std::sync::Mutex::new(None);
pub(super) fn fail_if_requested(stage: &str) -> Result<(), String> {
    let mut failure = FAILURE.lock().unwrap();
    if *failure == Some(stage) {
        *failure = None;
        return Err(format!("injected move failure: {stage}"));
    }
    Ok(())
}

fn operation() -> CrdtOperation {
    CrdtOperation {
        operation_id: "move-test".into(),
        kind: KIND.into(),
        graph_id: "source".into(),
        document_id: None,
        payload: json!({"targetGraphId":"target","graphIncarnation":"source-incarnation",
            "targetGraphIncarnation":"target-incarnation","documentIds":["doc"],"order":1}),
        enqueue_timestamp: "1720000000000".into(),
    }
}
#[test]
fn request_rejects_duplicate_same_graph_bad_parent_and_large_batch() {
    let original = operation();
    assert!(Request::parse(&original).is_ok());
    for (key, value) in [
        ("documentIds", json!(["doc", "doc"])),
        ("targetGraphId", json!("source")),
        ("parentId", json!(17)),
        (
            "documentIds",
            json!((0..101).map(|n| format!("doc-{n}")).collect::<Vec<_>>()),
        ),
    ] {
        let mut op = original.clone();
        op.payload[key] = value;
        assert!(Request::parse(&op).is_err(), "{key}");
    }
}
#[test]
fn history_wire_stripping_preserves_text_and_other_marks() {
    let xml = "<paragraph data-block-id=\"b\"><strong>before <wireMark wireId=\"w\"><commentMark commentId=\"c\">text &amp; more</commentMark></wireMark></strong></paragraph>";
    let cleaned = strip_wire_xml(xml).unwrap();
    assert!(!cleaned.contains("wireMark"));
    assert!(cleaned.contains("<commentMark commentId=\"c\">text &amp; more</commentMark>"));
    assert!(cleaned.contains("data-block-id=\"b\""));
    let mut blocks = json!({"blocks":[{"id":"b","marks":[{"type":"wire","wireId":"w"},{"type":"bold"}]}],"tiptapXml":xml});
    strip_wires(&mut blocks).unwrap();
    assert_eq!(blocks["blocks"][0]["marks"], json!([{"type":"bold"}]));
}
#[test]
fn prepared_state_preserves_comments_block_ids_and_removes_wire_marks() {
    let doc = Doc::new();
    {
        let mut txn = doc.transact_mut();
        let content = txn.get_or_insert_xml_fragment("content");
        super::super::builder::append_nodes(
            &mut txn,
            &content,
            &[json!({"type":"paragraph","attrs":{"blockId":"b"},
            "content":[{"type":"text","text":"Hello", "marks":[{"type":"bold"},{"type":"wireMark","attrs":{"wireId":"w"}},{"type":"commentMark","attrs":{"commentId":"c"}}]}]})],
        );
        txn.get_or_insert_map("comments").insert(
            &mut txn,
            "c",
            super::super::builder::json_to_any(&json!({"text":"keep"})),
        );
    }
    let before = encode(&doc);
    let after = prepared_state(&before, "op", "doc").unwrap();
    assert_eq!(
        document_ops::comments_from_ydoc_base64(&before).unwrap(),
        document_ops::comments_from_ydoc_base64(&after).unwrap()
    );
    let restored = Doc::new();
    restored
        .transact_mut()
        .apply_update(decode(&after).unwrap())
        .unwrap();
    assert_eq!(
        encode(&restored),
        after,
        "full-state encoding stable after hydration"
    );
    let projected = super::super::projection::materialize_ydoc(&restored, "doc");
    assert_eq!(projected.blocks_json[0]["id"], "b");
    assert!(!projected.tiptap_json.to_string().contains("wireMark"));
    assert!(projected.tiptap_json.to_string().contains("commentMark"));
}
#[test]
fn oversized_file_is_refused_without_reading_or_modifying_it() {
    let path = std::env::temp_dir().join(format!("move-cap-{}", uuid::Uuid::new_v4()));
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(MAX_FILE_BYTES + 1).unwrap();
    assert!(bounded_file(&path).unwrap_err().contains("32 MiB"));
    assert_eq!(file.metadata().unwrap().len(), MAX_FILE_BYTES + 1);
    std::fs::remove_file(path).unwrap();
}
#[test]
fn local_assets_fail_closed() {
    assert!(reject_assets(&json!({"src":"garden://images/image-a"})).is_err());
    assert!(reject_assets(&json!({"sourceFile":{"storageKey":"x"}})).is_err());
    assert!(reject_assets(&json!({"src":"https://example.org/a.png"})).is_ok());
}

#[test]
fn document_links_are_not_asset_dependencies() {
    for href in [
        "infra-audit-validation-report",
        "./y-doc-audit",
        "https://example.org/api/reference",
        "http://localhost:3000/docs",
    ] {
        assert!(reject_assets(&json!({"type":"link","attrs":{"href":href}})).is_ok());
        let xml = format!("<paragraph><link href=\"{href}\">reference</link></paragraph>");
        assert_eq!(strip_wire_xml(&xml).unwrap(), xml);
    }
    assert!(reject_assets(&json!({"type":"wikilink","attrs":{"targetDocId":"old-doc"}})).is_ok());
    let xml = "<paragraph><wikilink targetDocId=\"old-doc\">Old doc</wikilink></paragraph>";
    assert_eq!(strip_wire_xml(xml).unwrap(), xml);
    assert!(reject_assets(&json!({"type":"image","attrs":{"src":"./image.png"}})).is_err());
}

#[cfg(all(feature = "headless", not(feature = "desktop")))]
mod integration {
    use super::*;
    use crate::graph_service::{create_graph_service, CreateGraphInput};
    fn graph(app: &AppHandle, id: &str) -> String {
        create_graph_service(
            app,
            CreateGraphInput {
                title: id.into(),
                graph_id: Some(id.into()),
                description: None,
                operation_id: None,
            },
        )
        .unwrap()
        .incarnation_id
        .unwrap()
    }
    async fn write(app: &AppHandle, graph: &str, id: &str) {
        document_ops::document_write_classified(app,&CrdtOperation {
            operation_id:format!("write-{graph}-{id}"),kind:"document.write".into(),graph_id:graph.into(),document_id:Some(id.into()),
            payload:json!({"title":"Preserved title","tiptapJson":{"type":"doc","content":[{"type":"paragraph","attrs":{"blockId":"b"},"content":[{"type":"text","text":"Keep me"}]}]},
                "comments":{"c":{"text":"A comment","author":"test","createdAt":1}},
                "chronicleRestore":{"createdAt":"1600000000000","updatedAt":"1700000000000","source":"test"}}),
            enqueue_timestamp:"1720000000000".into()
        }).await.unwrap();
    }
    fn fixture(test: impl AsyncFn(&AppHandle, CrdtOperation)) {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let path = std::env::temp_dir().join(format!("cross-graph-move-{}", uuid::Uuid::new_v4()));
        std::env::set_var("GARDEN_PROFILE_DIR", &path);
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    tokio::time::pause();
                    let app = crate::tauri_runtime::build_mock_app_for_tests(true);
                    let mut op = operation();
                    op.payload["graphIncarnation"] = json!(graph(&app, "source"));
                    op.payload["targetGraphIncarnation"] = json!(graph(&app, "target"));
                    write(&app, "source", "doc").await;
                    test(&app, op).await;
                });
        }));
        std::env::remove_var("GARDEN_PROFILE_DIR");
        let _ = std::fs::remove_dir_all(path);
        if let Err(error) = outcome {
            std::panic::resume_unwind(error);
        }
    }
    #[test]
    fn moves_content_comments_creation_and_history_and_replays_complete() {
        fixture(async |app, op| {
            let source = crate::graph_paths::existing_graph_dir(app, "source").unwrap();
            let target = crate::graph_paths::existing_graph_dir(app, "target").unwrap();
            let source_incarnation =
                crate::document_incarnation_store::ensure_document_incarnation_id(&source, "doc")
                    .unwrap();
            let history = capture_history(&source, "doc").unwrap();
            assert!(!history.is_empty());
            let answer = apply(app, &op).await.unwrap();
            assert_eq!(answer["movedCount"], 1);
            assert_eq!(answer["moved"], json!(["doc"]));
            assert_eq!(answer["missing"], json!([]));
            assert_eq!(answer["parentId"], Value::Null);
            let target_incarnation =
                crate::document_incarnation_store::ensure_document_incarnation_id(&target, "doc")
                    .unwrap();
            assert_ne!(source_incarnation, target_incarnation);
            assert!(
                crate::document_tombstone_store::document_is_tombstoned(&source, "doc").unwrap()
            );
            let record = crate::document_record_store::read_document_record_cold(
                &target,
                &target.join("documents/doc/document.json"),
            )
            .unwrap();
            assert_eq!(record.created_at, "1600000000000");
            assert_eq!(record.title, "Preserved title");
            assert!(record.body.contains("Keep me"));
            assert_eq!(record.blocks[0].id, "b");
            let state = room(app, "target", &target, "doc")
                .await
                .unwrap()
                .with_doc(encode)
                .await;
            assert_eq!(
                document_ops::comments_from_ydoc_base64(&state).unwrap()["c"]["text"],
                "A comment"
            );
            for (relative, value) in history {
                if relative == "history/history.json" {
                    continue;
                }
                let actual: Value =
                    crate::storage::read_json(&target.join("documents/doc").join(relative))
                        .unwrap();
                assert_eq!(actual, target_history(&value, "target").unwrap());
            }
            assert_eq!(apply(app, &op).await.unwrap(), answer);
            let receipt: Receipt = crate::storage::read_json(
                &crate::profile_paths::profile_dir(app)
                    .unwrap()
                    .join("cross-graph-moves/move-test.json"),
            )
            .unwrap();
            assert!(receipt.complete && receipt.entries.is_empty());
        });
    }
    #[test]
    fn batch_collision_preflight_does_not_publish_first_document() {
        fixture(async |app, mut op| {
            write(app, "source", "other").await;
            write(app, "target", "other").await;
            op.payload["documentIds"] = json!(["doc", "other"]);
            assert!(apply(app, &op)
                .await
                .unwrap_err()
                .to_string()
                .contains("collision"));
            let target = crate::graph_paths::existing_graph_dir(app, "target").unwrap();
            assert!(!target.join("documents/doc").exists());
            let source = crate::graph_paths::existing_graph_dir(app, "source").unwrap();
            assert!(source.join("documents/doc/document.json").exists());
            assert!(!crate::profile_paths::profile_dir(app)
                .unwrap()
                .join("cross-graph-moves/move-test.json")
                .exists());
        });
    }
    #[test]
    fn resumes_after_source_tombstone_tail_failure() {
        fixture(async |app, op| {
            crate::document_delete_service::fail_next_document_delete_after_tombstone_for_test(
                "source", "doc",
            );
            assert!(apply(app, &op)
                .await
                .unwrap_err()
                .to_string()
                .contains("injected document directory cleanup"));
            assert_eq!(apply(app, &op).await.unwrap()["movedCount"], 1);
        });
    }
    #[test]
    fn recovery_never_overwrites_target_edits() {
        fixture(async |app, op| {
            crate::document_delete_service::fail_next_document_delete_after_tombstone_for_test(
                "source", "doc",
            );
            assert!(apply(app, &op)
                .await
                .unwrap_err()
                .to_string()
                .contains("injected document directory cleanup"));
            let target = crate::graph_paths::existing_graph_dir(app, "target").unwrap();
            let room = room(app, "target", &target, "doc").await.unwrap();
            room.update_doc(|_, txn| {
                txn.get_or_insert_map("comments")
                    .insert(txn, "later", "keep later edit");
                Ok(())
            })
            .await
            .unwrap();
            let changed = room.with_doc(encode).await;
            assert!(apply(app, &op)
                .await
                .unwrap_err()
                .to_string()
                .contains("target changed"));
            assert_eq!(room.with_doc(encode).await, changed);
        });
    }
    #[test]
    fn retries_prepared_published_delete_intent_and_deleted_boundaries() {
        for stage in ["prepared", "published", "deleting", "deleted"] {
            fixture(async |app, op| {
                *FAILURE.lock().unwrap() = Some(stage);
                assert!(apply(app, &op)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains(stage));
                assert_eq!(apply(app, &op).await.unwrap()["movedCount"], 1, "{stage}");
            });
        }
    }
    #[test]
    fn changed_source_after_preparation_never_deletes_or_publishes() {
        fixture(async |app, op| {
            *FAILURE.lock().unwrap() = Some("prepared");
            assert!(apply(app, &op).await.is_err());
            let source = crate::graph_paths::existing_graph_dir(app, "source").unwrap();
            let room = room(app, "source", &source, "doc").await.unwrap();
            room.update_doc(|_, txn| {
                txn.get_or_insert_map("comments")
                    .insert(txn, "later", "source edit");
                Ok(())
            })
            .await
            .unwrap();
            let changed = room.with_doc(encode).await;
            assert!(apply(app, &op)
                .await
                .unwrap_err()
                .to_string()
                .contains("source changed"));
            assert_eq!(room.with_doc(encode).await, changed);
            assert!(!crate::graph_paths::existing_graph_dir(app, "target")
                .unwrap()
                .join("documents/doc")
                .exists());
        });
    }
    #[test]
    fn stale_target_incarnation_and_unsafe_operation_id_fail_before_receipt() {
        fixture(async |app, mut op| {
            op.payload["targetGraphIncarnation"] = json!("wrong-incarnation");
            assert!(apply(app, &op)
                .await
                .unwrap_err()
                .to_string()
                .contains("stale target"));
            op.operation_id = "../escape".into();
            assert!(apply(app, &op).await.is_err());
            assert!(!crate::profile_paths::profile_dir(app)
                .unwrap()
                .join("cross-graph-moves")
                .exists());
        });
    }
    #[test]
    fn both_graph_hot_write_and_lifecycle_gates_block_before_receipt() {
        fixture(async |app, op| {
            let coordinator = app.state::<super::super::super::persistence_coordinator::GraphPersistenceCoordinator>();
            for graph in ["source", "target"] {
                let held = coordinator.acquire_hot_write(graph).await.unwrap();
                assert!(tokio::time::timeout(
                    std::time::Duration::from_millis(10),
                    apply(app, &op)
                )
                .await
                .is_err());
                drop(held);
                let held = coordinator
                    .acquire_lifecycle_exclusive(graph)
                    .await
                    .unwrap();
                assert!(tokio::time::timeout(
                    std::time::Duration::from_millis(10),
                    apply(app, &op)
                )
                .await
                .is_err());
                drop(held);
            }
            assert!(!crate::profile_paths::profile_dir(app)
                .unwrap()
                .join("cross-graph-moves")
                .exists());
            assert_eq!(apply(app, &op).await.unwrap()["movedCount"], 1);
        });
    }
    #[test]
    fn resumed_transfer_keeps_new_target_attention() {
        fixture(async |app, op| {
            *FAILURE.lock().unwrap() = Some("published");
            assert!(apply(app, &op)
                .await
                .unwrap_err()
                .to_string()
                .contains("published"));
            crate::salience_mcp_valuation::mcp_local_value(
                app.clone(),
                &json!({
                    "graphId":"target","documentId":"doc","blockId":"b","importance":5
                }),
            )
            .unwrap();
            let target = crate::graph_paths::existing_graph_dir(app, "target").unwrap();
            let before = crate::salience_value_store::read_value_store(&target, "target").unwrap();
            assert!(!before.blocks.is_empty());
            assert_eq!(apply(app, &op).await.unwrap()["movedCount"], 1);
            let after = crate::salience_value_store::read_value_store(&target, "target").unwrap();
            assert_eq!(
                serde_json::to_value(before).unwrap(),
                serde_json::to_value(after).unwrap()
            );
        });
    }

    #[test]
    fn opposite_direction_moves_complete_without_lock_order_deadlock() {
        fixture(async |app, forward| {
            // The fixture pauses Tokio time for background schedulers. This
            // test needs a wall-clock deadline: virtual time can jump to the
            // timeout while both moves await filesystem spawn_blocking work.
            tokio::time::resume();
            write(app, "target", "other").await;
            let mut reverse = forward.clone();
            reverse.operation_id = "move-reverse".into();
            reverse.graph_id = "target".into();
            reverse.payload["targetGraphId"] = json!("source");
            reverse.payload["documentIds"] = json!(["other"]);
            reverse.payload["graphIncarnation"] = forward.payload["targetGraphIncarnation"].clone();
            reverse.payload["targetGraphIncarnation"] = forward.payload["graphIncarnation"].clone();

            let started = std::time::Instant::now();
            let (forward_result, reverse_result) =
                tokio::time::timeout(std::time::Duration::from_secs(30), async {
                    tokio::join!(apply(app, &forward), apply(app, &reverse))
                })
                .await
                .expect("opposite-direction moves must not deadlock");
            eprintln!(
                "opposite-direction moves completed in {:?} wall time",
                started.elapsed()
            );
            assert_eq!(forward_result.unwrap()["moved"], json!(["doc"]));
            assert_eq!(reverse_result.unwrap()["moved"], json!(["other"]));
            let source = crate::graph_paths::existing_graph_dir(app, "source").unwrap();
            let target = crate::graph_paths::existing_graph_dir(app, "target").unwrap();
            assert!(source.join("documents/other/document.json").is_file());
            assert!(target.join("documents/doc/document.json").is_file());
            assert!(
                crate::document_tombstone_store::document_is_tombstoned(&source, "doc").unwrap()
            );
            assert!(
                crate::document_tombstone_store::document_is_tombstoned(&target, "other").unwrap()
            );
        });
    }
}
