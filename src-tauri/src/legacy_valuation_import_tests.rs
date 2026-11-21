//! Isolated end-to-end coverage of cloud-1 valuations through graph.importArchive.

use crate::app_runtime::AppHandle;
use crate::crdt_engine::import_archive_ops::parse_graph_archive;
use crate::crdt_operation_types::EnqueueCrdtOperationInput;
use crate::crdt_queue::enqueue_crdt_operation;
use crate::graph_service::{create_graph_service, CreateGraphInput};
use crate::salience_value_store::{block_value_key, read_value_store};
use base64::Engine;
use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

struct IsolatedProfile {
    path: PathBuf,
    previous: Option<OsString>,
}

impl IsolatedProfile {
    // The caller holds profile_env_serial for this guard's entire lifetime.
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "garden-legacy-valuation-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&path).expect("create isolated profile");
        #[cfg(unix)] {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let previous = std::env::var_os("GARDEN_PROFILE_DIR");
        std::env::set_var("GARDEN_PROFILE_DIR", &path);
        Self { path, previous }
    }
}

impl Drop for IsolatedProfile {
    fn drop(&mut self) {
        if std::thread::panicking() && std::env::var_os("GARDEN_MIGRATION_KEEP_FAILED_PROFILE").is_some() {
            // A timed-out durable operation may still be draining. Keep its
            // process-local profile binding pinned until this test process exits.
            eprintln!("failed rehearsal profile retained at {}", self.path.display());
            return;
        }
        match &self.previous {
            Some(previous) => std::env::set_var("GARDEN_PROFILE_DIR", previous),
            None => std::env::remove_var("GARDEN_PROFILE_DIR"),
        }
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn import(app: &AppHandle, bytes: &[u8], target: &str) {
    crate::app_runtime::async_runtime::block_on(enqueue_crdt_operation(
        app.clone(),
        EnqueueCrdtOperationInput {
            kind: "graph.importArchive".into(),
            graph_id: target.into(),
            document_id: None,
            payload: json!({
                "filename": "legacy-valuation.tar.gz",
                "newGraphId": target,
                "newTitle": "Isolated valuation import",
                "tarGzBase64": base64::engine::general_purpose::STANDARD.encode(bytes),
                "sizeBytes": bytes.len(),
            }),
        },
    ))
    .expect("real CRDT archive import");
}

fn runtime_sample(app: &AppHandle, target: &str, document: &str, block: &str) -> Value {
    let result = crate::salience_service::mcp_local_get_block_values(
        app.clone(),
        &json!({"graph_id": target, "document_id": document, "block_id": block}),
    )
    .expect("native get_block_values");
    assert_eq!(result["source"], "local-value-store");
    assert_eq!(result["count"], 1, "runtime must expose the imported block");
    let sample = result["blocks"][0].clone();
    assert_eq!(sample["document_id"], document);
    assert_eq!(sample["block_id"], block);
    sample
}

fn source_valuation_count(rdf: &str, user: &str, graph: &str) -> usize {
    use oxigraph::io::{RdfFormat, RdfParser};
    use oxigraph::model::{GraphName, NamedOrBlankNode};

    let root = format!("urn:mnemosyne:user:{user}:graph:{graph}");
    let prefix = format!("{root}:valuation:");
    let mut subjects = BTreeSet::new();
    // Inventory subjects independently of translation, including missing blockRef.
    for quad in RdfParser::from_format(RdfFormat::NQuads).for_slice(rdf.as_bytes()) {
        let quad = quad.expect("valid source RDF for independent inventory");
        if !matches!(&quad.graph_name, GraphName::NamedNode(name) if name.as_str() == root) {
            continue;
        }
        if let NamedOrBlankNode::NamedNode(subject) = quad.subject {
            if subject.as_str().starts_with(&prefix) {
                subjects.insert(subject.as_str().to_owned());
            }
        }
    }
    subjects.len()
}

fn append_legacy_rdf(bytes: &[u8], rdf: &str) -> Vec<u8> {
    let mut archive = tar::Archive::new(GzDecoder::new(bytes));
    let mut entries = Vec::new();
    for entry in archive.entries().expect("tar entries") {
        let mut entry = entry.expect("tar entry");
        let path = entry.path().expect("entry path").into_owned();
        let mut data = Vec::new();
        entry.read_to_end(&mut data).expect("entry bytes");
        entries.push((path, entry.header().clone(), data));
    }
    let manifest: Value = serde_json::from_slice(
        &entries
            .iter()
            .find(|(path, _, _)| path == std::path::Path::new("manifest.json"))
            .expect("manifest")
            .2,
    )
    .unwrap();
    let rdf_path = std::path::Path::new(manifest["files"]["rdf"].as_str().unwrap());
    let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
    let mut found_rdf = false;
    for (path, mut header, mut data) in entries {
        // This fixture must exercise RDF translation, never native-store copying.
        if path.starts_with("values") {
            continue;
        }
        if path == rdf_path {
            found_rdf = true;
            data.extend_from_slice(b"\n");
            data.extend_from_slice(rdf.as_bytes());
        }
        header.set_size(data.len() as u64);
        header.set_cksum();
        builder
            .append_data(&mut header, &path, data.as_slice())
            .expect("rewrite entry");
    }
    assert!(found_rdf);
    builder.into_inner().unwrap().finish().unwrap()
}

#[test]
fn legacy_rdf_import_reaches_shared_store_runtime_and_verifier() {
    let _serial = crate::tauri_runtime::profile_env_serial()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let _profile = IsolatedProfile::new();
    let app = crate::tauri_runtime::build_mock_app_for_tests(true);
    let source = "legacy-values-source";
    let target = "legacy-values-target";
    let document = "legacy-values-document";
    create_graph_service(
        &app,
        CreateGraphInput {
            graph_id: Some(source.into()),
            title: "Legacy fixture source".into(),
            description: None,
            operation_id: None,
        },
    )
    .expect("create isolated source");
    crate::app_runtime::async_runtime::block_on(enqueue_crdt_operation(
        app.clone(),
        EnqueueCrdtOperationInput {
            kind: "document.write".into(),
            graph_id: source.into(),
            document_id: Some(document.into()),
            payload: json!({"documentId": document, "content": "A valued paragraph.",
                "format": "markdown", "title": "Legacy fixture"}),
        },
    ))
    .expect("write actual CRDT document");
    let record =
        crate::document_service::read_document(app.clone(), source.into(), document.into())
            .unwrap();
    let block = crate::document_projection_service::document_blocks_for_read(&record)
        .first()
        .expect("real projected block")
        .id
        .clone();
    let built = crate::graph_archive_export::build_graph_archive(&app, source, true).unwrap();
    let native = std::fs::read(&built.archive_path).unwrap();
    let parsed = parse_graph_archive(&native).unwrap();
    assert_eq!(parsed.manifest.source_user_id, "default");
    assert!(crate::legacy_valuation_import::parse_legacy_valuations(
        &parsed.rdf_n_quads,
        "default",
        source,
    )
    .unwrap()
    .is_empty());
    let source_dir = crate::paths::existing_graph_dir(&app, source).unwrap();
    assert!(read_value_store(&source_dir, source)
        .unwrap()
        .blocks
        .is_empty());

    let root = format!("urn:mnemosyne:user:default:graph:{source}");
    let subject = format!("{root}:valuation:{document}:{block}");
    let ns = "http://mnemosyne.dev/doc#";
    let mut rdf =
        format!("<{subject}> <{ns}blockRef> <{root}:doc:{document}#block-{block}> <{root}> .\n");
    for (field, value) in [
        ("rawImportanceSum", "3"),
        ("importanceCount", "2"),
        ("cumulativeImportance", "2"),
        ("rawValenceSum", "-3"),
        ("valenceCount", "3"),
        ("cumulativeValence", "-2"),
        ("userImportance", "0"),
        ("userValence", "-1"),
        ("lastValuatedAt", "2026-08-25T15:45:55.123456+00:00"),
    ] {
        rdf.push_str(&format!(
            "<{subject}> <{ns}{field}> \"{value}\" <{root}> .\n"
        ));
    }
    let bytes = append_legacy_rdf(&native, &rdf);
    let parsed = parse_graph_archive(&bytes).unwrap();
    let plan = crate::legacy_valuation_import::plan_legacy_valuations(
        &parsed.rdf_n_quads,
        "default",
        source,
    )
    .expect("plan complete fixture valuation");
    assert!(plan.quarantine.is_empty());
    assert_eq!(plan.records.len(), 1);
    assert!(!parsed
        .originals
        .iter()
        .any(|(path, _)| path.starts_with("values/")));
    import(&app, &bytes, target);
    let graph_dir = crate::paths::existing_graph_dir(&app, target).unwrap();
    let store = read_value_store(&graph_dir, target).unwrap();
    assert!(store.observer.is_empty());
    assert_eq!(store.blocks.len(), 1);
    let key = block_value_key(document, &block);
    let expected = json!({
        "documentId": document, "blockId": block, "rawImportanceSum": 3.0,
        "importanceCount": 2, "cumulativeImportance": 2.0, "rawValenceSum": -3.0,
        "valenceCount": 3, "cumulativeValence": -2.0, "valuationCount": 5,
        "userImportance": 0.0, "userValence": -1.0, "tags": [],
        "lastValuatedAt": "2026-08-25T15:45:55.123456+00:00",
    });
    assert_eq!(serde_json::to_value(&store.blocks[&key]).unwrap(), expected);
    let disk: Value =
        serde_json::from_slice(&std::fs::read(graph_dir.join("values/block-values.json")).unwrap())
            .unwrap();
    assert_eq!(disk["blocks"][&key], expected);
    let sample = runtime_sample(&app, target, document, &block);
    for (field, value) in expected.as_object().unwrap() {
        assert_eq!(&sample[field], value, "runtime field {field}");
    }
    let report = crate::graph_fidelity_report::build_fidelity_report(&app, target, &bytes, None)
        .expect("small fixture fidelity report");
    assert_eq!(report["summary"]["documents_exact"], 1);
    assert!(
        !report["exceptions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|exception| exception["kind"] == "valuation_translation_unverified"),
        "fixture valuation must be independently verified"
    );

    crate::salience_mcp_valuation::mcp_local_value(
        app.clone(),
        &json!({
            "graph_id": target,
            "document_id": document,
            "block_id": block,
            "importance": 1,
        }),
    )
    .expect("native commons valuation after legacy import");
    let updated_store = read_value_store(&graph_dir, target).unwrap();
    assert!(updated_store.observer.is_empty());
    let updated = &updated_store.blocks[&key];
    assert_eq!(updated.raw_importance_sum, 4.0);
    assert_eq!(updated.importance_count, 3);
    assert_eq!(updated.raw_valence_sum, -3.0);
    assert_eq!(updated.valence_count, 3);
    assert_eq!(updated.user_importance, Some(0.0));
    assert_eq!(updated.user_valence, Some(-1.0));
}

#[test]
#[ignore = "opt-in isolated corpus import; requires GARDEN_MIGRATION_ARCHIVE"]
fn legacy_archive_corpus_import_summary() {
    let archive_path = std::env::var_os("GARDEN_MIGRATION_ARCHIVE")
        .expect("set GARDEN_MIGRATION_ARCHIVE to a local archive");
    let report_path = std::env::var_os("GARDEN_MIGRATION_REPORT");
    let _serial = crate::tauri_runtime::profile_env_serial()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let _profile = IsolatedProfile::new();
    let app = crate::tauri_runtime::build_mock_app_for_tests(true);
    let bytes = std::fs::read(archive_path).expect("read corpus archive");
    let parsed = parse_graph_archive(&bytes).expect("parse corpus archive");
    let plan = crate::legacy_valuation_import::plan_legacy_valuations(
        &parsed.rdf_n_quads,
        &parsed.manifest.source_user_id,
        &parsed.manifest.source_graph_id,
    )
    .expect("plan legacy corpus valuations");
    let legacy = &plan.records;
    let source_count = source_valuation_count(
        &parsed.rdf_n_quads,
        &parsed.manifest.source_user_id,
        &parsed.manifest.source_graph_id,
    );
    assert_eq!(
        source_count,
        legacy.len() + plan.quarantine.len(),
        "every source valuation subject must be installed or quarantined"
    );
    assert!(!legacy.is_empty(), "corpus must contain legacy valuations");
    let target = "legacy-corpus-isolated";
    import(&app, &bytes, target);
    let graph_dir = crate::paths::existing_graph_dir(&app, target).unwrap();
    let store = read_value_store(&graph_dir, target).unwrap();
    let disk: Value =
        serde_json::from_slice(&std::fs::read(graph_dir.join("values/block-values.json")).unwrap())
            .unwrap();
    let raw_matches = legacy
        .iter()
        .filter(|(key, record)| {
            disk["blocks"].get(*key) == Some(&serde_json::to_value(record).unwrap())
        })
        .count();
    let shared_matches = legacy
        .iter()
        .filter(|(key, record)| {
            store
                .blocks
                .get(*key)
                .map(|value| serde_json::to_value(value).unwrap())
                == Some(serde_json::to_value(record).unwrap())
        })
        .count();
    let quarantine_disk: Value = serde_json::from_slice(
        &std::fs::read(graph_dir.join("values/legacy-valuation-quarantine.json"))
            .expect("read persisted legacy valuation quarantine"),
    )
    .expect("parse persisted legacy valuation quarantine");
    let quarantine_matches =
        quarantine_disk["records"] == serde_json::to_value(&plan.quarantine).unwrap();
    let sample_record = legacy
        .values()
        .find(|record| {
            parsed
                .documents
                .iter()
                .any(|(id, _)| id == &record.document_id)
        })
        .expect("legacy valuation attached to an archived document");
    let sample = runtime_sample(
        &app,
        target,
        &sample_record.document_id,
        &sample_record.block_id,
    );
    let expected = serde_json::to_value(sample_record).unwrap();
    let runtime_matches = expected.as_object().unwrap().iter().all(|(field, value)| {
        if matches!(
            field.as_str(),
            "rawImportanceSum" | "rawValenceSum" | "cumulativeImportance" | "cumulativeValence"
        ) {
            return sample[field].as_f64()
                == Some(crate::salience_value_store::round4(value.as_f64().unwrap()));
        }
        // The API renders an absent valuation timestamp as null.
        (field == "lastValuatedAt" && value == "" && sample[field].is_null())
            || &sample[field] == value
    });
    let summary = json!({
        "source_document_count": parsed.documents.len(),
        "legacy_count": legacy.len(), "installed": store.blocks.len(),
        "source_valuation_count": source_count, "quarantined": plan.quarantine.len(),
        "raw_record_matches": raw_matches, "shared_record_matches": shared_matches,
        "runtime_sample_matches": runtime_matches,
        "quarantine_matches": quarantine_matches,
    });
    // Summary only: never persist source content, block identifiers, or raw records.
    if let Some(path) = report_path {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .expect("create summary without overwriting any existing path");
        serde_json::to_writer_pretty(&mut file, &summary).unwrap();
        writeln!(file).unwrap();
    }
    assert_eq!(
        legacy.len(),
        store.blocks.len(),
        "legacy/install count mismatch"
    );
    assert_eq!(raw_matches, legacy.len(), "raw stored record mismatch");
    assert_eq!(
        shared_matches,
        legacy.len(),
        "shared reader record mismatch"
    );
    assert!(runtime_matches, "runtime sample differs from legacy fields");
    assert!(
        quarantine_matches,
        "persisted quarantine differs from source plan"
    );
}
