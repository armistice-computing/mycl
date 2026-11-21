//! The Chronicle, end to end through the real CRDT engine: authorship stamps
//! from the request principal, the staleness guard on document and block
//! writes, and the recorded `force` override. Design of record: graph doc
//! `settlement-chronicle-design-2026-08-26`, Parts 2 and 3.

use crate::{
    app_runtime::AppHandle,
    crdt_queue::{enqueue_crdt_operation, EnqueueCrdtOperationInput},
    document_chronicle::{with_actor, ChronicleActor},
    document_types::DocumentRecord,
    graph_service::{create_graph_service, CreateGraphInput},
};
use serde_json::{json, Value};
use std::{panic::UnwindSafe, path::PathBuf};
use uuid::Uuid;

fn with_profile(prefix: &str, test: impl FnOnce() + UnwindSafe) {
    let _serial = crate::tauri_runtime::profile_env_serial()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let profile: PathBuf = std::env::temp_dir().join(format!("{prefix}-{}", Uuid::new_v4()));
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let result = std::panic::catch_unwind(test);
    std::env::remove_var("GARDEN_PROFILE_DIR");
    let _ = std::fs::remove_dir_all(&profile);
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

fn seed_graph(app: &AppHandle, graph_id: &str) {
    create_graph_service(
        app,
        CreateGraphInput {
            graph_id: Some(graph_id.to_string()),
            title: "Chronicle".to_string(),
            description: None,
            operation_id: None,
        },
    )
    .expect("create graph");
}

async fn enqueue(
    app: &AppHandle,
    kind: &str,
    graph_id: &str,
    document_id: &str,
    payload: Value,
) -> Result<Value, String> {
    enqueue_crdt_operation(
        app.clone(),
        EnqueueCrdtOperationInput {
            kind: kind.to_string(),
            graph_id: graph_id.to_string(),
            document_id: Some(document_id.to_string()),
            payload,
        },
    )
    .await
}

fn write(app: &AppHandle, graph_id: &str, document_id: &str, markdown: &str, extra: Value) -> Result<Value, String> {
    let mut payload = json!({
        "documentId": document_id,
        "content": markdown,
        "format": "markdown",
        "title": "Chronicle Doc",
    });
    if let (Some(target), Some(fields)) = (payload.as_object_mut(), extra.as_object()) {
        for (key, value) in fields {
            target.insert(key.clone(), value.clone());
        }
    }
    crate::app_runtime::async_runtime::block_on(enqueue(
        app,
        "document.write",
        graph_id,
        document_id,
        payload,
    ))
}

fn edit_text(
    app: &AppHandle,
    graph_id: &str,
    document_id: &str,
    block_id: &str,
    text: &str,
    extra: Value,
) -> Result<Value, String> {
    let mut payload = json!({
        "documentId": document_id,
        "blockId": block_id,
        "operations": [{ "type": "insert", "offset": 0, "text": text }],
    });
    if let (Some(target), Some(fields)) = (payload.as_object_mut(), extra.as_object()) {
        for (key, value) in fields {
            target.insert(key.clone(), value.clone());
        }
    }
    crate::app_runtime::async_runtime::block_on(enqueue(
        app,
        "block.editText",
        graph_id,
        document_id,
        payload,
    ))
}

fn record(app: &AppHandle, graph_id: &str, document_id: &str) -> DocumentRecord {
    let graph_dir = crate::graph_paths::existing_graph_dir(app, graph_id).expect("graph dir");
    crate::document_record_store::read_graph_documents_cold(&graph_dir)
        .expect("read records")
        .into_iter()
        .find(|record| record.document_id == document_id)
        .expect("document record")
}

fn label(actor: &Option<ChronicleActor>) -> Option<String> {
    actor.as_ref().map(ChronicleActor::label)
}

/// Every mutation ack names the blocks it touched by their STORED text, so
/// an agent can tell which id is which sentence without a second read. The
/// preview is read inside the mutating transaction, after content conversion.
#[test]
fn mutation_acks_carry_block_previews() {
    with_profile("garden-preview-test", || {
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);
        seed_graph(&app, "g");
        let written = write(
            &app,
            "g",
            "doc-preview",
            "# Marker probe\n\nThis line carries a marker.\n\nShort.",
            json!({}),
        )
        .expect("write");
        let ids = written["blockIds"]
            .as_array()
            .or_else(|| written["block_ids"].as_array())
            .cloned()
            .unwrap_or_default();
        assert!(ids.len() >= 2, "write ack lists block ids: {written}");

        let inserted = crate::app_runtime::async_runtime::block_on(enqueue(
            &app,
            "block.insert",
            "g",
            "doc-preview",
            json!({
                "documentId": "doc-preview",
                "content": "Inserted   paragraph with\nextra   space",
                "format": "markdown",
            }),
        ))
        .expect("insert");
        let previews = inserted["blocks"].as_array().expect("insert ack carries previews");
        assert_eq!(previews.len(), inserted["block_ids"].as_array().unwrap().len());
        let line = previews.last().unwrap().as_str().unwrap();
        let (preview_id, head) = line.split_once(' ').expect("id and head");
        assert_eq!(head, "Inserted p", "first ten characters of the STORED text: {line}");
        // The id in the preview is the id the caller uses next. Target the
        // follow-up edit with it, not with the separate block_ids array.
        let new_id = preview_id.to_string();
        assert_eq!(
            inserted["block_ids"].as_array().unwrap().last().unwrap().as_str().unwrap(),
            new_id,
            "preview id is the full stored id"
        );

        let edited = edit_text(&app, "g", "doc-preview", &new_id, "Edited: ", json!({})).expect("edit");
        assert_eq!(
            edited["block"].as_str().unwrap(),
            format!("{new_id} Edited: In"),
            "editText ack previews the post-edit text: {edited}"
        );

        let updated = crate::app_runtime::async_runtime::block_on(enqueue(
            &app,
            "block.update",
            "g",
            "doc-preview",
            json!({ "documentId": "doc-preview", "blockId": new_id, "attrs": { "checked": true } }),
        ))
        .expect("update");
        assert_eq!(updated["updated"].as_array().unwrap().len(), 1);
        assert_eq!(updated["blocks"].as_array().unwrap().len(), 1, "update ack previews: {updated}");

        // A giant single-leaf paragraph: the preview is still ten characters
        // and the insert returns promptly (the leaf is not rendered whole).
        let giant = format!("Giant leaf {}", "z".repeat(400_000));
        let started = std::time::Instant::now();
        let big = crate::app_runtime::async_runtime::block_on(enqueue(
            &app,
            "block.insert",
            "g",
            "doc-preview",
            json!({ "documentId": "doc-preview", "content": giant, "format": "plain" }),
        ))
        .expect("giant insert");
        let line = big["blocks"].as_array().unwrap().last().unwrap().as_str().unwrap();
        assert!(line.ends_with(" Giant leaf"), "ten-char head of a 400k leaf: {line}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "giant insert took {:?}",
            started.elapsed()
        );
    });
}

#[test]
fn chronicle_stamps_principals_guards_staleness_and_records_force() {
    with_profile("garden-chronicle", || {
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);
        seed_graph(&app, "chr");

        // 1. A write with no request principal is the local human owner's.
        write(&app, "chr", "doc-a", "# One\n\nfirst body", json!({})).expect("owner write");
        let first = record(&app, "chr", "doc-a");
        assert_eq!(label(&first.chronicle.created_by).as_deref(), Some("human:owner"));
        assert_eq!(label(&first.chronicle.updated_by).as_deref(), Some("human:owner"));
        assert_eq!(first.chronicle.recent_authors.len(), 1);
        assert_eq!(first.revision, 1);
        assert!(first.chronicle.created_at_source.is_none(), "native clock, no backfill flag");

        // 2. An agent principal (what the loopback middleware + MCP dispatcher
        //    establish) is stamped as updatedBy; the creator does not move.
        let beta = ChronicleActor::agent("beta", Some("master-token".to_string()));
        crate::app_runtime::async_runtime::block_on(with_actor(beta.clone(), async {
            enqueue(
                &app,
                "document.write",
                "chr",
                "doc-a",
                json!({
                    "documentId": "doc-a",
                    "content": "# One\n\nsecond body",
                    "format": "markdown",
                    "title": "Chronicle Doc",
                    "actor": "spoofed:client",
                }),
            )
            .await
        }))
        .expect("agent write");
        let second = record(&app, "chr", "doc-a");
        assert_eq!(second.created_at, first.created_at, "createdAt is stable");
        assert_eq!(label(&second.chronicle.created_by).as_deref(), Some("human:owner"));
        assert_eq!(second.chronicle.updated_by, Some(beta.clone()), "server principal wins over payload");
        assert_eq!(second.revision, 2);
        assert_eq!(second.chronicle.recent_authors[0].actor, beta);
        assert_eq!(second.chronicle.recent_authors[1].actor, ChronicleActor::human_owner());

        // 3. The staleness guard: a stale expectedRevision is refused, and
        //    nothing about the record moves.
        let stale = write(
            &app,
            "chr",
            "doc-a",
            "# One\n\nstale intention",
            json!({ "expectedRevision": 0 }),
        )
        .expect_err("stale write refused");
        assert!(stale.contains("revision conflict"), "{stale}");
        let after_refusal = record(&app, "chr", "doc-a");
        assert_eq!(after_refusal.revision, 2);
        assert_eq!(after_refusal.chronicle.forced_writes, 0);
        assert!(after_refusal.body.contains("second body"));

        // 4. `force` (the MCP builder drops the guard and carries the flag):
        //    the write lands and the override is recorded, never silent.
        crate::app_runtime::async_runtime::block_on(with_actor(beta.clone(), async {
            enqueue(
                &app,
                "document.write",
                "chr",
                "doc-a",
                json!({
                    "documentId": "doc-a",
                    "content": "# One\n\nforced body",
                    "format": "markdown",
                    "title": "Chronicle Doc",
                    "force": true,
                }),
            )
            .await
        }))
        .expect("forced write");
        let forced = record(&app, "chr", "doc-a");
        assert_eq!(forced.revision, 3);
        assert_eq!(forced.chronicle.forced_writes, 1);
        assert_eq!(forced.chronicle.last_forced_by, Some(beta.clone()));
        assert_eq!(forced.chronicle.last_forced_at.as_deref(), Some(forced.updated_at.as_str()));
        assert!(forced.body.contains("forced body"));

        // 5. Surgical edits carry the same guard: stale refused, current
        //    accepted, forced accepted and recorded.
        let block_id = forced.blocks.last().expect("a block").id.clone();
        let stale_edit = edit_text(&app, "chr", "doc-a", &block_id, "x", json!({ "expectedRevision": 1 }))
            .expect_err("stale block edit refused");
        assert!(stale_edit.contains("revision conflict"), "{stale_edit}");
        assert_eq!(record(&app, "chr", "doc-a").revision, 3);

        edit_text(
            &app,
            "chr",
            "doc-a",
            &block_id,
            "guarded ",
            json!({ "expectedRevision": 3 }),
        )
        .expect("current block edit accepted");
        let guarded = record(&app, "chr", "doc-a");
        assert_eq!(guarded.revision, 4);
        assert_eq!(label(&guarded.chronicle.updated_by).as_deref(), Some("human:owner"));
        assert!(guarded.body.contains("guarded "));

        crate::app_runtime::async_runtime::block_on(with_actor(beta.clone(), async {
            enqueue(
                &app,
                "block.editText",
                "chr",
                "doc-a",
                json!({
                    "documentId": "doc-a",
                    "blockId": block_id,
                    "operations": [{ "type": "insert", "offset": 0, "text": "forced " }],
                    "expectedRevision": 1,
                    "force": true,
                }),
            )
            .await
        }))
        .expect("forced block edit");
        let forced_edit = record(&app, "chr", "doc-a");
        assert_eq!(forced_edit.revision, 5);
        assert_eq!(forced_edit.chronicle.forced_writes, 2);
        assert_eq!(forced_edit.chronicle.updated_by, Some(beta.clone()));
        assert!(forced_edit.body.contains("forced guarded "));

        // 6. A malformed guard is a caller error, not a silent pass.
        let malformed = edit_text(&app, "chr", "doc-a", &block_id, "y", json!({ "expectedRevision": -4 }))
            .expect_err("malformed guard rejected");
        assert!(malformed.contains("expectedRevision"), "{malformed}");
    });
}

#[test]
fn chronicle_carries_agentic_time_as_the_cantor_last_witnessed_it() {
    with_profile("garden-chronicle-clock", || {
        let clock_path = std::env::temp_dir().join(format!("garden-clock-{}.json", Uuid::new_v4()));
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_secs_f64();
        std::fs::write(
            &clock_path,
            json!({
                "schema": 1,
                "witnessed_at": now,
                "witnessed_at_iso": "2026-09-18T14:23:08-07:00",
                "day": "2026-09-18",
                "stanza": 4.283,
                "stanza_tokens": 4_283_000,
                "agents": {"beta": {"watch": 15, "watch_hex": "0xf", "measure": 14}},
                "provenance": {"evidence": "inferred"}
            })
            .to_string(),
        )
        .expect("write clock");
        std::env::set_var(crate::agentic_clock::CLOCK_PATH_ENV, &clock_path);
        let result = std::panic::catch_unwind(|| {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            seed_graph(&app, "clk");

            // The owner's first write: house clock only, no watch — the
            // Cantor witnesses agents, not the human at the keyboard.
            write(&app, "clk", "doc-t", "# T\n\nfirst", json!({})).expect("owner write");
            let first = record(&app, "clk", "doc-t");
            let created = first.chronicle.created_clock.clone().expect("created clock");
            assert_eq!(created.day, "2026-09-18");
            assert_eq!(created.stanza_tokens, 4_283_000);
            assert_eq!(created.watch, None);
            assert_eq!(first.chronicle.updated_clock, Some(created.clone()));

            // An agent's write: the update stamp carries its watch and
            // measure; the creation stamp does not move.
            let beta = ChronicleActor::agent("beta", Some("master-token".to_string()));
            crate::app_runtime::async_runtime::block_on(with_actor(beta, async {
                enqueue(&app, "document.write", "clk", "doc-t", json!({
                    "documentId": "doc-t", "content": "# T\n\nsecond", "format": "markdown", "title": "T",
                })).await
            }))
            .expect("agent write");
            let second = record(&app, "clk", "doc-t");
            assert_eq!(second.chronicle.created_clock, Some(created));
            let updated = second.chronicle.updated_clock.clone().expect("updated clock");
            assert_eq!(updated.watch, Some(15));
            assert_eq!(updated.measure, Some(14));
            let shown = second.chronicle.to_json();
            assert_eq!(shown["updatedClock"]["watchHex"], json!("0xf"));
            assert_eq!(shown["updatedClock"]["stanza"], json!(4.283));
            assert_eq!(shown["updatedClock"]["evidence"], json!("inferred"));

            // The Cantor goes quiet: later writes keep the last honest stamp
            // rather than clearing it or inventing a fresh one.
            std::env::remove_var(crate::agentic_clock::CLOCK_PATH_ENV);
            write(&app, "clk", "doc-t", "# T\n\nthird", json!({})).expect("unwitnessed write");
            let third = record(&app, "clk", "doc-t");
            assert_eq!(third.chronicle.updated_clock, Some(updated));
        });
        std::env::remove_var(crate::agentic_clock::CLOCK_PATH_ENV);
        let _ = std::fs::remove_file(&clock_path);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    });
}
