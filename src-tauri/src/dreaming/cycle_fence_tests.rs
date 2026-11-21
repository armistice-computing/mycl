//! A dream cycle against a real (isolated) graph, with the dreamer held
//! mid-cycle while the graph is deleted, recreated, restored, opted out of,
//! or rearranged; and a publication paused while its caller is aborted.
//! Epsilon's Phase 3 acceptance (2026-10-07): none of the journal, the dream
//! state, the last-cycle receipt or the schedule record may land in a graph
//! that changed underneath the cycle, and the profile's job receipt keeps
//! the paid-for dreams. Never touches the caller's real profile.

use super::articulate::Articulation;
use super::cycle::{run_cycle, synthetic, Articulated, ArticulateHook, CycleHooks, DreamState, LoadHook, MANUAL};
use super::journal::journal_doc_id;
use crate::app_runtime::{async_runtime::block_on, AppHandle};
use crate::dreamer::ConsentGate;
use crate::graph_service::{create_graph_service, read_graph_record, CreateGraphInput};
use serde_json::{json, Value};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{atomic::AtomicBool, mpsc, Arc, Mutex};
use std::time::Duration;
#[cfg(feature = "desktop")]
use tauri::Manager;

thread_local! {
    /// Each test dreams on its own graph id: a held cycle in one failing
    /// test must not claim the graph another test uses.
    static GRAPH_ID: std::cell::RefCell<String> = std::cell::RefCell::new(String::new());
}

fn graph() -> String {
    GRAPH_ID.with(|g| g.borrow().clone())
}

struct IsolatedProfile {
    path: PathBuf,
    previous: Option<OsString>,
}

impl IsolatedProfile {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("garden-dream-fence-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        let previous = std::env::var_os("GARDEN_PROFILE_DIR");
        std::env::set_var("GARDEN_PROFILE_DIR", &path);
        Self { path, previous }
    }
}

impl Drop for IsolatedProfile {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(previous) => std::env::set_var("GARDEN_PROFILE_DIR", previous),
            None => std::env::remove_var("GARDEN_PROFILE_DIR"),
        }
        if !std::thread::panicking() {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

fn create(app: &AppHandle) -> PathBuf {
    create_graph_service(app, CreateGraphInput {
        graph_id: Some(graph()),
        title: "Dream fence".into(),
        description: None,
        operation_id: None,
    })
    .expect("create isolated graph");
    read_graph_record(app, &graph()).unwrap().0
}

fn isolated(test: impl FnOnce(&AppHandle, PathBuf)) {
    let _serial = crate::tauri_runtime::profile_env_serial().lock().unwrap_or_else(|p| p.into_inner());
    let profile = IsolatedProfile::new();
    GRAPH_ID.with(|g| *g.borrow_mut() = format!("dream-fence-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
    let app = crate::tauri_runtime::build_mock_app_for_tests(true);
    let graph_dir = create(&app);
    assert!(graph_dir.canonicalize().unwrap().starts_with(profile.path.canonicalize().unwrap()));
    test(&app, graph_dir);
}

/// The synthetic five-chain graph instead of the semantic index.
fn synthetic_load() -> LoadHook {
    Arc::new(|_dir: &Path, graph_id: &str| Ok(synthetic(graph_id, DreamState::default(), &[])))
}

/// A dreamer that asks the gate (one send), reports that it has started and
/// waits for release; the reply to that send then arrives. With
/// `second_send`, it asks the gate once more after release, as the next
/// bundle's send would. Every gate answer is recorded.
struct Held {
    hook: ArticulateHook,
    started: mpsc::Receiver<()>,
    release: Arc<tokio::sync::Notify>,
    sends: Arc<Mutex<Vec<bool>>>,
}

fn held_dreamer(second_send: bool) -> Held {
    let (tx, started) = mpsc::channel();
    let tx = Arc::new(Mutex::new(tx));
    let release = Arc::new(tokio::sync::Notify::new());
    let sends = Arc::new(Mutex::new(Vec::new()));
    let (hook_release, hook_sends) = (release.clone(), sends.clone());
    let hook: ArticulateHook = Arc::new(move |prompts: Vec<String>, gate: ConsentGate| {
        let (tx, release, sends) = (tx.clone(), hook_release.clone(), hook_sends.clone());
        Box::pin(async move {
            let mut out = Vec::new();
            for (i, _) in prompts.iter().enumerate() {
                let sent = gate().is_ok();
                sends.lock().unwrap().push(sent);
                if i == 0 {
                    let _ = tx.lock().unwrap().send(());
                    release.notified().await;
                    if second_send {
                        let next = gate().is_ok();
                        sends.lock().unwrap().push(next);
                    }
                }
                let articulation = sent.then(|| Articulation {
                    kind: "scene".into(),
                    narrative: format!("dream {i}"),
                    confidence: 0.8,
                    is_navel: false,
                });
                out.push(Articulated { articulation, receipts: Vec::new() });
            }
            out
        })
    });
    Held { hook, started, release, sends }
}

fn open_gate() -> ConsentGate {
    Arc::new(|| Ok("test".to_string()))
}

type CycleHandle = tokio::task::JoinHandle<Result<Value, String>>;

fn spawn_cycle(app: &AppHandle, consent: ConsentGate, hooks: CycleHooks) -> CycleHandle {
    let (app, graph_id) = (app.clone(), graph());
    crate::app_runtime::async_runtime::spawn(async move {
        run_cycle(app, graph_id, false, consent, Arc::new(AtomicBool::new(false)), MANUAL, hooks).await
    })
}

fn finish(handle: CycleHandle) -> Value {
    block_on(handle).expect("cycle task").expect("cycle result")
}

/// What a cycle can write into a graph directory.
fn written(graph_dir: &Path) -> Vec<&'static str> {
    let mut out = Vec::new();
    let checks: [(&str, PathBuf); 4] = [
        ("journal", graph_dir.join("narrative/derived-views").join(format!("{}.json", journal_doc_id(&graph())))),
        ("dream-state", graph_dir.join("dreaming/dream-state.json")),
        ("last-cycle", graph_dir.join("dreaming/last-cycle.json")),
        ("schedule", graph_dir.join("dreaming/schedule.json")),
    ];
    for (name, path) in checks {
        if path.exists() {
            out.push(name);
        }
    }
    out
}

fn schedule_status(graph_dir: &Path) -> String {
    super::scheduler::read_cycle_log(graph_dir).unwrap().last_status
}

#[test]
fn positive_control_a_held_cycle_on_an_unchanged_graph_publishes_everything() {
    isolated(|app, graph_dir| {
        let held = held_dreamer(true);
        let hooks = CycleHooks { load: Some(synthetic_load()), articulate: Some(held.hook.clone()), ..Default::default() };
        let handle = spawn_cycle(app, open_gate(), hooks);
        held.started.recv_timeout(Duration::from_secs(20)).expect("dreamer started");
        held.release.notify_one();
        let receipt = finish(handle);
        assert_eq!(receipt["status"], "completed", "{receipt}");
        assert_eq!(written(&graph_dir), ["journal", "dream-state", "last-cycle", "schedule"]);
        assert_eq!(schedule_status(&graph_dir), "completed");
        assert!(held.sends.lock().unwrap().iter().all(|s| *s));
    });
}

/// The ids of the dreamtOf wires in the graph's materialized workspace.
fn dream_wire_ids(graph_dir: &Path) -> Vec<String> {
    let snapshot: Value = crate::storage::read_json(&crate::ydoc_paths::workspace_snapshot_path(graph_dir)).unwrap();
    snapshot["wires"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|w| crate::wire_predicates::WireFamily::of_wire(w) == crate::wire_predicates::WireFamily::Dream)
        .map(|w| {
            assert!(crate::wire_state::wire_expires_at_ms(w).is_some(), "a server dream wire carries its expiry: {w}");
            assert_eq!(w["sourceDocumentId"], journal_doc_id(&graph()));
            w["id"].as_str().unwrap().to_string()
        })
        .collect()
}

#[test]
fn a_cycle_wires_its_journal_and_the_next_cycle_deletes_those_wires_first() {
    isolated(|app, graph_dir| {
        let run = || {
            let held = held_dreamer(false);
            let hooks = CycleHooks { load: Some(synthetic_load()), articulate: Some(held.hook.clone()), ..Default::default() };
            let handle = spawn_cycle(app, open_gate(), hooks);
            held.started.recv_timeout(Duration::from_secs(20)).expect("dreamer started");
            held.release.notify_one();
            finish(handle)
        };
        let first = run();
        assert_eq!(first["status"], "completed", "{first}");
        assert_eq!(first["dreamWiresCleared"]["removed"], 0);
        let created = first["dreamWires"]["created"].as_u64().expect("dream wires created");
        assert!(created > 0, "{first}");
        let first_ids = dream_wire_ids(&graph_dir);
        assert_eq!(first_ids.len() as u64, created);
        assert!(graph_dir.join("dreaming/dream-wires.json").is_file());

        let second = run();
        assert_eq!(second["status"], "completed", "{second}");
        assert_eq!(second["dreamWiresCleared"]["removed"].as_u64(), Some(created));
        let second_ids = dream_wire_ids(&graph_dir);
        assert_eq!(second_ids.len() as u64, second["dreamWires"]["created"].as_u64().unwrap());
        assert!(second_ids.iter().all(|id| !first_ids.contains(id)), "last night's wires are gone, not kept");
    });
}

#[test]
fn a_graph_deleted_and_recreated_mid_cycle_receives_nothing() {
    isolated(|app, _graph_dir| {
        let held = held_dreamer(false);
        let hooks = CycleHooks { load: Some(synthetic_load()), articulate: Some(held.hook.clone()), ..Default::default() };
        let handle = spawn_cycle(app, open_gate(), hooks);
        held.started.recv_timeout(Duration::from_secs(20)).expect("dreamer started");
        crate::graph_service::soft_delete_graph_service(app, graph(), true).expect("delete");
        let replacement = create(app);
        held.release.notify_one();
        let receipt = finish(handle);
        assert_eq!(receipt["status"], "graph_changed", "{receipt}");
        assert_eq!(receipt["graphWritesSkipped"], true);
        assert!(written(&replacement).is_empty(), "{:?}", written(&replacement));
        assert!(receipt["journalMarkdown"].as_str().is_some_and(|m| m.contains("dream 0")), "the paid text is kept");
        let job = receipt["jobId"].as_str().unwrap_or_else(|| panic!("no job: {receipt}"));
        let jobs = crate::local_jobs::LocalJobRegistry::new(crate::profile_paths::jobs_dir(app).unwrap()).unwrap();
        assert!(jobs.get(job).unwrap().is_some(), "the profile keeps the receipt");
    });
}

#[test]
fn a_graph_deleted_mid_cycle_is_not_resurrected() {
    isolated(|app, graph_dir| {
        let held = held_dreamer(false);
        let hooks = CycleHooks { load: Some(synthetic_load()), articulate: Some(held.hook.clone()), ..Default::default() };
        let handle = spawn_cycle(app, open_gate(), hooks);
        held.started.recv_timeout(Duration::from_secs(20)).expect("dreamer started");
        crate::graph_service::soft_delete_graph_service(app, graph(), true).expect("delete");
        held.release.notify_one();
        let receipt = finish(handle);
        assert_eq!(receipt["status"], "graph_changed", "{receipt}");
        assert!(!graph_dir.exists(), "the deleted graph's directory came back");
    });
}

#[test]
fn a_graph_restored_mid_cycle_receives_nothing() {
    isolated(|app, graph_dir| {
        let captured = crate::time_travel_service::capture_restore_point(
            app, &graph(), crate::time_travel_types::RestorePointTrigger::Manual, Some("before".into()),
        )
        .expect("capture");
        let restore_point = captured["restorePointId"].as_str().or(captured["restore_point_id"].as_str())
            .unwrap_or_else(|| panic!("no restore point id in {captured}")).to_string();
        let held = held_dreamer(false);
        let hooks = CycleHooks { load: Some(synthetic_load()), articulate: Some(held.hook.clone()), ..Default::default() };
        let handle = spawn_cycle(app, open_gate(), hooks);
        held.started.recv_timeout(Duration::from_secs(20)).expect("dreamer started");
        let jobs = Arc::new(crate::local_jobs::LocalJobRegistry::new(crate::profile_paths::jobs_dir(app).unwrap()).unwrap());
        let started = crate::time_travel_restore_service::start_restore(app, jobs.clone(), &graph(), &restore_point, false)
            .expect("start restore");
        let operation = started["operationId"].as_str().unwrap().to_string();
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        let state = loop {
            let op = crate::time_travel_restore_service::get_restore_operation(&jobs, &operation).unwrap();
            let state = op["state"].as_str().unwrap_or("").to_string();
            if matches!(state.as_str(), "succeeded" | "completed" | "failed" | "rolled_back" | "cancelled") || std::time::Instant::now() > deadline {
                break (state, op);
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        assert_eq!(state.0, "succeeded", "the restore itself must succeed: {}", state.1);
        held.release.notify_one();
        let receipt = finish(handle);
        assert_eq!(receipt["status"], "graph_changed", "{receipt}");
        let after = written(&graph_dir);
        assert!(!after.contains(&"journal") && !after.contains(&"dream-state") && !after.contains(&"last-cycle"), "{after:?}");
        if after.contains(&"schedule") {
            assert_eq!(schedule_status(&graph_dir), "running", "the reservation is never settled into a restored graph");
        }
    });
}

#[test]
fn opting_out_after_the_last_response_withdraws_the_night_and_keeps_the_text() {
    isolated(|app, graph_dir| {
        super::graph_settings::set_nightly(&graph_dir, true).unwrap();
        let consent = super::scheduler::nightly_consent_gate(graph_dir.clone(), graph());
        let held = held_dreamer(false);
        let hooks = CycleHooks { load: Some(synthetic_load()), articulate: Some(held.hook.clone()), ..Default::default() };
        let handle = spawn_cycle(app, consent, hooks);
        held.started.recv_timeout(Duration::from_secs(20)).expect("dreamer started");
        // Every send has happened; the reply is on its way.
        super::graph_settings::set_nightly(&graph_dir, false).unwrap();
        held.release.notify_one();
        let receipt = finish(handle);
        assert_eq!(receipt["status"], "withdrawn", "{receipt}");
        assert!(receipt["journalMarkdown"].as_str().is_some_and(|m| m.contains("dream 0")));
        assert!(!written(&graph_dir).contains(&"journal"));
        assert!(!written(&graph_dir).contains(&"dream-state"));
        assert_eq!(schedule_status(&graph_dir), "withdrawn");
    });
}

#[test]
fn a_dreamed_document_moved_into_an_excluded_folder_mid_cycle_goes_no_further() {
    isolated(|app, graph_dir| {
        // "c" sits at the root; the folder "private" is excluded.
        block_on(crate::crdt_engine::workspace_ops::upsert_document_entry(app, &graph(),
            &json!({"documentId":"c","title":"Doc c","order":0,"updatedAt":1}), "fence-test")).unwrap();
        super::exclusions::write(&graph_dir, &super::exclusions::Exclusions { folders: vec!["private".into()], documents: vec![] }).unwrap();
        let held = held_dreamer(true);
        let hooks = CycleHooks { load: Some(synthetic_load()), articulate: Some(held.hook.clone()), ..Default::default() };
        let handle = spawn_cycle(app, open_gate(), hooks);
        held.started.recv_timeout(Duration::from_secs(20)).expect("dreamer started");
        // The person moves "c" into the excluded folder; the exclusions file
        // itself does not change.
        block_on(crate::crdt_engine::workspace_ops::upsert_document_entry(app, &graph(),
            &json!({"documentId":"c","title":"Doc c","parentId":"private","order":0,"updatedAt":2}), "fence-test")).unwrap();
        held.release.notify_one();
        let receipt = finish(handle);
        assert_eq!(*held.sends.lock().unwrap(), vec![true, false], "the next send after the move is refused");
        // The reply to the first send arrived, but it is not published.
        assert_eq!(receipt["status"], "withdrawn", "{receipt}");
        assert!(!written(&graph_dir).contains(&"journal"));
    });
}

#[test]
fn an_aborted_caller_does_not_release_the_graph_while_publication_writes() {
    isolated(|app, graph_dir| {
        let held = held_dreamer(false);
        held.release.notify_one(); // the dreamer answers at once
        let (at_publish_tx, at_publish) = mpsc::channel();
        let (go_tx, go) = mpsc::channel::<()>();
        let at_publish_tx = Mutex::new(at_publish_tx);
        let go = Mutex::new(go);
        let hooks = CycleHooks {
            load: Some(synthetic_load()),
            articulate: Some(held.hook.clone()),
            before_publish: Some(Arc::new(move || {
                let _ = at_publish_tx.lock().unwrap().send(());
                let _ = go.lock().unwrap().recv_timeout(Duration::from_secs(30));
            })),
        };
        let handle = spawn_cycle(app, open_gate(), hooks);
        at_publish.recv_timeout(Duration::from_secs(20)).expect("publication reached");
        handle.abort();
        let gid = graph();
        let coordinator = app.state::<crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator>();
        let blocked = block_on(async { tokio::time::timeout(Duration::from_millis(300), coordinator.acquire_hot_write(&gid)).await });
        assert!(blocked.is_err(), "a restore or delete got the graph while the worker was writing");
        // Nor can a second cycle start.
        let second = block_on(run_cycle(app.clone(), graph(), false, open_gate(), Arc::new(AtomicBool::new(false)), MANUAL,
            CycleHooks { load: Some(synthetic_load()), ..Default::default() }));
        assert!(second.unwrap_err().contains("already running"));
        go_tx.send(()).unwrap();
        let free = block_on(async { tokio::time::timeout(Duration::from_secs(20), coordinator.acquire_hot_write(&gid)).await });
        assert!(free.is_ok_and(|lease| lease.is_ok()), "the worker never released the graph");
        assert!(written(&graph_dir).contains(&"journal"), "the worker finished its publication");
    });
}
