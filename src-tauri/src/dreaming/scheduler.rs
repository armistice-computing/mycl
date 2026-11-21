//! Nightly dreaming: gardend's first recurring job (plan Phase 3).
//!
//! A ticker checks every few minutes. A graph dreams when all hold:
//! - its nightly consent is on (graph_settings, off by default);
//! - a dreamer is chosen;
//! - no cycle for it, scheduled or by hand, started in the last 20 hours;
//! - the local time is inside the night window (1–4 am, cloud-1's), or the
//!   graph asked to catch up, consent was on for some part of the last
//!   window, and no cycle has started since that window began.
//!
//! Missed nights are never replayed: catching up runs one cycle, not one per
//! missed night. Catch-up is off by default (Eschaton, 2026-10-07: a
//! person's choice, defaulting to the window only). The cycle repeats these
//! checks after it claims the graph, and its consent gate reads consent
//! again before every send and before publication.
//!
//! Automatic dreaming is off in headless cells: they exit when idle, and the
//! app is the house's node. Nothing technical stops a cell from dreaming.

use super::graph_settings::{self, NightlyConsent};
use crate::app_runtime::AppHandle;
use crate::dreamer::{ConsentGate, DreamerConfig};
use chrono::{DateTime, Local, TimeZone, Timelike};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{atomic::AtomicBool, Arc};
use std::time::Duration;

/// Local hours [start, end) when a scheduled cycle may start.
pub(crate) const NIGHT_WINDOW_HOURS: (u32, u32) = (1, 4);
/// No two cycles for one graph within this many hours.
pub(crate) const COOLDOWN_HOURS: i64 = 20;
const HOUR_MS: i64 = 3_600_000;
const TICK: Duration = Duration::from_secs(600);

/// `GARDEN_DREAM_TICK_SECONDS` overrides the tick, for testing a night by hand.
fn tick() -> Duration {
    std::env::var("GARDEN_DREAM_TICK_SECONDS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .map(Duration::from_secs)
        .unwrap_or(TICK)
}

/// When this graph's last cycle started and how it ended. A cycle reserves
/// its start here before it reads the graph and before any send, and stops
/// if it cannot; so a crash or a failing dreamer still counts toward the
/// cooldown and is not retried every tick.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CycleLog {
    #[serde(default)]
    pub(crate) last_started_ms: Option<i64>,
    #[serde(default)]
    pub(crate) last_trigger: String,
    #[serde(default)]
    pub(crate) last_status: String,
    #[serde(default)]
    pub(crate) last_job_id: Option<String>,
}

pub(crate) fn cycle_log_path(graph_dir: &Path) -> PathBuf {
    graph_dir.join("dreaming").join("schedule.json")
}

/// The log, or the default when the file is absent. A file that exists but
/// can't be read or parsed is an error: without it the cooldown is unknown,
/// and an unknown cooldown must not permit a cycle.
pub(crate) fn read_cycle_log(graph_dir: &Path) -> Result<CycleLog, String> {
    let path = cycle_log_path(graph_dir);
    match std::fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(CycleLog::default()),
        Err(error) => return Err(format!("the dream schedule record can't be read: {error}")),
        Ok(meta) if !meta.is_file() => return Err("the dream schedule record is not a file".into()),
        Ok(_) => {}
    }
    let text = std::fs::read_to_string(&path).map_err(|e| format!("the dream schedule record can't be read: {e}"))?;
    serde_json::from_str(&text).map_err(|e| format!("the dream schedule record is malformed: {e}"))
}

pub(crate) fn write_cycle_log(graph_dir: &Path, log: &CycleLog) -> Result<(), String> {
    let path = cycle_log_path(graph_dir);
    crate::storage::create_dir_all(path.parent().ok_or("schedule path")?).map_err(|e| e.to_string())?;
    crate::storage::write_json(&path, log).map_err(|e| e.to_string())
}

/// The most recent night window that began at or before `now`, as epoch ms
/// [start, end). Both ends are local wall-clock times resolved through the
/// time zone, so the window is four hours on the night clocks fall back and
/// two on the night they spring forward.
fn last_window_ms(now: DateTime<Local>) -> Option<(i64, i64)> {
    let (start, end) = NIGHT_WINDOW_HOURS;
    let day = if now.hour() >= start { now.date_naive() } else { now.date_naive().pred_opt()? };
    let local = |hour: u32| -> Option<i64> {
        let at = day.and_hms_opt(hour, 0, 0)?;
        // A wall-clock time skipped by a spring-forward resolves to the
        // first valid instant after it.
        let resolved = Local.from_local_datetime(&at).earliest().or_else(|| {
            Local.from_local_datetime(&(at + chrono::Duration::hours(1))).earliest()
        })?;
        Some(resolved.timestamp_millis())
    };
    Some((local(start)?, local(end)?))
}

/// Why a graph does not dream now, or `None` when it is due.
pub(crate) fn not_due(
    now: DateTime<Local>,
    nightly: Option<&NightlyConsent>,
    catch_up: bool,
    last_started_ms: Option<i64>,
) -> Option<&'static str> {
    let Some(consent) = nightly.filter(|n| n.enabled) else { return Some("nightly-off") };
    let now_ms = now.timestamp_millis();
    if last_started_ms.is_some_and(|last| now_ms - last < COOLDOWN_HOURS * HOUR_MS) {
        return Some("cooldown");
    }
    let (start, end) = NIGHT_WINDOW_HOURS;
    if (start..end).contains(&now.hour()) {
        return None;
    }
    if !catch_up {
        return Some("outside-window");
    }
    // Catch up once for a window that passed with consent on for some part
    // of it and no cycle since it began.
    let Some(consented_ms) = super::temporal::parse_ms_str(&consent.changed_at) else { return Some("consent-time-unknown") };
    let Some((window_ms, window_end_ms)) = last_window_ms(now) else { return Some("outside-window") };
    if consented_ms >= window_end_ms || last_started_ms.is_some_and(|last| last >= window_ms) {
        return Some("nothing-missed");
    }
    None
}

/// The gate a scheduled cycle carries: the graph's consent, read from disk
/// before each send and before publication.
pub(crate) fn nightly_consent_gate(graph_dir: PathBuf, graph_id: String) -> ConsentGate {
    Arc::new(move || {
        let (settings, error) = graph_settings::read_reported(&graph_dir);
        if let Some(error) = error {
            return Err(format!("the dream settings for {graph_id} can't be read: {error}"));
        }
        match settings.nightly.filter(|n| n.enabled) {
            Some(consent) => Ok(format!("nightly dreaming on for {graph_id} since {}", consent.changed_at)),
            None => Err(format!("nightly dreaming is off for {graph_id}")),
        }
    })
}

/// Whether a scheduled cycle may start now for this graph: the checks the
/// ticker makes, repeated by the cycle once it holds the graph's claim.
pub(crate) fn scheduled_admission(graph_dir: &Path) -> Result<Option<&'static str>, String> {
    let (settings, error) = graph_settings::read_reported(graph_dir);
    if let Some(error) = error {
        return Err(format!("the dream settings can't be read: {error}"));
    }
    let log = read_cycle_log(graph_dir)?;
    Ok(not_due(Local::now(), settings.nightly.as_ref(), settings.catch_up(), log.last_started_ms))
}

pub(crate) fn start_dream_scheduler(app: AppHandle) {
    crate::app_runtime::async_runtime::spawn(async move {
        let mut ticker = tokio::time::interval(tick());
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // The first tick fires at once; let the app finish starting.
        ticker.tick().await;
        log::info!("dream scheduler armed: window {:?} local, cooldown {COOLDOWN_HOURS}h", NIGHT_WINDOW_HOURS);
        loop {
            ticker.tick().await;
            if crate::cell_lifecycle::process_lifecycle().is_some_and(|l| l.is_draining()) {
                log::info!("dream scheduler stopped for cell drain");
                return;
            }
            run_due(&app).await;
        }
    });
}

/// How often the sweep reads each graph's workspace even when its marker
/// says nothing is due: wires the marker can't know about (a duplicated or
/// restored graph, a crash before the marker, a raw CRDT write) still go.
const PERIODIC_SWEEP: Duration = Duration::from_secs(6 * 3600);

fn periodic_sweep_due(graph_id: &str) -> bool {
    static LAST: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>> =
        std::sync::OnceLock::new();
    let mut last = LAST.get_or_init(Default::default).lock().unwrap_or_else(|p| p.into_inner());
    let due = last.get(graph_id).is_none_or(|at| at.elapsed() >= PERIODIC_SWEEP);
    if due {
        last.insert(graph_id.to_string(), std::time::Instant::now());
    }
    due
}

async fn sweep_dream_wires(app: &AppHandle, graph_id: &str, graph_dir: &Path, now_ms: i64) {
    let Ok(_write) = super::cycle::graph_write_lease(app, graph_id).await else { return };
    match super::dream_wires::sweep_expired(app, graph_id, graph_dir, now_ms).await {
        Ok(0) => {}
        Ok(removed) => log::info!("dream scheduler: {graph_id}: deleted {removed} expired dream wires"),
        Err(error) => log::warn!("dream scheduler: {graph_id}: expired dream wires not deleted: {error}"),
    }
}

/// One pass: dream every graph that is due, one at a time.
async fn run_due(app: &AppHandle) {
    let graphs = match crate::graph_service::list_graphs(app.clone()) {
        Ok(graphs) => graphs,
        Err(error) => {
            log::warn!("dream scheduler: cannot list graphs: {error}");
            return;
        }
    };
    // Sweeps first, for every graph, so no cycle (which waits on the
    // dreamer) and no early stop can hold one back. Expired dreamtOf wires
    // leave the graph whether or not it dreams; consent doesn't gate
    // deleting what a dream wrote.
    let now_ms = chrono::Utc::now().timestamp_millis();
    for graph in &graphs {
        let Ok(graph_dir) = crate::paths::existing_graph_dir(app, &graph.graph_id) else { continue };
        if super::dream_wires::marker_due(&graph_dir, now_ms) || periodic_sweep_due(&graph.graph_id) {
            sweep_dream_wires(app, &graph.graph_id, &graph_dir, now_ms).await;
        }
    }
    for graph in graphs {
        let Ok(graph_dir) = crate::paths::existing_graph_dir(app, &graph.graph_id) else { continue };
        match scheduled_admission(&graph_dir) {
            Ok(None) => {}
            Ok(Some(_)) => continue,
            Err(error) => {
                log::warn!("dream scheduler: {} skipped: {error}", graph.graph_id);
                continue;
            }
        }
        match crate::dreamer::read_dreamer_config(app) {
            Ok(DreamerConfig::None) | Err(_) => {
                log::info!("dream scheduler: {} is due but no dreamer is chosen", graph.graph_id);
                return;
            }
            Ok(_) => {}
        }
        let consent = nightly_consent_gate(graph_dir.clone(), graph.graph_id.clone());
        let cancel = Arc::new(AtomicBool::new(false));
        match super::cycle::dream_now(app.clone(), graph.graph_id.clone(), false, consent, cancel, super::cycle::SCHEDULED).await {
            Ok(receipt) => log::info!(
                "dream scheduler: {} {}",
                graph.graph_id,
                receipt.get("status").and_then(|s| s.as_str()).unwrap_or("?")
            ),
            Err(error) => log::warn!("dream scheduler: {} failed: {error}", graph.graph_id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(day: u32, hour: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 10, day, hour, 30, 0).single().unwrap()
    }

    fn on_since(when: DateTime<Local>) -> NightlyConsent {
        NightlyConsent { enabled: true, changed_at: when.timestamp_millis().to_string() }
    }

    #[test]
    fn a_graph_dreams_once_a_night_inside_the_window_and_only_with_consent() {
        let consent = on_since(at(1, 12));
        let on = Some(&consent);
        assert_eq!(not_due(at(8, 2), None, false, None), Some("nightly-off"));
        let off = NightlyConsent { enabled: false, changed_at: "1".into() };
        assert_eq!(not_due(at(8, 2), Some(&off), false, None), Some("nightly-off"));
        assert_eq!(not_due(at(8, 0), on, false, None), Some("outside-window"));
        assert_eq!(not_due(at(8, 4), on, false, None), Some("outside-window"));
        assert_eq!(not_due(at(8, 1), on, false, None), None);
        assert_eq!(not_due(at(8, 3), on, false, None), None);
        // Started at 1:30 tonight: not again at 3:30.
        let tonight = at(8, 1).timestamp_millis();
        assert_eq!(not_due(at(8, 3), on, false, Some(tonight)), Some("cooldown"));
        // Started at 1:30 last night: due tonight.
        assert_eq!(not_due(at(8, 1), on, false, Some(at(7, 1).timestamp_millis())), None);
        // A hand-run cycle at 11 pm keeps tonight's window closed (20 h).
        let evening = Local.with_ymd_and_hms(2026, 10, 7, 23, 0, 0).single().unwrap().timestamp_millis();
        assert_eq!(not_due(at(8, 2), on, false, Some(evening)), Some("cooldown"));
    }

    #[test]
    fn catching_up_runs_once_for_a_missed_window_and_never_by_default() {
        let consent = on_since(at(1, 12));
        let on = Some(&consent);
        let last_night = Some(at(7, 1).timestamp_millis());
        // Asleep through tonight's window (last cycle the night before).
        assert_eq!(not_due(at(8, 9), on, false, last_night), Some("outside-window"));
        assert_eq!(not_due(at(8, 9), on, true, last_night), None);
        // Caught up at 9:30: not again at 15:30, nor after the cooldown ends
        // before the next window.
        let caught_up = Some(at(8, 9).timestamp_millis());
        assert_eq!(not_due(at(8, 15), on, true, caught_up), Some("cooldown"));
        assert_eq!(not_due(Local.with_ymd_and_hms(2026, 10, 9, 0, 45, 0).single().unwrap(), on, true, Some(at(8, 4).timestamp_millis())), Some("nothing-missed"));
        // Dreamed inside tonight's window: nothing to catch up at noon.
        assert_eq!(not_due(at(8, 12), on, true, Some(at(8, 1).timestamp_millis() - 21 * HOUR_MS)), None);
        // Consent turned on after tonight's window closed: nothing was missed.
        let late = on_since(at(8, 8));
        assert_eq!(not_due(at(8, 9), Some(&late), true, None), Some("nothing-missed"));
        // Turned on at 2:00, asleep before the next tick, awake at 9:00:
        // part of the window was eligible, so it catches up.
        let mid = Local.with_ymd_and_hms(2026, 10, 8, 2, 0, 0).single().unwrap();
        assert_eq!(not_due(at(8, 9), Some(&on_since(mid)), true, None), None);
    }

    #[test]
    fn the_window_follows_the_wall_clock_across_a_daylight_saving_change() {
        // Where this machine's zone falls back on 2026-11-01 (America/
        // Los_Angeles, the house's), 1–4 am lasts four hours: consent at
        // 3:30 that morning is inside the window, not after it.
        let day = chrono::NaiveDate::from_ymd_opt(2026, 11, 1).unwrap();
        let at = |h: u32, m: u32| Local.from_local_datetime(&day.and_hms_opt(h, m, 0).unwrap()).latest().unwrap();
        let (start, end) = last_window_ms(at(9, 0)).unwrap();
        let hours = (end - start) / HOUR_MS;
        if hours == 3 {
            return; // this zone has no transition that night
        }
        assert_eq!(hours, 4);
        let consent = NightlyConsent { enabled: true, changed_at: at(3, 30).timestamp_millis().to_string() };
        assert_eq!(not_due(at(9, 0), Some(&consent), true, None), None);
    }

    #[test]
    fn an_unreadable_schedule_record_refuses_rather_than_assuming_no_prior_run() {
        let dir = std::env::temp_dir().join(format!("dream-log-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("dreaming")).unwrap();
        assert_eq!(read_cycle_log(&dir).unwrap(), CycleLog::default(), "absent is a fresh graph");
        std::fs::write(cycle_log_path(&dir), "{not json").unwrap();
        assert!(read_cycle_log(&dir).is_err());
        std::fs::remove_file(cycle_log_path(&dir)).unwrap();
        std::fs::create_dir(cycle_log_path(&dir)).unwrap();
        assert!(read_cycle_log(&dir).is_err());
        assert!(write_cycle_log(&dir, &CycleLog::default()).is_err(), "a reservation that can't be written fails");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_gate_reads_consent_at_each_send() {
        let dir = std::env::temp_dir().join(format!("dream-gate-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let gate = nightly_consent_gate(dir.clone(), "g".into());
        assert!(gate().is_err(), "off by default");
        graph_settings::set_nightly(&dir, true).unwrap();
        assert!(gate().unwrap().starts_with("nightly dreaming on for g since "));
        graph_settings::set_nightly(&dir, false).unwrap();
        assert!(gate().is_err(), "turned off between sends");
        std::fs::write(graph_settings::settings_path(&dir), "{broken").unwrap();
        assert!(gate().is_err(), "unreadable settings refuse");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
