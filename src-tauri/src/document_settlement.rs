//! Document settlement service — the shared "this graph has gone quiet"
//! detector. Design of record: graph doc `settlement-chronicle-design-2026-08-26`.
//!
//! Every successful document persist (and delete) marks its graph dirty with
//! the current instant — one O(1), infallible hook at the persistence choke
//! point, so editor ops, MCP writes, imports, and duplications are covered by
//! construction. A slow background tick then declares a graph SETTLED once it
//! has been dirty and untouched for a quiet period (default 10 minutes,
//! Eschaton's call: semantic search serves what you don't remember exactly;
//! what you touched in the last ten minutes you find lexically).
//!
//! Consumer zero is the semantic-index drain: on settle, submit the existing
//! (already-incremental) refresh job, which reuses unchanged vectors by
//! content hash and reconciles deletions by rebuilding from live sources.
//! Deliberately NOT here, per the design doc: search-triggered embedding
//! (search latency is sacred) and any Redis-style dedupe protocol — the
//! single-process dirty-set makes requeue storms structurally impossible.
//!
//! Future consumers (chronicle update-event finalization, the subscription
//! service) attach at `run_settlement_tick` without changing the marking side.

use crate::app_runtime::AppHandle;
use crate::{
    local_job_types::LocalJobStatus,
    loopback_state::LoopbackState,
    semantic_index_refresh_jobs::{RefreshSemanticIndexInput, SemanticRefreshFlushBoundary},
    semantic_model_remote::remote_embeddings_endpoint,
    semantic_model_state::{
        read_semantic_model_config, read_semantic_model_setup_manifest,
        semantic_model_manifest_matches,
    },
};
use std::{
    collections::{BTreeSet, HashMap},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
#[cfg(feature = "desktop")]
use tauri::Manager;
use tokio::time::MissedTickBehavior;

const DEFAULT_QUIET_SECONDS: u64 = 600;
const DEFAULT_TICK_SECONDS: u64 = 60;
/// Consecutive-failure backoff: quiet * failures, capped here. A failing
/// drain (e.g. model weights unreachable) retries with growing patience
/// instead of hammering — the retry-cap lesson from cloud-1 (f90dfd68),
/// applied at drain granularity since the refresh itself is all-or-nothing.
const MAX_FAILURE_COOLDOWN: Duration = Duration::from_secs(6 * 60 * 60);

#[derive(Default)]
pub(crate) struct SettlementTracker {
    inner: Mutex<HashMap<String, GraphSettlement>>,
}

#[derive(Default)]
struct GraphSettlement {
    last_touch: Option<Instant>,
    dirty_documents: BTreeSet<String>,
    inflight_job: Option<String>,
    consecutive_failures: u32,
    cooldown_until: Option<Instant>,
}

impl SettlementTracker {
    fn mark(&self, graph_id: &str, document_id: &str) {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let entry = inner.entry(graph_id.to_string()).or_default();
        entry.last_touch = Some(Instant::now());
        entry.dirty_documents.insert(document_id.to_string());
    }
}

/// Infallible dirty-mark. Called from the persistence choke points; a missing
/// tracker (isolated unit-test apps that skip managed-state setup) is a no-op
/// rather than a panic — settlement is an accelerant, never a correctness
/// dependency of the write path.
pub(crate) fn mark_document_dirty(app: &AppHandle, graph_id: &str, document_id: &str) {
    if let Some(tracker) = app.try_state::<Arc<SettlementTracker>>() {
        tracker.inner().mark(graph_id, document_id);
    }
}

/// Process-global tracker handle for write paths that have no AppHandle in
/// scope (the Geist projection-document writer was the angel-found bypass).
/// Installed at managed-state setup; re-installation (test mock apps)
/// replaces the previous handle.
fn global_tracker() -> &'static std::sync::Mutex<Option<Arc<SettlementTracker>>> {
    static GLOBAL: std::sync::OnceLock<std::sync::Mutex<Option<Arc<SettlementTracker>>>> =
        std::sync::OnceLock::new();
    GLOBAL.get_or_init(|| std::sync::Mutex::new(None))
}

pub(crate) fn install_global_tracker(tracker: Arc<SettlementTracker>) {
    *global_tracker()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(tracker);
}

/// Same infallible mark, reachable without an AppHandle.
pub(crate) fn mark_document_dirty_global(graph_id: &str, document_id: &str) {
    if let Some(tracker) = global_tracker()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
    {
        tracker.mark(graph_id, document_id);
    }
}

fn env_seconds(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

pub(crate) fn settlement_quiet_duration() -> Duration {
    Duration::from_secs(env_seconds(
        "GARDEN_SETTLEMENT_QUIET_SECONDS",
        DEFAULT_QUIET_SECONDS,
    ))
}

fn settlement_tick_duration() -> Duration {
    Duration::from_secs(env_seconds(
        "GARDEN_SETTLEMENT_TICK_SECONDS",
        DEFAULT_TICK_SECONDS,
    ))
}

fn autoindex_enabled() -> bool {
    std::env::var("GARDEN_SEMANTIC_AUTOINDEX")
        .map(|value| {
            let value = value.trim();
            !(value == "0" || value.eq_ignore_ascii_case("false") || value.eq_ignore_ascii_case("off"))
        })
        .unwrap_or(true)
}

/// The drain only runs where semantic indexing is actually configured: a
/// remote embeddings pool, or a locally prepared model whose setup manifest
/// matches the active spec. This is what prevents the settlement service
/// from ever triggering a surprise multi-hundred-MB model download on a
/// profile that never opted into semantic search.
fn semantic_autoindex_eligible(app: &AppHandle) -> bool {
    if remote_embeddings_endpoint().is_some() {
        return true;
    }
    let Ok(config) = read_semantic_model_config(app) else {
        return false;
    };
    let Ok(spec) = crate::semantic_model_catalog::semantic_model_spec_by_id(&config.selected_model_id)
    else {
        return false;
    };
    match read_semantic_model_setup_manifest(app, &spec) {
        Ok(Some(manifest)) => semantic_model_manifest_matches(&spec, &manifest),
        _ => false,
    }
}

pub(crate) fn start_settlement_scheduler(app: AppHandle) {
    crate::app_runtime::async_runtime::spawn(async move {
        run_scheduler(app).await;
    });
}

async fn run_scheduler(app: AppHandle) {
    let lifecycle = app
        .state::<Arc<crate::cell_lifecycle::CellLifecycle>>()
        .inner()
        .clone();
    let quiet = settlement_quiet_duration();
    let tick = settlement_tick_duration();
    let mut ticker = tokio::time::interval(tick);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // First tick fires immediately — skip it so boot never drains.
    ticker.tick().await;
    log::info!(
        "settlement scheduler armed: quiet={}s, tick={}s, autoindex={}",
        quiet.as_secs(),
        tick.as_secs(),
        autoindex_enabled()
    );
    // Boot reconcile sweep: mark every graph dirty once so the ordinary
    // drain converges each index to reality one quiet-window after boot.
    // This is the durability story for the deliberately in-memory dirty set
    // (crash-only design: recovery path = normal path), and it also heals
    // drift from any write path that missed its mark — the refresh is
    // hash-incremental, so sweeping an already-clean graph embeds nothing.
    boot_reconcile_sweep(&app);
    loop {
        ticker.tick().await;
        let _maintenance = match lifecycle.begin_maintenance("settlement-tick") {
            Ok(lease) => lease,
            Err(_) => {
                log::info!("settlement scheduler stopped for cell drain");
                return;
            }
        };
        // spawn_blocking + JoinError handling (mirroring the time-travel
        // scheduler): the tick does blocking registry I/O, and a panic
        // anywhere in its call chain must not silently kill this loop for
        // the life of the process.
        let tick_app = app.clone();
        match crate::app_runtime::async_runtime::spawn_blocking(move || {
            run_settlement_tick(&tick_app, quiet)
        })
        .await
        {
            Ok(()) => {}
            Err(error) => {
                log::warn!("settlement tick task failed: {error}");
            }
        }
    }
}

fn boot_reconcile_sweep(app: &AppHandle) {
    let Some(tracker) = app.try_state::<Arc<SettlementTracker>>() else {
        return;
    };
    let tracker = tracker.inner().clone();
    match crate::graph_service::list_graphs(app.clone()) {
        Ok(graphs) => {
            let count = graphs.len();
            for graph in graphs {
                tracker.mark(&graph.graph_id, "@boot-reconcile");
            }
            if count > 0 {
                log::info!("settlement boot sweep marked {count} graph(s) for reconcile");
            }
        }
        Err(error) => {
            log::warn!("settlement boot sweep could not list graphs: {error}");
        }
    }
}

fn run_settlement_tick(app: &AppHandle, quiet: Duration) {
    if !autoindex_enabled() {
        return;
    }
    let Some(tracker) = app.try_state::<Arc<SettlementTracker>>() else {
        return;
    };
    let tracker = tracker.inner().clone();
    // Loopback (and its job registry) may not be up yet during boot.
    let Some(loopback) = app.try_state::<Arc<LoopbackState>>() else {
        return;
    };
    let loopback = loopback.inner().clone();

    let now = Instant::now();
    let due: Vec<String> = {
        let mut inner = tracker.inner.lock().unwrap_or_else(|p| p.into_inner());
        let mut due = Vec::new();
        for (graph_id, entry) in inner.iter_mut() {
            if entry.dirty_documents.is_empty() {
                continue;
            }
            let Some(last_touch) = entry.last_touch else {
                continue;
            };
            if now.duration_since(last_touch) < quiet {
                continue;
            }
            if let Some(cooldown_until) = entry.cooldown_until {
                if now < cooldown_until {
                    continue;
                }
            }
            // One drain in flight per graph, ever. When the previous job
            // reaches a terminal state, record success/failure and move on.
            if let Some(job_id) = entry.inflight_job.clone() {
                match loopback.jobs.get(&job_id) {
                    Ok(Some(record)) => match record.status {
                        LocalJobStatus::Queued | LocalJobStatus::Running => continue,
                        LocalJobStatus::Succeeded => {
                            entry.inflight_job = None;
                            entry.consecutive_failures = 0;
                            entry.cooldown_until = None;
                        }
                        LocalJobStatus::Failed | LocalJobStatus::Cancelled => {
                            entry.inflight_job = None;
                            entry.consecutive_failures =
                                entry.consecutive_failures.saturating_add(1);
                            let cooldown = quiet
                                .saturating_mul(entry.consecutive_failures)
                                .min(MAX_FAILURE_COOLDOWN);
                            entry.cooldown_until = Some(now + cooldown);
                            log::warn!(
                                "settlement drain for graph {graph_id} ended {:?}; backing off {}s (failure #{})",
                                record.status,
                                cooldown.as_secs(),
                                entry.consecutive_failures
                            );
                            continue;
                        }
                    },
                    Ok(None) => {
                        entry.inflight_job = None;
                    }
                    Err(error) => {
                        log::warn!("settlement drain job lookup failed for {graph_id}: {error}");
                        continue;
                    }
                }
            }
            due.push(graph_id.clone());
        }
        due
    };

    for graph_id in due {
        if !semantic_autoindex_eligible(app) {
            // Not configured for semantic indexing (no prepared model, no
            // remote pool). Keep the dirty set — if a model is prepared
            // later, the pending docs drain on the next settle check.
            log::debug!(
                "settlement: graph {graph_id} settled but semantic indexing is not configured; holding"
            );
            continue;
        }
        let dirty_count = {
            let inner = tracker.inner.lock().unwrap_or_else(|p| p.into_inner());
            inner
                .get(&graph_id)
                .map(|entry| entry.dirty_documents.len())
                .unwrap_or(0)
        };
        match crate::semantic_index_refresh_jobs::submit_semantic_index_refresh_job(
            app.clone(),
            loopback.jobs.clone(),
            RefreshSemanticIndexInput {
                enrich: false,
                graph_id: graph_id.clone(),
                flush_boundary: SemanticRefreshFlushBoundary::BestEffort,
            },
        ) {
            Ok(response) => {
                let mut inner = tracker.inner.lock().unwrap_or_else(|p| p.into_inner());
                if let Some(entry) = inner.get_mut(&graph_id) {
                    entry.inflight_job = Some(response.job_id().to_string());
                    // The submitted refresh reads live state, so everything
                    // dirty up to this instant is covered by this drain.
                    entry.dirty_documents.clear();
                }
                log::info!(
                    "settlement: graph {graph_id} settled ({dirty_count} dirty docs) — semantic refresh submitted"
                );
            }
            Err(error) => {
                let mut inner = tracker.inner.lock().unwrap_or_else(|p| p.into_inner());
                if let Some(entry) = inner.get_mut(&graph_id) {
                    entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
                    let cooldown = quiet
                        .saturating_mul(entry.consecutive_failures)
                        .min(MAX_FAILURE_COOLDOWN);
                    entry.cooldown_until = Some(Instant::now() + cooldown);
                }
                log::warn!("settlement: semantic refresh submission failed for {graph_id}: {error}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marking_tracks_dirty_documents_and_touch_time() {
        let tracker = SettlementTracker::default();
        tracker.mark("g1", "doc-a");
        tracker.mark("g1", "doc-b");
        tracker.mark("g1", "doc-a");
        tracker.mark("g2", "doc-c");

        let inner = tracker.inner.lock().unwrap();
        let g1 = inner.get("g1").expect("g1 tracked");
        assert_eq!(g1.dirty_documents.len(), 2);
        assert!(g1.last_touch.is_some());
        let g2 = inner.get("g2").expect("g2 tracked");
        assert_eq!(g2.dirty_documents.len(), 1);
    }

    #[test]
    fn quiet_and_tick_env_overrides_parse_with_defaults() {
        // No env set in tests: defaults apply.
        assert_eq!(settlement_quiet_duration(), Duration::from_secs(600));
        assert_eq!(settlement_tick_duration(), Duration::from_secs(60));
        assert!(autoindex_enabled());
    }
}
