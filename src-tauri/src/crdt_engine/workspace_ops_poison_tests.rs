//! A queued workspace operation that can never succeed answers its caller at
//! once and leaves the graph's queue free.
//!
//! A refusal made after an operation's hot commit is kept for tail retry: every
//! 25 ms to 1 s while the caller waits, then on a backoff up to 60 s, until the
//! retry ceiling. A retry is also the graph's FIFO barrier, so every later write
//! to that graph waits behind it.
//!
//! These tests drive the real MCP handlers and the real queue against a
//! disposable profile. Each call is bounded far below the queue's caller
//! timeout, so a parked operation shows up as a failed test, not a hung one.
//!
//! Adapted from sophia-labs/garden's workspace_ops_poison_tests.
use serde_json::{json, Value};
use std::time::Duration;

/// Plenty for an operation that answers; a parked one never does.
const ANSWER_WITHIN: Duration = Duration::from_secs(10);

fn with_graph<F, Fut>(graph_id: &'static str, case: F)
where
    F: FnOnce(crate::app_runtime::AppHandle) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let _serial = crate::tauri_runtime::profile_env_serial()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let previous = std::env::var_os("GARDEN_PROFILE_DIR");
    let profile = std::env::temp_dir().join(format!("garden-{graph_id}-{}", uuid::Uuid::new_v4()));
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        crate::app_runtime::async_runtime::block_on(async {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            crate::graph_service::create_graph_service(
                &app,
                crate::graph_service::CreateGraphInput {
                    graph_id: Some(graph_id.into()),
                    title: "Poison specimen".into(),
                    description: None,
                    operation_id: None,
                },
            )
            .unwrap();
            case(app).await;
        })
    }));
    match previous {
        Some(value) => std::env::set_var("GARDEN_PROFILE_DIR", value),
        None => std::env::remove_var("GARDEN_PROFILE_DIR"),
    }
    let _ = std::fs::remove_dir_all(&profile);
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

async fn create(
    app: &crate::app_runtime::AppHandle,
    graph_id: &str,
    mut arguments: Value,
) -> Result<Value, String> {
    arguments["graphId"] = json!(graph_id);
    tokio::time::timeout(
        ANSWER_WITHIN,
        crate::mcp_workspace_crdt_service::mcp_local_create_document(app.clone(), &arguments),
    )
    .await
    .unwrap_or_else(|_| {
        panic!(
            "create_document {arguments} did not answer within {ANSWER_WITHIN:?}: parked for retry"
        )
    })
}

async fn enqueue(
    app: &crate::app_runtime::AppHandle,
    kind: &str,
    graph_id: &str,
    document_id: Option<&str>,
    payload: Value,
) -> Result<Value, String> {
    let described = format!("{kind} {payload}");
    tokio::time::timeout(
        ANSWER_WITHIN,
        crate::crdt_queue::enqueue_crdt_operation(
            app.clone(),
            crate::crdt_operation_types::EnqueueCrdtOperationInput {
                kind: kind.to_string(),
                graph_id: graph_id.to_string(),
                document_id: document_id.map(str::to_string),
                payload,
            },
        ),
    )
    .await
    .unwrap_or_else(|_| {
        panic!("{described} did not answer within {ANSWER_WITHIN:?}: parked for retry")
    })
}

async fn workspace_rows(app: &crate::app_runtime::AppHandle, graph_id: &str) -> Vec<Value> {
    let workspace = crate::workspace_projection_service::mcp_local_get_workspace(
        app.clone(),
        &json!({ "graphId": graph_id, "depth": 5, "limit": 200 }),
    )
    .await
    .unwrap();
    workspace["documents"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn row_id(row: &Value) -> &str {
    row["documentId"]
        .as_str()
        .or_else(|| row["id"].as_str())
        .unwrap_or_default()
}

/// Nothing pending in the journal, nothing in flight, no retry barrier.
fn assert_queue_free(app: &crate::app_runtime::AppHandle) {
    let pending = crate::crdt_operation_journal::recover_pending_crdt_operations(app)
        .expect("read the operation journal");
    assert!(
        pending.is_empty(),
        "operations left pending in the journal: {:?}",
        pending
            .iter()
            .map(|op| (&op.operation_id, &op.kind))
            .collect::<Vec<_>>()
    );
    let queue = app.state::<crate::crdt_queue::CrdtOperationQueue>();
    assert_eq!(
        queue.counts_for_test().unwrap(),
        (0, 0),
        "pending, in flight"
    );
    assert_eq!(
        queue.retry_wait_count_for_test().unwrap(),
        0,
        "retry barriers"
    );
}

/// A row in the workspace Y.Doc, applied the way the WebSocket host applies a
/// client's update. The workspace map takes any key; only queued operations
/// check ids.
async fn client_writes_workspace_row(
    app: &crate::app_runtime::AppHandle,
    graph_id: &str,
    row_id: &str,
    parent_id: Option<&str>,
) {
    use yrs::{Any, Doc, Map, MapPrelim, ReadTxn, StateVector, Transact, WriteTxn};
    let client = Doc::new();
    {
        let mut txn = client.transact_mut();
        let documents = txn.get_or_insert_map("documents");
        let entry = documents.insert(&mut txn, row_id, MapPrelim::default());
        entry.insert(&mut txn, "title", "Stray row");
        entry.insert(
            &mut txn,
            "parentId",
            parent_id
                .map(|parent| Any::String(parent.into()))
                .unwrap_or(Any::Null),
        );
        entry.insert(&mut txn, "section", "documents");
        entry.insert(&mut txn, "order", 7.0);
        entry.insert(&mut txn, "createdAt", 1.0);
        entry.insert(&mut txn, "updatedAt", 1.0);
        entry.insert(&mut txn, "readOnly", false);
    }
    let update = client
        .transact()
        .encode_state_as_update_v1(&StateVector::default());
    let graph_dir = crate::graph_paths::existing_graph_dir(app, graph_id).unwrap();
    let room = super::workspace_ops::workspace_room(app, graph_id, &graph_dir)
        .await
        .unwrap();
    assert!(
        room.apply_client_update(&update).await.unwrap(),
        "the row was applied"
    );
}

#[test]
fn a_workspace_row_whose_id_is_not_a_local_id_blocks_no_other_write() {
    const GRAPH: &str = "poison-stray-row";
    with_graph(GRAPH, |app| async move {
        create(
            &app,
            GRAPH,
            json!({ "documentId": "doc-before", "title": "Before" }),
        )
        .await
        .expect("created");
        client_writes_workspace_row(&app, GRAPH, "stray row", None).await;

        create(
            &app,
            GRAPH,
            json!({ "documentId": "doc-after", "title": "After" }),
        )
        .await
        .expect("a create beside the stray row lands");
        enqueue(
            &app,
            "workspace.updateDocument",
            GRAPH,
            Some("doc-before"),
            json!({ "documentId": "doc-before", "title": "Before, renamed" }),
        )
        .await
        .expect("a rename beside the stray row lands");
        assert_queue_free(&app);
        let rows = workspace_rows(&app, GRAPH).await;
        assert!(
            rows.iter().any(|row| row_id(row) == "doc-after"),
            "{rows:?}"
        );
        let renamed = rows
            .iter()
            .find(|row| row_id(row) == "doc-before")
            .expect("listed");
        assert_eq!(renamed["title"], "Before, renamed", "{renamed}");
    });
}

// ─────────────────────────────────────────────────────────────────────────────
// Refusals made before the hot commit (create, delete, write) and stray rows
// that delete on their own.
// ─────────────────────────────────────────────────────────────────────────────

async fn delete(
    app: &crate::app_runtime::AppHandle,
    graph_id: &str,
    document_id: &str,
) -> Result<Value, String> {
    let arguments = json!({ "graphId": graph_id, "documentId": document_id });
    tokio::time::timeout(
        ANSWER_WITHIN,
        crate::mcp_workspace_crdt_service::mcp_local_delete_document(app.clone(), &arguments),
    )
    .await
    .unwrap_or_else(|_| {
        panic!(
            "delete_document {arguments} did not answer within {ANSWER_WITHIN:?}: parked for retry"
        )
    })
}

#[test]
fn a_title_over_the_cap_is_refused_at_once_and_leaves_the_row_alone() {
    const GRAPH: &str = "poison-title";
    let too_long = "x".repeat(crate::ids::MAX_TITLE_CHARS + 1);
    with_graph(GRAPH, |app| async move {
        let refused = create(&app, GRAPH, json!({ "title": too_long }))
            .await
            .expect_err("a title over the cap is refused");
        assert!(refused.contains("characters or fewer"), "{refused}");
        assert_queue_free(&app);

        create(
            &app,
            GRAPH,
            json!({ "documentId": "doc-kept", "title": "Kept" }),
        )
        .await
        .expect("a short title is created");
        let refused = create(
            &app,
            GRAPH,
            json!({ "documentId": "doc-kept", "title": too_long }),
        )
        .await
        .expect_err("an upsert with a long title is refused");
        assert!(refused.contains("characters or fewer"), "{refused}");
        assert_queue_free(&app);

        let at_cap = "y".repeat(crate::ids::MAX_TITLE_CHARS);
        create(
            &app,
            GRAPH,
            json!({ "documentId": "doc-cap", "title": at_cap }),
        )
        .await
        .expect("the cap itself is allowed");
        assert_queue_free(&app);

        let rows = workspace_rows(&app, GRAPH).await;
        let kept = rows
            .iter()
            .find(|row| row_id(row) == "doc-kept")
            .expect("doc-kept listed");
        assert_eq!(
            kept["title"], "Kept",
            "the refused upsert renamed it: {kept}"
        );
        assert!(rows.iter().any(|row| row_id(row) == "doc-cap"), "{rows:?}");
    });
}

#[test]
fn a_create_whose_document_id_is_not_a_local_id_is_refused_at_once() {
    const GRAPH: &str = "poison-create-id";
    with_graph(GRAPH, |app| async move {
        let refused = create(
            &app,
            GRAPH,
            json!({ "documentId": "not a local id", "title": "Fine" }),
        )
        .await
        .expect_err("an id with spaces is refused");
        assert!(refused.contains("document_id"), "{refused}");
        assert_queue_free(&app);
        let rows = workspace_rows(&app, GRAPH).await;
        assert!(
            !rows.iter().any(|row| row_id(row) == "not a local id"),
            "{rows:?}"
        );
    });
}

#[test]
fn creating_over_a_deleted_document_is_refused_at_once() {
    const GRAPH: &str = "poison-tombstone";
    with_graph(GRAPH, |app| async move {
        create(
            &app,
            GRAPH,
            json!({ "documentId": "doc-gone", "title": "Gone" }),
        )
        .await
        .expect("created");
        delete(&app, GRAPH, "doc-gone").await.expect("deleted");
        let refused = create(
            &app,
            GRAPH,
            json!({ "documentId": "doc-gone", "title": "Back" }),
        )
        .await
        .expect_err("a deleted id is recreated through write_document, not create_document");
        assert!(
            refused.contains("document tombstoned: doc-gone"),
            "{refused}"
        );
        assert_queue_free(&app);
        let rows = workspace_rows(&app, GRAPH).await;
        assert!(
            !rows.iter().any(|row| row_id(row) == "doc-gone"),
            "{rows:?}"
        );
    });
}

#[test]
fn creating_over_a_derived_view_is_refused_and_does_not_refile_it() {
    const GRAPH: &str = "poison-derived-view";
    with_graph(GRAPH, |app| async move {
        crate::mcp_workspace_crdt_service::mcp_local_create_folder(
            app.clone(),
            &json!({ "graphId": GRAPH, "folderId": "folder-elsewhere", "name": "Elsewhere" }),
        )
        .await
        .expect("folder created");
        create(
            &app,
            GRAPH,
            json!({ "documentId": "doc-view", "title": "Song" }),
        )
        .await
        .expect("created");
        let graph_dir = crate::graph_paths::existing_graph_dir(&app, GRAPH).unwrap();
        crate::geist_derived_view::register(&graph_dir, "doc-view").unwrap();

        let refused = create(
            &app,
            GRAPH,
            json!({ "documentId": "doc-view", "title": "Hijack", "parentId": "folder-elsewhere" }),
        )
        .await
        .expect_err("a derived view is not created over");
        assert!(
            refused.contains("Derived Geist views are read-only"),
            "{refused}"
        );
        assert_queue_free(&app);
        let rows = workspace_rows(&app, GRAPH).await;
        let view = rows
            .iter()
            .find(|row| row_id(row) == "doc-view")
            .expect("the view is still listed");
        assert_eq!(view["title"], "Song", "{view}");
        assert!(view["parentId"].is_null(), "the view was re-filed: {view}");
    });
}

#[test]
fn deleting_an_id_that_is_not_a_local_id_and_has_no_row_is_refused_at_once() {
    const GRAPH: &str = "poison-delete-id";
    with_graph(GRAPH, |app| async move {
        let refused = delete(&app, GRAPH, "not a local id")
            .await
            .expect_err("an id with spaces and no row is refused");
        assert!(
            refused.contains("document_id contains invalid characters"),
            "{refused}"
        );
        assert_queue_free(&app);
        create(
            &app,
            GRAPH,
            json!({ "documentId": "doc-after", "title": "After" }),
        )
        .await
        .expect("the graph takes the next write");
    });
}

#[test]
fn deleting_a_stray_row_removes_just_the_row() {
    const GRAPH: &str = "poison-delete-stray";
    with_graph(GRAPH, |app| async move {
        client_writes_workspace_row(&app, GRAPH, "stray row", None).await;
        let deleted = delete(&app, GRAPH, "stray row")
            .await
            .expect("the stray row is removed");
        assert_eq!(
            deleted["removedStrayRowIds"],
            json!(["stray row"]),
            "{deleted}"
        );
        assert_queue_free(&app);
        let rows = workspace_rows(&app, GRAPH).await;
        assert!(
            !rows.iter().any(|row| row_id(row) == "stray row"),
            "{rows:?}"
        );
    });
}

#[test]
fn a_hard_folder_delete_over_a_stray_row_removes_it_with_the_folder() {
    const GRAPH: &str = "poison-folder-cascade";
    with_graph(GRAPH, |app| async move {
        crate::mcp_workspace_crdt_service::mcp_local_create_folder(
            app.clone(),
            &json!({ "graphId": GRAPH, "folderId": "folder-held", "name": "Held" }),
        )
        .await
        .expect("folder created");
        create(
            &app,
            GRAPH,
            json!({ "documentId": "doc-sibling", "title": "Sibling", "parentId": "folder-held" }),
        )
        .await
        .expect("a document in the folder");
        client_writes_workspace_row(&app, GRAPH, "stray row", Some("folder-held")).await;

        let deleted = enqueue(
            &app,
            "workspace.deleteFolder",
            GRAPH,
            None,
            json!({ "folderId": "folder-held", "cascade": true, "hard": true }),
        )
        .await
        .expect("the cascade lands");
        assert_eq!(
            deleted["deletedDocumentIds"],
            json!(["doc-sibling"]),
            "{deleted}"
        );
        assert_eq!(
            deleted["removedStrayRowIds"],
            json!(["stray row"]),
            "{deleted}"
        );
        assert_queue_free(&app);

        let graph_dir = crate::graph_paths::existing_graph_dir(&app, GRAPH).unwrap();
        assert!(
            crate::document_tombstone_store::document_is_tombstoned(&graph_dir, "doc-sibling")
                .unwrap()
        );
        let rows = workspace_rows(&app, GRAPH).await;
        assert!(
            !rows
                .iter()
                .any(|row| row_id(row) == "stray row" || row_id(row) == "doc-sibling"),
            "{rows:?}"
        );
    });
}

#[test]
fn a_write_whose_document_id_is_not_a_local_id_is_refused_at_once() {
    const GRAPH: &str = "poison-write-id";
    with_graph(GRAPH, |app| async move {
        let refused = enqueue(
            &app,
            "document.write",
            GRAPH,
            None,
            json!({ "documentId": "not a local id", "content": "# Written\n\nBody.\n", "format": "markdown" }),
        )
        .await
        .expect_err("an id with spaces is refused");
        assert!(refused.contains("document_id"), "{refused}");
        assert_queue_free(&app);
        create(
            &app,
            GRAPH,
            json!({ "documentId": "doc-after", "title": "After" }),
        )
        .await
        .expect("the graph takes the next write");
    });
}

#[test]
fn a_create_whose_body_cannot_be_written_for_a_while_is_retried_until_it_lands() {
    const GRAPH: &str = "poison-transient";
    with_graph(GRAPH, |app| async move {
        // A file where the document's folder goes: the workspace entry
        // commits, the body cannot be made, and that can heal.
        let graph_dir = crate::graph_paths::existing_graph_dir(&app, GRAPH).unwrap();
        let blocker = crate::graph_paths::documents_dir(&graph_dir).join("doc-tail");
        std::fs::create_dir_all(blocker.parent().unwrap()).unwrap();
        std::fs::write(&blocker, b"not a folder").unwrap();

        let caller = {
            let app = app.clone();
            crate::app_runtime::async_runtime::spawn(async move {
                crate::mcp_workspace_crdt_service::mcp_local_create_document(
                    app,
                    &json!({ "graphId": GRAPH, "documentId": "doc-tail", "title": "Tail" }),
                )
                .await
            })
        };
        let queue = app.state::<crate::crdt_queue::CrdtOperationQueue>();
        let parked = async {
            while queue.retry_wait_count_for_test().unwrap() == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        tokio::time::timeout(ANSWER_WITHIN, parked)
            .await
            .expect("the failed tail is kept for retry");
        assert!(
            !caller.is_finished(),
            "the caller still waits for its document"
        );

        std::fs::remove_file(&blocker).unwrap();
        let created = tokio::time::timeout(ANSWER_WITHIN, caller)
            .await
            .expect("the retry lands once the folder can be made")
            .expect("caller task")
            .expect("created");
        assert_eq!(created["documentId"], "doc-tail", "{created}");
        assert_queue_free(&app);
    });
}
