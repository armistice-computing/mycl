//! Native owner UI adapter. Plans never cross IPC; job truth lives in the journal.
use crate::{
    app_runtime::AppHandle,
    vault_job::{Record, State, Store},
    vault_job_controller::{self, Review},
};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
#[cfg(feature = "desktop")]
use tauri::Manager;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Error {
    pub code: &'static str,
    pub message: String,
}
impl Error {
    fn new(code: &'static str, message: &str) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
    fn unavailable() -> Self {
        Self::new(
            "unavailable",
            "Import state is unavailable. Check its status before starting another import.",
        )
    }
    fn backend(error: String) -> Self {
        log::warn!("Vault UI operation refused: {error}");
        if error.contains("expired") {
            Self::new(
                "expired",
                "Source permission expired. Select the source again.",
            )
        } else if error.contains("revoked") {
            Self::new(
                "revoked",
                "Source permission was revoked. Select the source again.",
            )
        } else if error.contains("incarnation") || error.contains("lifetime changed") {
            Self::new(
                "stale_source",
                "The source graph changed. Create a new preview.",
            )
        } else if error.contains("budget")
            || error.contains("exceeds")
            || error.contains("too many")
        {
            Self::new(
                "budget",
                "This import exceeds a safety limit. Select a smaller source.",
            )
        } else {
            Self::unavailable()
        }
    }
}
type Result<T> = std::result::Result<T, Error>;
#[cfg(all(test, feature = "headless", not(feature = "desktop")))]
#[path = "vault_ui_tests.rs"]
mod tests;
struct Held {
    id: String,
    review: Arc<Review>,
    expires: u64,
    until: Instant,
}
#[derive(Clone)]
struct Worker {
    review_id: String,
    digest: String,
    title: String,
    cancel: Arc<AtomicBool>,
    active: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
}
#[derive(Default)]
struct Inner {
    review: Option<Held>,
    jobs: BTreeMap<String, Worker>,
    order: VecDeque<String>,
    selections: BTreeMap<String, (crate::vault_capture::Binding, Instant)>,
    selection_order: VecDeque<String>,
}
pub(crate) struct Runtime {
    inner: Mutex<Inner>,
    operation: Arc<tokio::sync::Semaphore>,
    pub(crate) picker: Arc<PickerLatch>,
}
impl Default for Runtime {
    fn default() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            operation: Arc::new(tokio::sync::Semaphore::new(1)),
            picker: Arc::new(PickerLatch::default()),
        }
    }
}
impl Runtime {
    pub(crate) fn expire(&self) -> std::result::Result<(), String> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "preview registry unavailable")?;
        if inner
            .review
            .as_ref()
            .is_some_and(|h| Instant::now() >= h.until)
        {
            inner.review = None;
        }
        inner
            .selections
            .retain(|_, (_, until)| Instant::now() < *until);
        let live: std::collections::BTreeSet<_> = inner.selections.keys().cloned().collect();
        inner.selection_order.retain(|id| live.contains(id));
        Ok(())
    }
    /// Called only after a real native chooser selection, never from IPC arguments.
    pub(crate) fn record_chooser_selection(
        &self,
        binding: crate::vault_capture::Binding,
        scan: String,
    ) -> std::result::Result<(), String> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "selection registry unavailable")?;
        inner.selection_order.retain(|id| id != &scan);
        inner.selection_order.push_back(scan.clone());
        inner
            .selections
            .insert(scan, (binding, Instant::now() + Duration::from_secs(3600)));
        while inner.selection_order.len() > 32 {
            if let Some(old) = inner.selection_order.pop_front() {
                inner.selections.remove(&old);
            }
        }
        Ok(())
    }
    fn permit(&self) -> Result<tokio::sync::OwnedSemaphorePermit> {
        self.operation.clone().try_acquire_owned().map_err(|_| {
            Error::new(
                "busy",
                "An import operation is already running. Wait for it to finish.",
            )
        })
    }
    fn worker(&self, id: &str) -> Result<Option<Worker>> {
        Ok(self
            .inner
            .lock()
            .map_err(|_| Error::unavailable())?
            .jobs
            .get(id)
            .cloned())
    }
}
fn runtime(app: &AppHandle) -> Arc<Runtime> {
    app.state::<Arc<Runtime>>().inner().clone()
}
fn now() -> Result<u64> {
    crate::vault_job_bridge::now().map_err(Error::backend)
}

pub(crate) async fn review(
    app: AppHandle,
    graph: String,
    scan: String,
    title: String,
) -> Result<Value> {
    let ui = runtime(&app);
    let _permit = ui.permit()?;
    let expected = ui
        .inner
        .lock()
        .map_err(|_| Error::unavailable())?
        .selections
        .get(&scan)
        .filter(|(_, until)| Instant::now() < *until)
        .map(|(binding, _)| binding.clone())
        .ok_or_else(|| {
            Error::new(
                "unavailable",
                "Choose the source in this app session before creating a preview.",
            )
        })?;
    if expected.graph != graph
        || crate::vault_selection::owner_binding(&app, &graph).map_err(Error::backend)? != expected
    {
        return Err(Error::new(
            "stale_source",
            "The selected source graph changed. Choose the source again.",
        ));
    }
    // Starting a new preview retires the old capability, freeing its retained bytes.
    ui.inner.lock().map_err(|_| Error::unavailable())?.review = None;
    let review = vault_job_controller::review(app, graph, scan, title)
        .await
        .map_err(Error::backend)?;
    let time = now()?;
    let expires = review.expires().min(time.saturating_add(300_000));
    if expires <= time {
        return Err(Error::new(
            "expired",
            "Source permission expired while preparing the preview.",
        ));
    }
    let id = uuid::Uuid::new_v4().to_string();
    let dto = review.preview(&id, expires).map_err(Error::backend)?;
    ui.inner.lock().map_err(|_| Error::unavailable())?.review = Some(Held {
        id,
        review,
        expires,
        until: Instant::now() + Duration::from_millis(expires - time),
    });
    Ok(dto)
}
fn authorize(app: &AppHandle, record: &Record) -> Result<()> {
    if record.identity.source.principal != "human:owner" {
        return Err(Error::unavailable());
    }
    app.state::<Arc<crate::cell_graph_boundary::CellGraphBoundary>>()
        .authorize_graph_id(&record.identity.destination_graph)
        .map_err(|_| Error::unavailable())
}
fn dto(record: &Record, worker: Option<&Worker>) -> Value {
    let active = worker.is_some_and(|w| w.active.load(Ordering::Acquire));
    let phase = match record.state {
        State::Accepted => "accepted",
        State::Building { .. } => "building",
        State::Publishing { .. } => "publishing",
        State::Published => "published",
        State::Uncertain { .. } => "uncertain",
        State::Abandoned { .. } => "abandoned",
    };
    let error=worker.and_then(|w|w.error.lock().ok().and_then(|e|e.clone())).or_else(|| {
        if !active&&matches!(record.state,State::Accepted|State::Building{..}) {Some("This import is not running. Its unfinished stage can be discarded; it will not restart automatically.".into())}
        else if matches!(record.state,State::Uncertain{..}) {Some("Publication needs reconciliation. Check again before starting another import.".into())}
        else {None}
    });
    json!({"jobId":record.identity.job_id,"phase":phase,"destinationGraphId":record.identity.destination_graph,"title":worker.map(|w|w.title.as_str()).unwrap_or("Imported graph"),"error":error,"workerActive":active,"cancelRequested":worker.is_some_and(|w|w.cancel.load(Ordering::Acquire))})
}
async fn record(app: AppHandle, id: String, active: bool) -> Result<Record> {
    let result = if active {
        let worker_app = app.clone();
        crate::app_runtime::async_runtime::spawn_blocking(move || {
            Store::open(&crate::profile_paths::profile_dir(&worker_app)?)?.read(&id)
        })
        .await
        .map_err(|_| Error::unavailable())?
        .map_err(Error::backend)?
    } else {
        vault_job_controller::status(app.clone(), id)
            .await
            .map_err(Error::backend)?
    };
    authorize(&app, &result)?;
    Ok(result)
}
pub(crate) async fn status(app: AppHandle, id: String) -> Result<Value> {
    let worker = runtime(&app).worker(&id)?;
    let record = record(
        app.clone(),
        id,
        worker
            .as_ref()
            .is_some_and(|w| w.active.load(Ordering::Acquire)),
    )
    .await?;
    response(app, record, worker).await
}
async fn response(app: AppHandle, record: Record, worker: Option<Worker>) -> Result<Value> {
    crate::app_runtime::async_runtime::spawn_blocking(move|| {
        let mut value=dto(&record,worker.as_ref());let mut available=false;
        if record.state==State::Published {
            let profile=crate::profile_paths::profile_dir(&app).map_err(Error::backend)?;
            let graph=vault_job_controller::read_destination(&profile.join("graphs").join(&record.identity.destination_graph)).map_err(Error::backend)?;
            if let Some(graph)=graph.filter(|g|g.status==crate::runtime_config::GRAPH_STATUS_ACTIVE && g.graph_id==record.identity.destination_graph && g.incarnation_id.as_deref()==Some(record.identity.destination_incarnation.as_str()) && g.created_by_operation_id.as_deref()==Some(record.identity.operation_id.as_str())) {
                available=true;value["title"]=json!(graph.title);
                // A current canonical witness supersedes a lost acknowledgement.
                value["error"]=Value::Null;
                if let Some(worker)=&worker {if let Ok(mut error)=worker.error.lock(){*error=None;}}
            } else {value["error"]=json!("This import was published, but its graph is no longer available or has been replaced.");}
        }
        value["destinationAvailable"]=json!(available);Ok(value)
    }).await.map_err(|_|Error::unavailable())?
}
pub(crate) fn release(app: &AppHandle, id: &str) -> Result<Value> {
    let ui = runtime(app);
    let mut inner = ui.inner.lock().map_err(|_| Error::unavailable())?;
    let released = inner.review.as_ref().is_some_and(|h| h.id == id);
    if released {
        inner.review = None;
    }
    Ok(json!({"released":released}))
}
struct Running(Arc<AtomicBool>);
impl Drop for Running {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

pub(crate) async fn commit(app: AppHandle, id: String, digest: String) -> Result<Value> {
    let ui = runtime(&app);
    let previous = {
        let inner = ui.inner.lock().map_err(|_| Error::unavailable())?;
        inner
            .jobs
            .iter()
            .find(|(_, w)| w.review_id == id)
            .map(|(job, w)| (job.clone(), w.digest.clone()))
    };
    if let Some((job, expected)) = previous {
        if expected != digest {
            return Err(Error::new(
                "stale_source",
                "The confirmation does not match the reviewed import.",
            ));
        }
        return status(app, job).await;
    }
    let permit = ui.permit()?;
    let (review, job, worker) = {
        let mut inner = ui.inner.lock().map_err(|_| Error::unavailable())?;
        let held = inner
            .review
            .as_ref()
            .filter(|h| h.id == id)
            .ok_or_else(|| {
                Error::new(
                    "unavailable",
                    "That preview is no longer available. Create a new preview.",
                )
            })?;
        if held.review.digest() != digest {
            return Err(Error::new(
                "stale_source",
                "The confirmation does not match the reviewed import.",
            ));
        }
        if now()? >= held.expires || Instant::now() >= held.until {
            inner.review = None;
            return Err(Error::new(
                "expired",
                "The preview expired. Create a new preview.",
            ));
        }
        let held = inner.review.take().unwrap();
        let job = held.review.job_id().map_err(Error::backend)?;
        let worker = Worker {
            review_id: id,
            digest: digest.clone(),
            title: held.review.title().into(),
            cancel: Arc::new(AtomicBool::new(false)),
            active: Arc::new(AtomicBool::new(true)),
            error: Arc::new(Mutex::new(None)),
        };
        // normalize_title caps at 512 Unicode scalars (at most 2048 bytes).
        // Reserve another 512 bytes for bounded error text and bookkeeping.
        if job.len() + worker.review_id.len() + worker.digest.len() + worker.title.len() + 512
            > 128 * 1024
        {
            return Err(Error::new(
                "budget",
                "Import metadata exceeds the history limit.",
            ));
        }
        inner.order.retain(|j| j != &job);
        inner.order.push_back(job.clone());
        inner.jobs.insert(job.clone(), worker.clone());
        while inner.order.len() > 32
            || inner
                .jobs
                .iter()
                .map(|(id, w)| id.len() + w.review_id.len() + w.digest.len() + w.title.len() + 512)
                .sum::<usize>()
                > 128 * 1024
        {
            let oldest = inner.order.iter().position(|id| {
                inner
                    .jobs
                    .get(id)
                    .is_none_or(|w| !w.active.load(Ordering::Acquire))
            });
            if let Some(old) = oldest.and_then(|n| inner.order.remove(n)) {
                inner.jobs.remove(&old);
            } else {
                inner.jobs.remove(&job);
                inner.order.retain(|id| id != &job);
                return Err(Error::new(
                    "busy",
                    "Import history is occupied by active work.",
                ));
            }
        }
        (held.review, job, worker)
    };
    let (send, receive) = tokio::sync::oneshot::channel::<Result<Record>>();
    let send = Arc::new(Mutex::new(Some(send)));
    let notify = send.clone();
    let worker_app = app.clone();
    let admission = Arc::new(AtomicBool::new(false));
    let notified = admission.clone();
    let pending = PendingJob {
        runtime: ui.clone(),
        job: job.clone(),
        active: worker.active.clone(),
        admitted: admission,
    };
    crate::app_runtime::async_runtime::spawn(async move {
        let _permit = permit;
        let _running = Running(worker.active.clone());
        let _pending = pending;
        let result = vault_job_controller::commit_notified(
            worker_app,
            review,
            digest,
            worker.cancel.clone(),
            Some(Box::new(move |record| {
                // This callback is after durable admission, not merely task spawning.
                notified.store(true, Ordering::Release);
                if let Ok(mut slot) = notify.lock() {
                    if let Some(send) = slot.take() {
                        let _ = send.send(Ok(record.clone()));
                    }
                }
            })),
        )
        .await
        .map_err(Error::backend);
        if let Err(error) = &result {
            if let Ok(mut slot) = worker.error.lock() {
                *slot = Some(error.message.clone());
            }
        }
        // Retire a failed pre-admission handle before the caller can retry it.
        drop(_pending);
        drop(_running);
        if let Ok(mut slot) = send.lock() {
            if let Some(send) = slot.take() {
                let _ = send.send(result);
            }
        }
    });
    receive.await.map_err(|_| Error::unavailable())??;
    status(app, job).await
}

pub(crate) async fn cancel(app: AppHandle, id: String) -> Result<Value> {
    let ui = runtime(&app);
    if let Some(worker) = ui.worker(&id)? {
        if worker.active.load(Ordering::Acquire) {
            let current = record(app.clone(), id.clone(), true).await?;
            if matches!(
                current.state,
                State::Publishing { .. } | State::Published | State::Uncertain { .. }
            ) {
                let mut value = response(app, current, Some(worker)).await?;
                value["error"] =
                    json!("Publication has started or finished; cancellation cannot undo it.");
                return Ok(value);
            }
            worker.cancel.store(true, Ordering::Release);
            return status(app, id).await;
        }
    }
    let _permit = ui.permit()?;
    let current = record(app.clone(), id.clone(), false).await?;
    if matches!(
        current.state,
        State::Accepted | State::Building { .. } | State::Abandoned { .. }
    ) {
        let report = crate::vault_job_recovery::discard_interrupted(app.clone(), id.clone())
            .await
            .map_err(Error::backend)?;
        let mut result = response(app, report.job, ui.worker(&id)?).await?;
        if report.reason.is_some() {
            result["error"] = if result["phase"]=="abandoned" {
                json!("The import stopped, but temporary data was retained for recovery.")
            } else {json!("The import could not be discarded safely. Its data was retained for recovery.")};
        }
        return Ok(result);
    }
    let mut result = response(app, current, ui.worker(&id)?).await?;
    result["error"] = json!("Publication has started or finished; cancellation cannot undo it.");
    Ok(result)
}

pub(crate) fn require_window(label: &str) -> Result<()> {
    if label != "main" {
        return Err(Error::new(
            "unsupported",
            "Import controls are available only in the main app window.",
        ));
    }
    Ok(())
}
pub(crate) async fn recent(app: AppHandle, limit: usize) -> Result<Value> {
    if !(1..=50).contains(&limit) {
        return Err(Error::new(
            "unsupported",
            "History limit must be 1 through 50.",
        ));
    }
    app.state::<Arc<crate::vault_runtime::VaultRuntime>>()
        .store()
        .map_err(Error::backend)?;
    let worker_app = app.clone();
    let (records, total) = crate::app_runtime::async_runtime::spawn_blocking(move || {
        Store::open(&crate::profile_paths::profile_dir(&worker_app)?)?.recent_owner(limit)
    })
    .await
    .map_err(|_| Error::unavailable())?
    .map_err(Error::backend)?;
    let mut rows = vec![];
    for record in records {
        if authorize(&app, &record).is_err() {
            continue;
        }
        // Listing never reconciles/starts a job; explicit status does reconciliation.
        let worker = runtime(&app).worker(&record.identity.job_id)?;
        rows.push(response(app.clone(), record, worker).await?);
    }
    Ok(json!({"jobs":rows,"complete":total<=limit}))
}

#[derive(Default)]
pub(crate) struct PickerLatch {
    active: std::sync::atomic::AtomicU64,
    sequence: std::sync::atomic::AtomicU64,
}
pub(crate) struct PickerGuard {
    latch: Arc<PickerLatch>,
    ticket: u64,
}
impl Drop for PickerGuard {
    fn drop(&mut self) {
        let _ =
            self.latch
                .active
                .compare_exchange(self.ticket, 0, Ordering::AcqRel, Ordering::Acquire);
    }
}
impl PickerLatch {
    pub(crate) fn enter(
        self: &Arc<Self>,
    ) -> std::result::Result<(PickerGuard, PickerGuard), String> {
        let ticket = self
            .sequence
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| "chooser counter exhausted")?
            + 1;
        self.active
            .compare_exchange(0, ticket, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| "a source chooser is already open")?;
        Ok((
            PickerGuard {
                latch: self.clone(),
                ticket,
            },
            PickerGuard {
                latch: self.clone(),
                ticket,
            },
        ))
    }
}
struct PendingJob {
    runtime: Arc<Runtime>,
    job: String,
    active: Arc<AtomicBool>,
    admitted: Arc<AtomicBool>,
}
impl Drop for PendingJob {
    fn drop(&mut self) {
        if self.admitted.load(Ordering::Acquire) {
            return;
        }
        if let Ok(mut inner) = self.runtime.inner.lock() {
            if inner
                .jobs
                .get(&self.job)
                .is_some_and(|w| Arc::ptr_eq(&w.active, &self.active))
            {
                inner.jobs.remove(&self.job);
                inner.order.retain(|id| id != &self.job);
            }
        }
    }
}
