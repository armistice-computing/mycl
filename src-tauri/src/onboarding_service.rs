use crate::app_runtime::AppHandle;
use crate::{
    app_error::{AppError, AppResult},
    clock::timestamp,
    graph_service::{create_graph_service, CreateGraphInput},
    onboarding_archive::{self, ARCHIVE_NAME},
    paths::graphs_dir,
    storage::{read_json, write_json},
};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// The first-run graph. Its title is "Start"; a profile seeded before v4
/// keeps its graph under the old id `onboarding`.
const DEFAULT_ONBOARDING_GRAPH_ID: &str = "start";
const SEEDED_MARKER_FILE: &str = "seeded.json";
/// Profile-level record that the first-run decision was made, so a person who
/// deletes the onboarding graph never gets it back, and a profile that
/// already had graphs is never seeded.
const PROFILE_ONBOARDING_MARKER: &str = "onboarding-seeded.json";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SeedOnboardingInput {
    #[serde(default, alias = "graph_id")]
    pub graph_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SeedReport {
    pub graph_id: String,
    pub document_ids: Vec<String>,
    pub archive_name: String,
    pub archive_version: u32,
    pub already_seeded: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SeededMarker {
    archive_name: String,
    archive_version: u32,
    seeded_at: String,
    document_ids: Vec<String>,
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn seed_onboarding_graph(
    app: AppHandle,
    input: SeedOnboardingInput,
) -> Result<SeedReport, String> {
    seed_onboarding_graph_service(&app, input).map_err(AppError::message)
}

pub(crate) fn seed_onboarding_graph_service(
    app: &AppHandle,
    input: SeedOnboardingInput,
) -> AppResult<SeedReport> {
    let graph_id = input
        .graph_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_ONBOARDING_GRAPH_ID)
        .to_string();

    let graph_dir = graphs_dir(app).map_err(AppError::storage)?.join(&graph_id);
    let seeded_marker_path = graph_dir.join(SEEDED_MARKER_FILE);

    // Idempotency guard #1: seeded.json present → already done, return cached report.
    if seeded_marker_path.is_file() {
        let marker: SeededMarker = read_json(&seeded_marker_path)?;
        return Ok(SeedReport {
            graph_id,
            document_ids: marker.document_ids,
            archive_name: marker.archive_name,
            archive_version: marker.archive_version,
            already_seeded: true,
        });
    }

    // Idempotency guard #2: graph.json present but no marker → another tool created
    // a graph at this id. Don't clobber it.
    if graph_dir.join("graph.json").is_file() {
        return Err(AppError::conflict(format!(
            "graph already exists at id={graph_id} without an onboarding marker; \
             refusing to overwrite"
        )));
    }

    let archive = onboarding_archive::extract()?;

    let title = archive
        .manifest
        .source_graph_title
        .clone()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "Mycl Onboarding".to_string());
    let description = archive
        .manifest
        .source_graph_description
        .clone()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

    create_graph_service(
        app,
        CreateGraphInput {
            title,
            graph_id: Some(graph_id.clone()),
            description,
            operation_id: None,
        },
    )?;

    // The workspace and every document go in the way an archive import
    // writes them, under the graph's write lease, so the folders, the
    // catalog, the wires and each document's projections exist before the
    // first open. (The old frontend's warm pass used to do this; the
    // current one never calls the seeder.)
    let now = timestamp();
    let archive_manifest = crate::crdt_engine::import_archive_ops::GraphArchiveManifest {
        source_user_id: "default".to_string(),
        source_graph_id: archive.manifest.source_graph_id.clone(),
        source_graph_title: archive.manifest.source_graph_title.clone(),
        source_graph_description: archive.manifest.source_graph_description.clone(),
        includes_artifacts: archive.manifest.includes_artifacts,
        exported_at: None,
    };
    let document_ids = {
        let _write = crate::crdt_engine::persistence_coordinator::acquire_hot_write_blocking_if_managed(app, &graph_id)
            .map_err(AppError::storage)?;
        let (seeded, warnings) = crate::crdt_engine::import_archive_ops::seed_graph_contents(
            app,
            &graph_id,
            &archive_manifest,
            &archive.workspace_bytes,
            &archive.documents,
            "onboarding-seed",
        )
        .map_err(AppError::storage)?;
        for warning in warnings {
            log::warn!("onboarding seed: {warning}");
        }
        seeded
    };

    if let Some(values) = &archive.values {
        if let Err(error) = seed_values(&graph_dir, &graph_id, values, &now) {
            // The documents and wires are the onboarding; ratings are a
            // bonus, never a reason to fail the seed.
            log::warn!("onboarding graph seeded without its ratings: {error}");
        }
    }

    let marker = SeededMarker {
        archive_name: ARCHIVE_NAME.to_string(),
        archive_version: archive.manifest.version,
        seeded_at: timestamp(),
        document_ids: document_ids.clone(),
    };
    write_json(&seeded_marker_path, &marker)?;

    log::info!(
        "onboarding graph seeded: graph_id={} archive={} documents={} version={}",
        graph_id,
        ARCHIVE_NAME,
        document_ids.len(),
        archive.manifest.version,
    );

    Ok(SeedReport {
        graph_id,
        document_ids,
        archive_name: ARCHIVE_NAME.to_string(),
        archive_version: archive.manifest.version,
        already_seeded: false,
    })
}

/// Write the archive's ratings into the new graph, re-stamped to now so they
/// arrive fresh instead of already faded by the months since they were set,
/// then project them into the graph's RDF.
fn seed_values(graph_dir: &std::path::Path, graph_id: &str, bytes: &[u8], now: &str) -> Result<(), String> {
    let mut store: crate::salience_value_store::LocalValueStore =
        serde_json::from_slice(bytes).map_err(|error| format!("parse the archive's values: {error}"))?;
    store.graph_id = graph_id.to_string();
    store.observer = String::new();
    store.config_history.clear();
    for record in store.blocks.values_mut() {
        record.last_valuated_at = now.to_string();
        if record.user_rated_at.is_some() {
            record.user_rated_at = Some(now.to_string());
        }
        record.user_renewed_from = None;
    }
    crate::salience_value_store::write_value_store(graph_dir, &store)?;
    let store = crate::salience_value_store::read_value_store(graph_dir, graph_id)?;
    if let Err(error) = crate::salience_rdf_materializer::reconcile_value_store(graph_dir, &store) {
        log::warn!("onboarding ratings written but not yet projected to RDF: {error}");
    }
    Ok(())
}

/// First run: give a brand-new profile its onboarding graph. Decided once
/// per profile; a profile that already has any graph is never seeded.
pub(crate) fn seed_fresh_profile(app: &AppHandle) -> AppResult<Option<SeedReport>> {
    let graphs = graphs_dir(app).map_err(AppError::storage)?;
    let marker_path = graphs
        .parent()
        .ok_or_else(|| AppError::storage("graphs dir has no profile"))?
        .join(PROFILE_ONBOARDING_MARKER);
    if marker_path.is_file() {
        return Ok(None);
    }
    let has_graphs = std::fs::read_dir(&graphs)
        .map(|entries| entries.flatten().any(|entry| entry.path().join("graph.json").is_file()))
        .unwrap_or(false);
    let report = if has_graphs {
        None
    } else {
        Some(seed_onboarding_graph_service(app, SeedOnboardingInput { graph_id: None })?)
    };
    write_json(
        &marker_path,
        &json!({ "decidedAt": timestamp(), "seeded": report.is_some(), "archive": ARCHIVE_NAME }),
    )?;
    Ok(report)
}

#[cfg(all(test, feature = "headless"))]
mod first_run_tests {
    use super::*;
    use crate::app_runtime::async_runtime::block_on;
    use std::path::PathBuf;

    struct IsolatedProfile {
        path: PathBuf,
        previous: Option<std::ffi::OsString>,
    }

    impl IsolatedProfile {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("garden-first-run-{}", uuid::Uuid::new_v4()));
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

    fn isolated(test: impl FnOnce(&AppHandle)) {
        let _serial = crate::tauri_runtime::profile_env_serial().lock().unwrap_or_else(|p| p.into_inner());
        let _profile = IsolatedProfile::new();
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);
        test(&app);
    }

    #[test]
    fn a_new_profile_gets_the_onboarding_graph_once() {
        isolated(|app| {
            let report = seed_fresh_profile(app).unwrap().expect("a new profile is seeded");
            assert_eq!(report.graph_id, "start");
            assert_eq!(report.document_ids.len(), 12, "{:?}", report.document_ids);

            let (graph_dir, graph) = crate::graph_record_store::read_graph_record_no_heal(app, "start").unwrap();
            assert_eq!(graph.title, "Start");

            // The sidebar and the agents read it through the ordinary paths.
            let workspace = block_on(crate::workspace_projection_service::mcp_local_get_workspace(
                app.clone(),
                &json!({"graphId": "start", "depth": 0}),
            ))
            .unwrap();
            let titles: Vec<&str> = workspace["documents"].as_array().unwrap().iter()
                .filter_map(|d| d["title"].as_str()).collect();
            assert!(titles.contains(&"Start Here"), "{titles:?}");
            assert!(workspace["counts"]["wires"].as_u64().unwrap_or(0) > 0, "the wires came with it: {workspace}");
            // Every wire of every page stays inside the seeded graph, or
            // the atlas of a new profile has nothing to draw.
            let mut wired_pages = 0;
            for id in &report.document_ids {
                let wires = crate::mcp_wire_read_service::mcp_local_get_wires(
                    app.clone(),
                    &json!({"graphId": "start", "documentId": id}),
                )
                .unwrap();
                let rows = wires["wires"].as_array().unwrap();
                if !rows.is_empty() {
                    wired_pages += 1;
                }
                assert!(
                    rows.iter().all(|wire| wire["targetGraphId"].as_str() == Some("start")
                        && wire["sourceGraphId"].as_str().is_none_or(|graph| graph == "start")),
                    "seeded wires must stay in the destination graph: {wires}"
                );
            }
            assert!(wired_pages >= 6, "only {wired_pages} pages have wires");
            assert!(workspace["folders"].as_array().is_some_and(|f| !f.is_empty()), "the folders came with it");
            assert_ne!(workspace["workspace"]["materialization"], "flat-document-fallback");
            let start = crate::document_mcp_service::mcp_local_read_document(
                app.clone(),
                &json!({"graphId": "start", "documentId": "start-here", "format": "markdown"}),
            )
            .unwrap();
            assert_eq!(start["title"], "Start Here");
            assert!(start["content"].as_str().unwrap().len() > 400, "{start}");

            // The ratings came too, fresh.
            let store = crate::salience_value_store::read_value_store(&graph_dir, "start").unwrap();
            assert!(store.blocks.len() >= 20, "{} rated blocks", store.blocks.len());
            let now = crate::clock::epoch_millis() as i64;
            for record in store.blocks.values() {
                let at: i64 = record.last_valuated_at.parse().unwrap();
                assert!(now - at < 60_000, "a rating arrived stale: {}", record.last_valuated_at);
            }

            // Decided once: a second start, or a deleted graph, never re-seeds.
            assert!(seed_fresh_profile(app).unwrap().is_none());
            crate::graph_service::soft_delete_graph_service(app, "start".into(), true).unwrap();
            assert!(seed_fresh_profile(app).unwrap().is_none());
        });
    }

    #[test]
    fn a_profile_that_already_has_graphs_is_never_seeded() {
        isolated(|app| {
            crate::graph_service::create_graph_service(app, crate::graph_service::CreateGraphInput {
                graph_id: Some("mine".into()),
                title: "Mine".into(),
                description: None,
                operation_id: None,
            })
            .unwrap();
            assert!(seed_fresh_profile(app).unwrap().is_none());
            assert!(crate::graph_record_store::read_graph_record_no_heal(app, "start").is_err());
        });
    }
}
