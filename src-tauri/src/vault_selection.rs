//! Native-owner capture service. Paths enter only from the native picker, never
//! from an HTTP/MCP or webview-supplied argument. Capture makes no graph entities.
use crate::{
    app_runtime::AppHandle,
    vault_capture::{Binding, SourceKind, State},
    vault_runtime::VaultRuntime,
};
use serde_json::{json, Value};
use std::{fs::File, path::Path, sync::Arc};
#[cfg(feature = "desktop")]
use tauri::Manager;

const OWNER: &str = "human:owner";
const TTL_MS: u64 = 60 * 60 * 1000;

#[cfg(all(test, feature = "headless", not(feature = "desktop")))]
#[path = "vault_selection_tests.rs"]
mod tests;

#[cfg(test)]
static CAPTURE_PAUSE: std::sync::Mutex<
    Option<(
        String,
        std::sync::mpsc::Sender<()>,
        std::sync::mpsc::Receiver<()>,
    )>,
> = std::sync::Mutex::new(None);

#[cfg(test)]
fn pause_capture(graph: &str) {
    let pause = {
        let mut slot = CAPTURE_PAUSE.lock().unwrap();
        if slot.as_ref().is_some_and(|p| p.0 == graph) {
            slot.take()
        } else {
            None
        }
    };
    if let Some((_, started, resume)) = pause {
        started.send(()).unwrap();
        resume
            .recv_timeout(std::time::Duration::from_secs(20))
            .unwrap();
    }
}

pub(crate) fn kind(value: &str) -> Result<SourceKind, String> {
    match value {
        "folder" => Ok(SourceKind::Folder),
        "zip" => Ok(SourceKind::Zip),
        _ => Err("source kind must be folder or zip".into()),
    }
}

pub(crate) fn owner_binding(app: &AppHandle, graph: &str) -> Result<Binding, String> {
    app.state::<Arc<crate::cell_graph_boundary::CellGraphBoundary>>()
        .authorize_graph_id(graph)
        .map_err(|_| "graph not available")?;
    let (_, record) = crate::graph_record_store::read_graph_record_no_heal(app, graph)
        .map_err(|e| e.message())?;
    if record.status != "active" {
        return Err("graph is not active".into());
    }
    Ok(Binding {
        principal: OWNER.into(),
        graph: graph.into(),
        incarnation: record.incarnation_id.ok_or("graph has no incarnation")?,
    })
}

/// The chosen root may not contain the owner's profile or sit within it.
/// The source itself is trusted only as a selection; staging rejects hostile
/// descendants and keeps reading through this descriptor rather than its name.
pub(crate) fn open_selected_source(
    path: &Path,
    profile: &Path,
    kind: SourceKind,
) -> Result<File, String> {
    open_selected_source_with_hook(path, profile, kind, || {})
}

fn open_selected_source_with_hook(
    path: &Path,
    profile: &Path,
    kind: SourceKind,
    before_open: impl FnOnce(),
) -> Result<File, String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let profile = std::fs::canonicalize(profile).map_err(|e| e.to_string())?;
        let private = std::fs::metadata(&profile).map_err(|e| e.to_string())?;
        let selected = std::fs::canonicalize(path).map_err(|e| e.to_string())?;
        let expected = std::fs::symlink_metadata(&selected).map_err(|e| e.to_string())?;
        if selected.starts_with(&profile) || profile.starts_with(&selected) {
            return Err(
                "choose a source outside the app profile, not a folder containing it".into(),
            );
        }
        before_open();
        let file = open_selected_chain(&selected, (private.dev(), private.ino()))?;
        let meta = file.metadata().map_err(|e| e.to_string())?;
        if matches!(kind, SourceKind::Zip) && meta.nlink() != 1 {
            return Err("selected ZIP must be an independent file, not a hard link".into());
        }
        if meta.dev() != expected.dev() || meta.ino() != expected.ino() {
            return Err("selected source changed before opening".into());
        }
        if !match kind {
            SourceKind::Folder => meta.is_dir(),
            SourceKind::Zip => meta.is_file(),
        } {
            return Err("selected source kind mismatch".into());
        }
        // Re-resolve after opening: never bless a descriptor reached through
        // a changed selection or newly redirected ancestor.
        let current = std::fs::canonicalize(path).map_err(|e| e.to_string())?;
        let named = std::fs::metadata(&current).map_err(|e| e.to_string())?;
        if current != selected
            || meta.dev() != named.dev()
            || meta.ino() != named.ino()
            || current.starts_with(&profile)
            || profile.starts_with(&current)
        {
            return Err("selected source changed while opening".into());
        }
        Ok(file)
    }
    #[cfg(not(unix))]
    {
        let _ = (path, profile, kind, before_open);
        Err("native vault selection is not supported on this platform".into())
    }
}

#[cfg(unix)]
fn open_selected_chain(path: &Path, private: (u64, u64)) -> Result<File, String> {
    use std::{
        ffi::CString,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::{ffi::OsStrExt, fs::MetadataExt},
        },
    };
    let mut parent = File::open("/").map_err(|e| e.to_string())?;
    let components: Vec<_> = path
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect();
    for (index, name) in components.iter().enumerate() {
        let name = CString::new(name.as_bytes()).map_err(|_| "invalid source component")?;
        let mut expected = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: parent is live, name is NUL-terminated, and stat is read only
        // after success. Each new descriptor immediately transfers to File.
        if unsafe {
            libc::fstatat(
                parent.as_raw_fd(),
                name.as_ptr(),
                expected.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let expected = unsafe { expected.assume_init() };
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | libc::O_CLOEXEC
            | if index + 1 < components.len() {
                libc::O_DIRECTORY
            } else {
                0
            };
        let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let child = unsafe { File::from_raw_fd(fd) };
        let actual = child.metadata().map_err(|e| e.to_string())?;
        if (actual.dev(), actual.ino()) == private {
            return Err("selected source passes through the owner's private profile".into());
        }
        if actual.dev() != expected.st_dev as u64 || actual.ino() != expected.st_ino as u64 {
            return Err("source component changed while opening".into());
        }
        parent = child;
    }
    Ok(parent)
}

/// The caller already selected and opened the capability. A worker owns the
/// activity lease and ticket even when the webview stops awaiting its response.
pub(crate) async fn start_capture(
    app: AppHandle,
    expected: Binding,
    source: File,
    kind: SourceKind,
) -> Result<Value, String> {
    let runtime = app.state::<Arc<VaultRuntime>>().inner().clone();
    let store = runtime.store()?;
    let lifecycle = app
        .state::<Arc<crate::cell_lifecycle::CellLifecycle>>()
        .inner()
        .clone();
    let activity = lifecycle
        .begin_background("vault-capture")
        .map_err(|_| "app is draining")?;
    let (send, receive) = tokio::sync::oneshot::channel();
    crate::app_runtime::async_runtime::spawn_blocking(move || {
        let _activity = activity;
        if send.is_closed() {
            return;
        }
        // The potentially busy capture mutex and filesystem work belong on
        // this worker, never on a Tokio scheduler thread or the AppKit thread.
        let started = (|| {
            let coordinator = app.state::<crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator>();
            let _lease = coordinator.acquire_lifecycle_shared_blocking(&expected.graph)?;
            if owner_binding(&app, &expected.graph)? != expected {
                return Err("graph changed during selection; select again".into());
            }
            let mut capture = store.lock()?;
            let grant = capture.grant_selected(expected.clone(), source, kind, TTL_MS)?;
            match capture.begin(&grant, &expected) {
                Ok(ticket) => Ok(ticket),
                Err(error) => {
                    let _ = capture.revoke(&grant);
                    Err(error)
                }
            }
        })();
        let mut ticket = match started {
            Ok(ticket) => ticket,
            Err(error) => {
                let _ = send.send(Err(error));
                return;
            }
        };
        let scan = ticket.id().to_owned();
        let result =
            json!({"scanId":scan,"graphId":expected.graph,"state":"capturing","graphWrites":false});
        if send.send(Ok(result)).is_err() {
            let _ = store
                .lock()
                .and_then(|mut capture| capture.cancel(&scan, &expected));
            return;
        }
        #[cfg(test)]
        pause_capture(&expected.graph);
        let snapshot = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| ticket.capture()))
            .unwrap_or_else(|_| Err("capture worker panicked".into()));
        // Reading sources need not pin the graph. Publication rechecks its
        // incarnation; deletion can revoke consent and cancel a long capture.
        let coordinator =
            app.state::<crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator>();
        let lease = coordinator.acquire_lifecycle_shared_blocking(&expected.graph);
        let snapshot = match &lease {
            Ok(_) if owner_binding(&app, &expected.graph).as_ref() == Ok(&expected) => snapshot,
            _ => Err("graph changed during capture".into()),
        };
        if let Err(error) = store
            .lock()
            .and_then(|mut capture| capture.finish(ticket, &expected, snapshot))
        {
            log::warn!("Vault capture ended without Ready: {error}");
        }
    });
    receive
        .await
        .map_err(|_| "capture worker ended before admission response")?
}

pub(crate) fn status(
    app: &AppHandle,
    graph: &str,
    scan: &str,
    offset: usize,
    limit: usize,
) -> Result<Value, String> {
    if !(1..=50).contains(&limit) {
        return Err("preview limit must be 1 through 50".into());
    }
    let runtime = app.state::<Arc<VaultRuntime>>();
    let coordinator =
        app.state::<crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator>();
    let _lease = coordinator.acquire_lifecycle_shared_blocking(graph)?;
    let binding = owner_binding(app, graph)?;
    let receipt = runtime.store()?.lock()?.owner_status(scan, &binding)?;
    let entries: Vec<_> = receipt.entries.iter().enumerate().skip(offset).take(limit)
        .map(|(index, e)| json!({"index":index,"path":e.path,"kind":e.kind,"bytes":e.bytes,"sha256":e.sha256})).collect();
    let next = offset.saturating_add(entries.len());
    let state = state_name(&receipt.state);
    Ok(
        json!({"scanId":scan,"graphId":graph,"graphIncarnation":binding.incarnation,"state":state,"graphWrites":false,
        "expiresAt":receipt.expires,"error":receipt.error,"fingerprint":receipt.fingerprint,"jobId":receipt.committed_job.as_ref().map(|j|j.job_id.as_str()),
        "totalEntries":receipt.entries.len(),"entries":entries,
        "nextOffset":if next < receipt.entries.len() {Some(next)} else {None},"complete":next>=receipt.entries.len()}),
    )
}

fn state_name(state: &State) -> &'static str {
    match state {
        State::Capturing => "capturing",
        State::Ready => "ready",
        State::Cancelled => "cancelled",
        State::Expired => "expired",
        State::Revoked => "revoked",
        State::Failed => "failed",
    }
}

pub(crate) fn cancel(app: &AppHandle, graph: &str, scan: &str) -> Result<Value, String> {
    let runtime = app.state::<Arc<VaultRuntime>>();
    let coordinator =
        app.state::<crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator>();
    let _lease = coordinator.acquire_lifecycle_shared_blocking(graph)?;
    let binding = owner_binding(app, graph)?;
    let store = runtime.store()?;
    let mut capture = store.lock()?;
    let receipt = capture.owner_status(scan, &binding)?;
    let receipt = capture.cancel(scan, &receipt.binding)?;
    Ok(
        json!({"scanId":scan,"state":state_name(&receipt.state),"cancelled":receipt.state == State::Cancelled,"graphWrites":false}),
    )
}
