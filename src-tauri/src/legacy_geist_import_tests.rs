//! Cross-service migration tests; never point the runtime at a real profile.
//! Register from lib.rs with #[cfg(all(test, feature = "headless"))].
#![cfg(all(test, feature = "headless"))]

use crate::{
    app_runtime::AppHandle,
    crdt_engine::{
        builder::ydoc_from_tiptap_json,
        import_archive_ops::{parse_graph_archive, GraphArchiveManifest, ParsedGraphArchive},
    },
    geist_memory_recall_service::mcp_local_recall_memories,
    geist_memory_service::mcp_local_remember,
    geist_memory_store::{memory_json, read_memory_store},
    geist_song_service::{mcp_local_music, mcp_local_sing},
    geist_song_store::{read_song_store, song_music_json},
    graph_service::{create_graph_service, read_graph_record, CreateGraphInput},
    legacy_geist_import::{plan, LegacyGeistPlan},
    salience_value_store::{read_value_store, value_mutation_guard},
};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};
use yrs::{ReadTxn, StateVector, Transact};

const GRAPH: &str = "legacy-geist-rehearsal";

struct IsolatedProfile {
    path: PathBuf,
    previous: Option<OsString>,
}

impl IsolatedProfile {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("garden-legacy-geist-{}", uuid::Uuid::new_v4()));
        // create_dir (not create_dir_all) refuses reuse of any existing profile.
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
        // A timed-out queued operation may still be draining. Never let it fall
        // back to the caller's real profile during panic unwinding.
        if std::thread::panicking() {
            eprintln!(
                "failed isolated Geist profile retained at {}",
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
            title: "Isolated legacy Geist rehearsal".into(),
            description: None,
            operation_id: None,
        },
    )
    .expect("create isolated graph");
    let (graph_dir, _) = read_graph_record(&app, GRAPH).expect("isolated graph exists");
    assert!(graph_dir
        .canonicalize()
        .unwrap()
        .starts_with(profile.path.canonicalize().unwrap()));
    test(&app, &graph_dir);
}

fn install(plan: &LegacyGeistPlan, graph_dir: &Path) -> Result<(), String> {
    let _guard = value_mutation_guard()?;
    plan.install(graph_dir, GRAPH)
}

fn reconcile(planned: &LegacyGeistPlan, graph_dir: &Path) {
    let documents = file_tree(&graph_dir.join("documents"));
    let ydocs = file_tree(&graph_dir.join("ydocs"));
    // install's value guard is released before derived-state reconciliation.
    planned
        .reconcile(graph_dir, GRAPH)
        .expect("reconcile imported Geist authorities");
    assert_eq!(file_tree(&graph_dir.join("documents")), documents);
    assert_eq!(file_tree(&graph_dir.join("ydocs")), ydocs);
}

fn seed_source_ydocs(archive: &ParsedGraphArchive, graph_dir: &Path) {
    for (id, bytes) in &archive.documents {
        if !id.starts_with("geist-") {
            continue;
        }
        crate::paths::ensure_document_dirs(graph_dir, id).unwrap();
        let path = crate::paths::document_ydoc_state_path(graph_dir, id);
        assert!(!path.exists());
        fs::write(path, bytes).unwrap();
    }
}

fn source_backup_matches(archive: &ParsedGraphArchive, graph_dir: &Path) {
    use base64::Engine;
    let backup: Value = serde_json::from_slice(
        &fs::read(graph_dir.join("memory/legacy-geist-source.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(backup["sourceGraphId"], archive.manifest.source_graph_id);
    assert_eq!(backup["sourceUserId"], archive.manifest.source_user_id);
    let sources = archive
        .documents
        .iter()
        .filter(|(id, _)| id.starts_with("geist-"))
        .collect::<Vec<_>>();
    assert_eq!(
        backup["documentsBase64"].as_object().unwrap().len(),
        sources.len()
    );
    for (id, bytes) in sources {
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(backup["documentsBase64"][id].as_str().unwrap())
                .unwrap(),
            *bytes,
            "source backup must retain exact archive bytes for {id}"
        );
    }
}

fn assert_native_song_projection(graph_dir: &Path, id: &str) {
    use yrs::{updates::decoder::Decode, Doc, Update};
    let bytes = fs::read(crate::paths::document_ydoc_state_path(graph_dir, id))
        .expect("native projection has a persisted Y.Doc");
    let doc = Doc::new();
    doc.transact_mut()
        .apply_update(Update::decode_v1(&bytes).unwrap())
        .unwrap();
    let snapshot = crate::crdt_engine::projection::materialize_ydoc(&doc, id);
    assert!(!snapshot.tiptap_json["content"]
        .as_array()
        .unwrap()
        .is_empty());
    let record: crate::document_service::DocumentRecord = serde_json::from_slice(
        &fs::read(graph_dir.join("documents").join(id).join("document.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(record.tiptap_json.as_ref(), Some(&snapshot.tiptap_json));
    assert_eq!(record.body, snapshot.body);
    if id == "geist-song-native" {
        assert!(record.body.contains("Only the isolated rehearsal sings."));
    }
}

fn file_tree(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        if !dir.exists() {
            return;
        }
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            assert!(
                !entry.file_type().unwrap().is_symlink(),
                "unexpected fixture symlink"
            );
            if entry.file_type().unwrap().is_dir() {
                visit(root, &entry.path(), out);
            } else {
                out.insert(
                    entry.path().strip_prefix(root).unwrap().to_owned(),
                    fs::read(entry.path()).unwrap(),
                );
            }
        }
    }
    let mut files = BTreeMap::new();
    visit(root, root, &mut files);
    files
}

fn authority_files(graph_dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    for root in ["memory", "narrative", "values"] {
        for (path, bytes) in file_tree(&graph_dir.join(root)) {
            files.insert(Path::new(root).join(path), bytes);
        }
    }
    files
}

fn node(kind: &str, id: &str, text: &str) -> Value {
    let mut node = json!({"type":kind,"attrs":{"data-block-id":id},
        "content":[{"type":"text","text":text}]});
    if kind == "heading" {
        node["attrs"]["level"] = json!(1);
    }
    node
}

fn document(id: &str, nodes: Vec<Value>) -> (String, Vec<u8>) {
    let doc = ydoc_from_tiptap_json(&json!({"type":"doc","content":nodes}));
    let bytes = doc
        .transact()
        .encode_state_as_update_v1(&StateVector::default());
    (id.into(), bytes)
}

fn fixture() -> ParsedGraphArchive {
    let meta = json!({"next_number":11,"memories":{
        "7":{"b":"memory-seven","c":"2026-08-20T12:00:00Z","a":"2026-08-21T12:00:00Z"},
        "10":{"b":"memory-ten","c":"2026-08-22T12:00:00Z","a":"2026-08-23T12:00:00Z"}
    }});
    ParsedGraphArchive {
        manifest: GraphArchiveManifest {
            source_user_id: "legacy-owner".into(),
            source_graph_id: "default".into(),
            source_graph_title: Some("default".into()),
            source_graph_description: None,
            includes_artifacts: false,
            exported_at: Some("2026-08-25T15:45:55Z".into()),
        },
        rdf_n_quads: String::new(),
        workspace_bytes: None,
        documents: vec![
            document(
                "geist-song",
                vec![
                    node("heading", "song-title", "The Song"),
                    node("paragraph", "verse-zero", "Newest source verse"),
                    json!({"type":"horizontalRule"}),
                    node("paragraph", "verse-one", "Middle source verse"),
                    json!({"type":"horizontalRule"}),
                    node("paragraph", "verse-two", "Oldest source verse"),
                ],
            ),
            document(
                "geist-memory-queue",
                vec![
                    node("heading", "queue-title", "Memory Queue"),
                    node("paragraph", "queue-meta", &format!("__geist_meta__:{meta}")),
                    node(
                        "paragraph",
                        "memory-seven",
                        "7. Keep the exact source text.",
                    ),
                    node(
                        "paragraph",
                        "memory-ten",
                        "10. 10. Preserve a repeated label.",
                    ),
                ],
            ),
            document(
                "geist-importance",
                vec![node(
                    "paragraph",
                    "importance",
                    "Keep this importance prompt.",
                )],
            ),
            document(
                "geist-valence",
                vec![node("paragraph", "valence", "Keep this valence prompt.")],
            ),
            document(
                "geist-weights",
                vec![node(
                    "paragraph",
                    "weights",
                    "half_life_days: 13\nimportance_weight: 0.42",
                )],
            ),
        ],
        originals: vec![],
        chronicle: vec![],
        warnings: vec![],
    }
}

fn readers_match(app: &AppHandle, graph_dir: &Path, planned: &LegacyGeistPlan) {
    let args = json!({"graph_id":GRAPH});
    let authorities = authority_files(graph_dir);
    let documents = file_tree(&graph_dir.join("documents"));
    let ydocs = file_tree(&graph_dir.join("ydocs"));
    if let Some(song) = &planned.song {
        assert_eq!(
            mcp_local_music(app.clone(), &args).unwrap(),
            song_music_json(song)
        );
        assert_eq!(
            serde_json::to_value(read_song_store(graph_dir, GRAPH).unwrap()).unwrap(),
            serde_json::to_value(song).unwrap()
        );
    }
    if let Some(memory) = &planned.memory {
        assert_eq!(
            serde_json::to_value(read_memory_store(graph_dir, GRAPH).unwrap()).unwrap(),
            serde_json::to_value(memory).unwrap()
        );
        for record in memory.memories.values() {
            let response = mcp_local_recall_memories(
                app.clone(),
                &json!({"graph_id":GRAPH,"number":record.number}),
            )
            .unwrap();
            assert_eq!(response["memories"], json!([memory_json(record)]));
        }
    }
    if let Some(config) = &planned.config {
        // The `get_values` tool was cut on 2026-10-28; the imported value
        // config is read straight from the store.
        let store = read_value_store(graph_dir, GRAPH).unwrap();
        assert_eq!(
            serde_json::to_value(&store.config).unwrap(),
            serde_json::to_value(config).unwrap()
        );
    }
    assert_eq!(
        authority_files(graph_dir),
        authorities,
        "readers must not rewrite authorities"
    );
    assert_eq!(
        file_tree(&graph_dir.join("documents")),
        documents,
        "music/recall/get_values must not create or rewrite source documents"
    );
    assert_eq!(
        file_tree(&graph_dir.join("ydocs")),
        ydocs,
        "readers must not rewrite source Y.Docs"
    );
}

fn first_mutations_preserve(app: &AppHandle, graph_dir: &Path, planned: &LegacyGeistPlan) {
    let source = fs::read(graph_dir.join("memory/legacy-geist-source.json")).unwrap();
    // Source document Y.Docs are immutable. The workspace is a live catalog:
    // native Geist mutations deliberately register derived document entries.
    let source_ydocs = file_tree(&graph_dir.join("ydocs/documents"));
    if let Some(memory) = &planned.memory {
        if let Some(record) = memory.memories.values().next() {
            // The `care` tool (the first mutation this rehearsal used) was cut
            // on 2026-10-28; `remember` is the first mutation now.
            let _ = record;
            let expected = serde_json::to_value(memory).unwrap();
            let response = crate::app_runtime::async_runtime::block_on(mcp_local_remember(
                app.clone(),
                &json!({"graph_id":GRAPH,"content":"Isolated migration continuity check."}),
                None,
            )).unwrap();
            assert_eq!(response["ok"], true, "{response}");
            let after = read_memory_store(graph_dir, GRAPH).unwrap();
            assert_eq!(after.next_number, memory.next_number + 1);
            assert_eq!(after.memories.len(), memory.memories.len() + 1);
            assert_eq!(after.memories[&memory.next_number.to_string()].content, "Isolated migration continuity check.");
            let mut actual = serde_json::to_value(&after).unwrap();
            actual["memories"].as_object_mut().unwrap().remove(&memory.next_number.to_string());
            actual["nextNumber"] = json!(memory.next_number);
            assert_eq!(actual, expected, "remember must not change imported memories");
        }
    }
    if let Some(song) = &planned.song {
        mcp_local_sing(
            app.clone(),
            &json!({"graph_id":GRAPH,"verse":"Only the isolated rehearsal sings."}),
        )
        .unwrap();
        let after = read_song_store(graph_dir, GRAPH).unwrap();
        assert_native_song_projection(graph_dir, "geist-song-native");
        if serde_json::from_slice::<Value>(&source).unwrap()["documentsBase64"]
            .get("geist-past-songs")
            .is_some()
        {
            assert_native_song_projection(graph_dir, "geist-past-songs-native");
        }
        assert_eq!(after.verses[0].text, "Only the isolated rehearsal sings.");
        let mut retained = after.verses[1..].to_vec();
        for archive in &after.archives[song.archives.len()..] {
            retained.extend(archive.verses.clone());
        }
        assert_eq!(
            serde_json::to_value(retained).unwrap(),
            serde_json::to_value(&song.verses).unwrap()
        );
        assert_eq!(
            serde_json::to_value(&after.archives[..song.archives.len()]).unwrap(),
            serde_json::to_value(&song.archives).unwrap()
        );
        if let Some(coda) = &song.coda {
            let remaining = coda.ejections_remaining - i64::from(song.verses.len() == 3);
            if remaining > 0 {
                let mut expected = serde_json::to_value(coda).unwrap();
                expected["ejectionsRemaining"] = json!(remaining);
                assert_eq!(serde_json::to_value(&after.coda).unwrap(), expected);
            } else {
                assert!(after.coda.is_none());
            }
        }
    }
    // The `revaluate` tool, the value-config mutation this rehearsal used,
    // was cut on 2026-10-28; the value config is settings-owned now.
    let _ = planned.config.as_ref();
    assert_eq!(
        fs::read(graph_dir.join("memory/legacy-geist-source.json")).unwrap(),
        source
    );
    for (relative, bytes) in source_ydocs {
        assert_eq!(
            fs::read(graph_dir.join("ydocs/documents").join(relative)).unwrap(),
            bytes,
            "first native mutations must retain source document Y.Docs byte-for-byte"
        );
    }
    let before_retry = authority_files(graph_dir);
    assert!(
        install(planned, graph_dir).is_err(),
        "replay after native edits must refuse overwrite"
    );
    assert_eq!(
        authority_files(graph_dir),
        before_retry,
        "conflicting replay must make no partial writes"
    );
}

#[test]
fn legacy_geist_plan_install_read_replay_and_first_mutations() {
    let archive = fixture();
    let planned = plan(&archive, GRAPH).expect("translate synthetic legacy authority");
    assert_eq!(planned.song.as_ref().unwrap().verses.len(), 3);
    assert!(planned
        .song
        .as_ref()
        .unwrap()
        .verses
        .iter()
        .all(|verse| verse.created_at.is_empty() && verse.updated_at.is_empty()));
    assert_eq!(
        planned.config.as_ref().unwrap().temporal_half_life_days,
        13.0
    );
    let memory = planned.memory.as_ref().unwrap();
    assert_eq!(memory.next_number, 11);
    assert_eq!(
        memory.memories["10"].content,
        "10. Preserve a repeated label."
    );
    isolated(|app, graph_dir| {
        seed_source_ydocs(&archive, graph_dir);
        install(&planned, graph_dir).unwrap();
        source_backup_matches(&archive, graph_dir);
        reconcile(&planned, graph_dir);
        let before = authority_files(graph_dir);
        install(&planned, graph_dir).expect("exact replay");
        assert_eq!(authority_files(graph_dir), before);
        readers_match(app, graph_dir, &planned);
        first_mutations_preserve(app, graph_dir, &planned);
    });
}

#[test]
fn legacy_geist_enqueue_archive_restores_runtime_and_derived_state() {
    enqueue_archive_rehearsal(false);
}

#[test]
fn legacy_geist_archive_workspace_survives_bootstrap_and_native_mutations() {
    enqueue_archive_rehearsal(true);
}

fn enqueue_archive_rehearsal(with_workspace: bool) {
    use base64::Engine;
    use flate2::{write::GzEncoder, Compression};
    use yrs::{Map, MapPrelim, WriteTxn};
    let mut archive = fixture();
    if with_workspace {
        archive.documents.push(document("ordinary-note", vec![node("paragraph", "ordinary-block", "Actual imported document")]));
        let workspace = yrs::Doc::new();
        {
            let mut txn = workspace.transact_mut();
            let folders = txn.get_or_insert_map("folders");
            let folder = folders.insert(&mut txn, "source-folder", MapPrelim::default());
            folder.insert(&mut txn, "id", "source-folder");
            folder.insert(&mut txn, "name", "Archive folder sentinel");
            let documents = txn.get_or_insert_map("documents");
            for (id, _) in &archive.documents {
                let entry = documents.insert(&mut txn, id.clone(), MapPrelim::default());
                entry.insert(&mut txn, "id", id.clone());
                entry.insert(&mut txn, "title", format!("Archive title {id}"));
                entry.insert(&mut txn, "parentId", "source-folder");
            }
            let wires = txn.get_or_insert_map("wires");
            let wire = wires.insert(&mut txn, "source-wire", MapPrelim::default());
            wire.insert(&mut txn, "sourceDocumentId", "ordinary-note");
            wire.insert(&mut txn, "targetDocumentId", "geist-song");
            wire.insert(&mut txn, "targetGraphId", archive.manifest.source_graph_id.clone());
            wire.insert(&mut txn, "predicate", "supports");
        }
        archive.workspace_bytes = Some(workspace.transact().encode_state_as_update_v1(&StateVector::default()));
    }
    let planned = plan(&archive, GRAPH).unwrap();
    let mut tar = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::fast()));
    let manifest = serde_json::to_vec(&json!({
        "version":1, "format":"mnemosyne-graph-export",
        "source_user_id":archive.manifest.source_user_id,
        "source_graph_id":archive.manifest.source_graph_id,
        "includes_artifacts":false,
        "files":{"rdf":"rdf/graph.nq","documents_dir":"crdt/documents/",
            "workspace":if with_workspace {"crdt/workspace.yjs"} else {""}}
    }))
    .unwrap();
    let mut entries = vec![
        ("manifest.json".to_owned(), manifest),
        ("rdf/graph.nq".to_owned(), Vec::new()),
    ];
    if let Some(workspace) = &archive.workspace_bytes {
        entries.push(("crdt/workspace.yjs".into(), workspace.clone()));
    }
    entries.extend(
        archive
            .documents
            .iter()
            .map(|(id, bytes)| (format!("crdt/documents/{id}.yjs"), bytes.clone())),
    );
    for (name, bytes) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o600);
        header.set_cksum();
        tar.append_data(&mut header, name, bytes.as_slice())
            .unwrap();
    }
    let bytes = tar.into_inner().unwrap().finish().unwrap();
    let _serial = crate::tauri_runtime::profile_env_serial()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let profile = IsolatedProfile::new();
    let app = crate::tauri_runtime::build_mock_app_for_tests(true);
    crate::app_runtime::async_runtime::block_on(crate::crdt_queue::enqueue_crdt_operation(
        app.clone(),
        crate::crdt_operation_types::EnqueueCrdtOperationInput {
            kind: "graph.importArchive".into(),
            graph_id: GRAPH.into(),
            document_id: None,
            payload: json!({"filename":"legacy-geist-fixture.tar.gz","newGraphId":GRAPH,
                "newTitle":"Isolated full import", "sizeBytes":bytes.len(),
                "tarGzBase64":base64::engine::general_purpose::STANDARD.encode(&bytes)}),
        },
    ))
    .expect("enqueue actual small archive import");
    let (graph_dir, _) = read_graph_record(&app, GRAPH).unwrap();
    assert!(graph_dir
        .canonicalize()
        .unwrap()
        .starts_with(profile.path.canonicalize().unwrap()));
    source_backup_matches(&archive, &graph_dir);
    // No manual install/reconcile here: this is the integration-placement assertion.
    readers_match(&app, &graph_dir, &planned);
    let store = crate::rdf_service::open_graph_store(&graph_dir).unwrap();
    let before = store
        .iter()
        .map(|q| q.unwrap().to_string())
        .collect::<std::collections::BTreeSet<_>>();
    assert!(
        before.iter().any(|q| q.contains(":song>")),
        "import must reconcile Song RDF"
    );
    assert!(
        before.iter().any(|q| q.contains(":memory:7>")),
        "import must reconcile memory RDF"
    );
    reconcile(&planned, &graph_dir);
    let after = store
        .iter()
        .map(|q| q.unwrap().to_string())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        before, after,
        "import must already have reconciled all derived Geist state"
    );
    first_mutations_preserve(&app, &graph_dir, &planned);
    if with_workspace {
        use yrs::{updates::decoder::Decode, Update};
        let bytes = fs::read(crate::ydoc_paths::workspace_ydoc_state_path(&graph_dir)).unwrap();
        assert_ne!(bytes, vec![0, 0], "imported workspace replaces bootstrap authority");
        let workspace = yrs::Doc::new();
        workspace.transact_mut().apply_update(Update::decode_v1(&bytes).unwrap()).unwrap();
        let snapshot = crate::crdt_engine::workspace_ops::materialize_workspace_snapshot_json(GRAPH, &workspace).unwrap();
        assert!(snapshot["folders"].as_array().unwrap().iter().any(|f| f["id"] == "source-folder" && f["name"] == "Archive folder sentinel"));
        assert!(snapshot["documents"].as_array().unwrap().iter().any(|d| d["id"] == "ordinary-note" && d["title"] == "Archive title ordinary-note" && d["parentId"] == "source-folder"));
        assert!(snapshot["wires"].as_array().unwrap().iter().any(|w| w["id"] == "source-wire" && w["targetGraphId"] == GRAPH && w["predicate"] == "supports" && w["sourceDocumentId"] == "ordinary-note" && w["targetDocumentId"] == "geist-song"));
        let document = crate::document_record_store::read_document_record(&graph_dir, &crate::paths::document_dir(&graph_dir, "ordinary-note").unwrap().join("document.json")).unwrap();
        assert!(document.body.contains("Actual imported document"));
    }
}

#[test]
fn legacy_geist_first_sing_preserves_counterpoint_and_coda_lifetime() {
    for remaining in [1, 2] {
        let mut archive = fixture();
        let meta = json!({"version":1,"verses":[
            {"text":"Newest","counterpoints":[]},
            {"text":"Middle","counterpoints":[]},
            {"text":"Oldest","counterpoints":["Another voice"]}
        ],"coda":{"text":"Held coda","ejections_remaining":remaining}});
        let heading = |level, text: &str| {
            let mut result = node("heading", text, text);
            result["attrs"]["level"] = json!(level);
            result
        };
        let italic = |text: &str| {
            let mut result = node("paragraph", text, text);
            result["content"][0]["marks"] = json!([{"type":"italic"}]);
            result
        };
        archive.documents[0] = document("geist-song", vec![
            heading(1,"The Song"), node("paragraph","meta",&format!("__song_meta__:{meta}")),
            heading(2,"Verse 0"),node("paragraph","v0","Newest"),json!({"type":"horizontalRule"}),
            heading(2,"Verse -1"),node("paragraph","v1","Middle"),json!({"type":"horizontalRule"}),
            heading(2,"Verse -2"),node("paragraph","v2","Oldest"),italic("Another voice"),json!({"type":"horizontalRule"}),
            heading(3,&format!("Coda ({remaining} remaining)")),italic("Held coda"),
        ]);
        let planned = plan(&archive, GRAPH).unwrap();
        isolated(|app, graph_dir| {
            seed_source_ydocs(&archive, graph_dir);
            install(&planned, graph_dir).unwrap();
            first_mutations_preserve(app, graph_dir, &planned);
        });
    }
}

#[test]
fn lightweight_remember_records_context_and_recall_defaults_to_eight() {
    isolated(|app, graph_dir| {
        for n in 0..10 {
            let actor = crate::document_chronicle::ChronicleActor::agent(
                if n % 2 == 0 { "epsilon" } else { "delta" }, Some("test-credential".into()));
            let result = crate::app_runtime::async_runtime::block_on(crate::document_chronicle::with_actor(
                actor.clone(), mcp_local_remember(app.clone(), &json!({"graph_id":GRAPH,"content":format!("Thought {n}"),"agent":"forged"}), Some(actor.clone())),
            )).unwrap();
            assert_eq!(result["number"], n + 1);
            assert_eq!(result["rememberer"], actor.to_value());
            assert_eq!(result["ok"], true);
        }
        let before = authority_files(graph_dir);
        let recall = mcp_local_recall_memories(app.clone(), &json!({"graph_id":GRAPH})).unwrap();
        assert_eq!(recall["count"], 8);
        let mine = mcp_local_recall_memories(app.clone(), &json!({"graph_id":GRAPH,"rememberer":"epsilon"})).unwrap();
        assert_eq!(mine["count"], 5);
        assert!(mine["memories"].as_array().unwrap().iter().all(|m|m["rememberer"]["name"] == "epsilon"));
        assert_eq!(before, authority_files(graph_dir), "recall must not change attention dates");
        for args in [json!({"content":" ","graph_id":GRAPH}),
            json!({"content":"Forged","graph_id":GRAPH,"rememberer":"other"}),
            json!({"content":"Typed","graph_id":GRAPH,"observer_agent_id":"other"})] {
            assert!(crate::app_runtime::async_runtime::block_on(mcp_local_remember(app.clone(), &args, None)).is_err());
        }
        assert_eq!(before, authority_files(graph_dir));
        assert!(!graph_dir.join("emporium/applied").exists(), "ordinary memory must not invent typed assertions");
    });
}

#[test]
fn legacy_geist_each_authority_conflict_is_non_destructive() {
    let planned = plan(&fixture(), GRAPH).unwrap();
    isolated(|_, graph_dir| {
        install(&planned, graph_dir).unwrap();
        for (relative, pointer) in [
            ("narrative/song.json", "/verses/0/text"),
            ("memory/memory-queue.json", "/memories/7/content"),
            ("values/block-values.json", "/config/importancePrompt"),
            ("memory/legacy-geist-source.json", "/sourceGraphId"),
        ] {
            let path = graph_dir.join(relative);
            let original = fs::read(&path).unwrap();
            let mut drifted: Value = serde_json::from_slice(&original).unwrap();
            *drifted.pointer_mut(pointer).unwrap() = json!("Conflicting existing authority");
            fs::write(&path, serde_json::to_vec(&drifted).unwrap()).unwrap();
            let before = authority_files(graph_dir);
            assert!(
                install(&planned, graph_dir).is_err(),
                "must reject {relative} conflict"
            );
            assert_eq!(
                authority_files(graph_dir),
                before,
                "partial write on {relative} conflict"
            );
            fs::write(path, original).unwrap();
        }
    });
}

#[test]
fn legacy_geist_recall_orders_mixed_clocks_by_instant() {
    isolated(|app, graph_dir| {
        let planned = plan(&fixture(), GRAPH).unwrap();
        install(&planned, graph_dir).unwrap();
        let mut store = read_memory_store(graph_dir, GRAPH).unwrap();
        // Lexical order puts the ISO year ahead of epoch milliseconds, incorrectly.
        store.memories.get_mut("7").unwrap().last_active = "1790000000000".into();
        store.memories.get_mut("10").unwrap().last_active = "2026-08-23T12:00:00Z".into();
        crate::geist_memory_store::write_memory_store(graph_dir, &store).unwrap();
        let response =
            mcp_local_recall_memories(app.clone(), &json!({"graph_id":GRAPH,"limit":2})).unwrap();
        assert_eq!(response["memories"][0]["number"], 7);
        assert_eq!(response["memories"][1]["number"], 10);
        let record = store.memories.get_mut("7").unwrap();
        record.created_at = "".into();
        record.last_active = "".into();
        assert_eq!(crate::geist_memory_store::memory_sort_key(record), i64::MIN);
        crate::geist_memory_store::write_memory_store(graph_dir, &store).unwrap();
        let response =
            mcp_local_recall_memories(app.clone(), &json!({"graph_id":GRAPH,"limit":2})).unwrap();
        assert_eq!(response["memories"][0]["number"], 10);
    });
}

#[test]
#[ignore = "Set GARDEN_MIGRATION_ARCHIVE; coordinate shared Cargo cache before running"]
fn legacy_geist_corpus_rehearsal() {
    let path = PathBuf::from(
        std::env::var_os("GARDEN_MIGRATION_ARCHIVE")
            .expect("set GARDEN_MIGRATION_ARCHIVE to an August or September cloud export"),
    );
    let bytes = fs::read(&path).expect("read corpus archive without modifying it");
    let archive = parse_graph_archive(&bytes).expect("parse corpus archive");
    let planned =
        plan(&archive, GRAPH).expect("translate corpus Geist; unsupported source must fail closed");
    assert!(
        planned.song.is_some() && planned.memory.is_some() && planned.config.is_some(),
        "cloud corpus must exercise all three authorities: {}",
        planned.summary()
    );
    let song = planned.song.as_ref().unwrap();
    let memory = planned.memory.as_ref().unwrap();
    eprintln!(
        "legacy Geist rehearsal: {}",
        json!({
            "summary":planned.summary(), "archive_documents":archive.documents.len(),
            "active_verses":song.verses.len(), "song_archives":song.archives.len(),
            "counterpoints":song.verses.iter().map(|v|v.counterpoints.len()).sum::<usize>(),
            "coda":song.coda.is_some(), "active_memories":memory.memories.len(),
            "memory_archives":memory.archives.len(),
            "archived_memories":memory.archives.iter().map(|a|a.memories.len()).sum::<usize>(),
            "next_number":memory.next_number,
            "weights":planned.config.as_ref().unwrap().weights.len()
        })
    );
    isolated(|app, graph_dir| {
        seed_source_ydocs(&archive, graph_dir);
        install(&planned, graph_dir).unwrap();
        source_backup_matches(&archive, graph_dir);
        reconcile(&planned, graph_dir);
        let before = authority_files(graph_dir);
        install(&planned, graph_dir).expect("corpus exact replay");
        assert_eq!(authority_files(graph_dir), before);
        readers_match(app, graph_dir, &planned);
        first_mutations_preserve(app, graph_dir, &planned);
    });
    assert_eq!(
        fs::read(path).unwrap(),
        bytes,
        "source archive must remain unchanged"
    );
}
