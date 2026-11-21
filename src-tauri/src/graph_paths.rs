use crate::app_runtime::AppHandle;
use crate::{
    graph_service::GraphRecord,
    ids::validate_local_id,
    profile_paths::graphs_dir,
    runtime_config::GRAPH_STATUS_DELETED,
    semantic_index_paths::semantic_index_dir,
    storage::{create_dir_all, read_json},
    ydoc_paths::workspace_ydoc_dir,
};
use std::path::{Path, PathBuf};
#[cfg(all(feature = "headless", not(feature = "desktop")))]
use std::sync::Arc;
#[cfg(all(feature = "headless", not(feature = "desktop")))]
use std::sync::{Mutex, OnceLock};
#[cfg(all(feature = "headless", not(feature = "desktop")))]
#[cfg(feature = "desktop")]
use tauri::Manager;

#[cfg(all(feature = "headless", not(feature = "desktop")))]
static GRAPH_SELF_HEAL_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[cfg(all(feature = "headless", not(feature = "desktop")))]
fn materialization_event_for_lifecycle(lifecycle: &str) -> &'static str {
    if lifecycle == "provisioning" {
        "graph.initialize"
    } else {
        "graph.repair"
    }
}

#[cfg(all(feature = "headless", not(feature = "desktop")))]
fn materialization_event() -> Result<
    (
        &'static str,
        crate::cell_registry_authority::CellRegistryBinding,
    ),
    String,
> {
    let binding = crate::cell_registry_authority::verified_binding()?
        .ok_or_else(|| "cell graph registry binding is unavailable".to_string())?;
    let event = materialization_event_for_lifecycle(&binding.lifecycle_state);
    Ok((event, binding))
}

#[cfg(all(feature = "headless", not(feature = "desktop")))]
fn authorize_cell_self_heal(app: &AppHandle, graph_id: &str) -> Result<(), String> {
    let boundary = app
        .try_state::<Arc<crate::cell_graph_boundary::CellGraphBoundary>>()
        .ok_or_else(|| "cell graph boundary is not initialized".to_string())?;
    boundary.authorize_graph_self_heal(graph_id)
}

/// Self-heal a missing `graph.json` (F4c). Compiled only into a pure headless
/// (cell) binary — `headless` without `desktop`, since the two are composable
/// cargo features and a desktop+headless build must never carry self-heal —
/// and only acts when the gateway has told us we're a gateway-fronted cell
/// (`GARDEN_SELF_HEAL_GRAPHS=1`) — see `runtime_config::self_heal_graphs_enabled()`.
/// No-op if the graph already exists (including a concurrent racer having
/// just created it).
///
/// Callers must check `!graph_path.is_file()` themselves before invoking this
/// — a soft-deleted graph (`GRAPH_STATUS_DELETED`) must never reach here, or
/// it would be silently resurrected. See `existing_graph_dir` /
/// `graph_record_store::read_graph_record`.
///
/// SECURITY: never call this from a surface reachable without gateway auth
/// (e.g. the anonymous query-signed image read/validation path) — a missing
/// graph there must 404, not self-heal. Use `existing_graph_dir_no_heal` for
/// those. See F4c security review finding 1.
#[cfg(all(feature = "headless", not(feature = "desktop")))]
pub(crate) fn self_heal_missing_graph(app: &AppHandle, graph_id: &str) -> Result<(), String> {
    if !crate::runtime_config::self_heal_graphs_enabled() {
        return Err(format!("graph not found: {graph_id}"));
    }
    // The gateway authorizes the accessor; the process-bound cell authority
    // constrains which namespace F4c may materialize. Requiring managed state
    // here also makes startup-order regressions fail closed instead of falling
    // back to the old caller-selected namespace.
    authorize_cell_self_heal(app, graph_id)?;
    let _guard = GRAPH_SELF_HEAL_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| "graph self-heal lock poisoned".to_string())?;

    // Re-check under the lock: a racing request may have already healed it.
    let graph_path = graphs_dir(app)?.join(graph_id).join("graph.json");
    if graph_path.is_file() {
        return Ok(());
    }

    log::warn!("F4c self-heal: creating missing graph store for {graph_id}");
    // Authenticated headless request roots acquire the graph persistence lease
    // before reaching this self-heal. Use the non-reentrant create body so a
    // missing graph cannot deadlock trying to reacquire its own lifecycle gate.
    match crate::graph_service::create_graph_service_inner(
        app,
        crate::graph_service::CreateGraphInput {
            title: graph_id.to_string(),
            graph_id: Some(graph_id.to_string()),
            description: Some("auto-created by cell self-heal (F4c)".to_string()),
            operation_id: None,
        },
    ) {
        Ok(_) => {
            let (event, binding) = materialization_event()?;
            log::info!(
                target: "audit",
                "event={event} owner={} graph={} generation={} registry_revision={}",
                binding.owner,
                binding.graph_id,
                binding.generation,
                binding.registry_revision,
            );
            Ok(())
        }
        // Lost the race to a concurrent legitimate creator (e.g. the
        // gateway's own async F4b create) — the record now exists for real,
        // with the real title. Don't clobber it; just proceed.
        Err(error) if error.kind() == crate::app_error::AppErrorKind::Conflict => Ok(()),
        Err(error) => Err(error.message()),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GraphDirOutcome {
    Existing,
    SelfHealed,
}

pub(crate) fn existing_graph_dir_reporting_heal(
    app: &AppHandle,
    graph_id: &str,
) -> Result<(PathBuf, GraphDirOutcome), String> {
    validate_local_id(graph_id, "graph_id")?;
    let graph_dir = graphs_dir(app)?.join(graph_id);
    let graph_path = graph_dir.join("graph.json");
    let outcome = if graph_path.is_file() {
        GraphDirOutcome::Existing
    } else {
        #[cfg(all(feature = "headless", not(feature = "desktop")))]
        {
            self_heal_missing_graph(app, graph_id)?;
            GraphDirOutcome::SelfHealed
        }
        #[cfg(any(not(feature = "headless"), feature = "desktop"))]
        {
            return Err(format!("graph not found: {graph_id}"));
        }
    };
    let graph = read_json::<GraphRecord>(&graph_path)?;
    if graph.status == GRAPH_STATUS_DELETED {
        return Err(format!("graph not found: {graph_id}")); // never self-healed
    }
    ensure_graph_content_dirs(&graph_dir)?;
    Ok((graph_dir, outcome))
}

pub(crate) fn existing_graph_dir(app: &AppHandle, graph_id: &str) -> Result<PathBuf, String> {
    existing_graph_dir_reporting_heal(app, graph_id).map(|(graph_dir, _)| graph_dir)
}

/// Read-only probe: does this graph's `graph.json` exist right now? The
/// room-open escalation uses this under its shared lease so the existence
/// check and the heal share one path construction instead of drifting apart.
pub(crate) fn graph_json_present(app: &AppHandle, graph_id: &str) -> Result<bool, String> {
    validate_local_id(graph_id, "graph_id")?;
    Ok(graphs_dir(app)?.join(graph_id).join("graph.json").is_file())
}

/// Non-healing counterpart to `existing_graph_dir` — the original
/// pre-self-heal lookup logic: a missing (or soft-deleted)
/// `graph.json` is always `Err`, never created. Use this for any surface
/// reachable WITHOUT gateway auth, where self-heal would let an attacker
/// materialize an arbitrary graph merely by hitting it with a guessed
/// `graph_id` — e.g. the anonymous query-signed image read/validation path
/// (`original_file_access_tokens::image_access_token_matches` and
/// `original_file_service::read_image_file`). A missing graph means no valid
/// image token (or anything else gated on the graph existing) can exist
/// anyway, so refusing to self-heal here changes no legitimate behavior.
/// See F4c security review finding 1.
pub(crate) fn existing_graph_dir_no_heal(
    app: &AppHandle,
    graph_id: &str,
) -> Result<PathBuf, String> {
    validate_local_id(graph_id, "graph_id")?;
    let graph_dir = graphs_dir(app)?.join(graph_id);
    let graph_path = graph_dir.join("graph.json");
    if !graph_path.is_file() {
        return Err(format!("graph not found: {graph_id}"));
    }
    let graph = read_json::<GraphRecord>(&graph_path)?;
    if graph.status == GRAPH_STATUS_DELETED {
        return Err(format!("graph not found: {graph_id}"));
    }
    ensure_graph_content_dirs(&graph_dir)?;
    Ok(graph_dir)
}

pub(crate) fn documents_dir(graph_dir: &Path) -> PathBuf {
    graph_dir.join("documents")
}

pub(crate) fn artifacts_dir(graph_dir: &Path) -> PathBuf {
    graph_dir.join("artifacts")
}

pub(crate) fn images_dir(graph_dir: &Path) -> PathBuf {
    graph_dir.join("images")
}

pub(crate) fn ensure_graph_content_dirs(graph_dir: &Path) -> Result<(), String> {
    create_dir_all(&documents_dir(graph_dir))?;
    create_dir_all(&artifacts_dir(graph_dir))?;
    create_dir_all(&graph_dir.join("ydocs/documents"))?;
    create_dir_all(&workspace_ydoc_dir(graph_dir))?;
    Ok(())
}

#[cfg(all(test, feature = "headless", not(feature = "desktop")))]
mod cell_self_heal_tests {
    use super::*;

    #[test]
    fn process_bound_authority_precedes_f4c_storage() {
        let app = crate::tauri_runtime::build_mock_app_for_tests(false);
        app.manage(Arc::new(
            crate::cell_graph_boundary::CellGraphBoundary::for_test(Some("graph-a")),
        ));

        assert!(authorize_cell_self_heal(&app, "graph-a").is_ok());
        assert_eq!(
            authorize_cell_self_heal(&app, "graph-b"),
            Err("graph not found: graph-b".to_string())
        );
        assert_eq!(
            materialization_event_for_lifecycle("provisioning"),
            "graph.initialize"
        );
        assert_eq!(
            materialization_event_for_lifecycle("active"),
            "graph.repair"
        );
        assert_eq!(
            materialization_event_for_lifecycle("repairing"),
            "graph.repair"
        );
    }
}

pub(crate) fn ensure_graph_index_dirs(graph_dir: &Path) -> Result<(), String> {
    create_dir_all(&graph_dir.join("indexes"))?;
    create_dir_all(&semantic_index_dir(graph_dir))?;
    Ok(())
}

pub(crate) fn ensure_graph_layout(graph_dir: &Path) -> Result<(), String> {
    ensure_graph_content_dirs(graph_dir)?;
    create_dir_all(&images_dir(graph_dir))?;
    ensure_graph_index_dirs(graph_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_graph_dir(name: &str) -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("mnemosyne-paths-{name}-{suffix}"))
    }

    #[test]
    fn ensure_graph_layout_creates_projection_and_index_directories() {
        let graph_dir = temp_graph_dir("layout");

        ensure_graph_layout(&graph_dir).expect("ensure graph layout");

        for relative in [
            "documents",
            "artifacts",
            "images",
            "ydocs/documents",
            "ydocs/workspace",
            "indexes",
            "indexes/semantic",
        ] {
            assert!(
                graph_dir.join(relative).is_dir(),
                "expected {relative} to exist"
            );
        }
        let _ = fs::remove_dir_all(graph_dir);
    }
}
