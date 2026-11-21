//! Owner-only recovery of interrupted, unpublished stages. No age-based deletion
//! and no public command. Caller-visible phases are evidence, not worker liveness.
use crate::{
    app_runtime::AppHandle,
    vault_capture::CommittedJob,
    vault_job::{Record, State, Store},
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};
#[cfg(feature = "desktop")]
use tauri::Manager;

const MAX_TREE_ENTRIES: usize = 100_000;
const MAX_DEPTH: usize = 64;

#[derive(Debug, Serialize)]
pub(crate) struct Report {
    pub job: Record,
    pub stage: &'static str,
    pub reason: Option<String>,
    /// Logical file lengths, not a promise of physical disk space reclaimed.
    pub logical_bytes: u64,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Marker {
    version: u32,
    stage_id: String,
    binding: CommittedJob,
}

/// Explicit discard/recovery action, not an automatic startup sweep. Startup
/// should report interrupted jobs; it must not infer that their copies are unwanted.
pub(crate) async fn discard_interrupted(app: AppHandle, job_id: String) -> Result<Report, String> {
    let profile = crate::profile_paths::profile_dir(&app)?;
    let jobs = Store::open(&profile)?;
    let record = jobs.read(&job_id)?;
    if record.identity.source.principal != "human:owner" {
        return Err("job is not owned by native owner".into());
    }
    app.state::<Arc<crate::cell_graph_boundary::CellGraphBoundary>>()
        .authorize_graph_id(&record.identity.destination_graph)
        .map_err(|e| e.mcp_message())?;
    let lease = app
        .state::<crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator>()
        .acquire_lifecycle_exclusive(&record.identity.destination_graph)
        .await?;
    let activity = app
        .state::<Arc<crate::cell_lifecycle::CellLifecycle>>()
        .begin_background("vault-recovery")
        .map_err(|_| "app is draining")?;
    crate::app_runtime::async_runtime::spawn_blocking(move || {
        let (_lease, _activity) = (lease, activity);
        let report = recover_profile(&profile, &job_id, remove)?;
        if report.job.state == State::Published {
            if let Ok(Some(graph)) = crate::vault_job_controller::read_destination(
                &profile
                    .join("graphs")
                    .join(&report.job.identity.destination_graph),
            ) {
                let identity = &report.job.identity;
                if graph.status == crate::runtime_config::GRAPH_STATUS_ACTIVE
                    && graph.incarnation_id.as_deref()
                        == Some(identity.destination_incarnation.as_str())
                    && graph.created_by_operation_id.as_deref()
                        == Some(identity.operation_id.as_str())
                {
                    crate::graph_service::repair_published_graph_projections_best_effort(
                        &app, &graph,
                    );
                }
            }
        }
        Ok(report)
    })
    .await
    .map_err(|e| e.to_string())?
}

fn report(job: Record, stage: &'static str, reason: Option<String>, logical_bytes: u64) -> Report {
    Report {
        job,
        stage,
        reason,
        logical_bytes,
    }
}
fn exists(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.to_string()),
    }
}
fn real_directory(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => Ok(true),
        Ok(_) => Err("recovery directory is linked or not a directory".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.to_string()),
    }
}
fn marker(path: &Path) -> Result<Marker, String> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    let file = options.open(path).map_err(|e| e.to_string())?;
    if !file.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err("ownership marker is not regular".into());
    }
    let mut bytes = vec![];
    file.take(65537)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > 65536 {
        return Err("ownership marker exceeds bound".into());
    }
    serde_json::from_slice(&bytes).map_err(|e| e.to_string())
}

fn inventory(root: &Path, limit: usize) -> Result<u64, String> {
    #[cfg(unix)]
    let device = {
        use std::os::unix::fs::MetadataExt;
        fs::symlink_metadata(root).map_err(|e| e.to_string())?.dev()
    };
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    let mut count = 0;
    let mut bytes = 0u64;
    while let Some((path, depth)) = stack.pop() {
        count += 1;
        if count > limit || depth > MAX_DEPTH {
            return Err("stage inventory exceeds safety bound".into());
        }
        let meta = fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if meta.dev() != device {
                return Err("stage crosses filesystem boundary".into());
            }
        }
        if meta.is_dir() {
            for child in fs::read_dir(&path).map_err(|e| e.to_string())? {
                if stack.len() + count >= limit {
                    return Err("stage inventory exceeds safety bound".into());
                }
                stack.push((child.map_err(|e| e.to_string())?.path(), depth + 1));
            }
        } else if meta.is_file() {
            bytes = bytes.checked_add(meta.len()).ok_or("stage size overflow")?;
        } else {
            return Err("stage contains a link or special file; retained".into());
        }
    }
    Ok(bytes)
}
fn remove(path: &Path, directory: bool) -> Result<(), String> {
    if directory {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
    .map_err(|e| e.to_string())
}

fn recover_profile(
    profile: &Path,
    job_id: &str,
    mut remove_child: impl FnMut(&Path, bool) -> Result<(), String>,
) -> Result<Report, String> {
    let profile = fs::canonicalize(profile).map_err(|e| e.to_string())?;
    let jobs = Store::open(&profile)?;
    let initial = jobs.read(job_id)?;
    let root = profile.join(".graph-staging");
    // Validate before the shared lock helper could traverse a redirected root.
    real_directory(&root)?;
    if exists(&root.join("locks"))? {
        real_directory(&root.join("locks"))?;
    }
    let _durability = crate::cell_durability::write_guard();
    let _publication = crate::graph_duplicate_storage::acquire_graph_publication_lock(
        &profile,
        &initial.identity.destination_graph,
    )?;
    let record = jobs.read(job_id)?;
    if record.identity != initial.identity {
        return Err("job identity changed while awaiting recovery".into());
    }
    let target = profile
        .join("graphs")
        .join(&record.identity.destination_graph);
    if matches!(
        record.state,
        State::Publishing { .. } | State::Uncertain { .. }
    ) {
        let observed = crate::vault_job_controller::read_destination(&target)?;
        let job = jobs.observe_publication(
            &record.identity,
            observed.as_ref(),
            crate::vault_job_bridge::now()?,
        )?;
        return Ok(report(
            job,
            "retained",
            Some("publication history reconciled; no stage deletion inferred".into()),
            0,
        ));
    }
    if record.state == State::Published {
        return Ok(report(
            record,
            "retained",
            Some("published history is not cleanup authority".into()),
            0,
        ));
    }
    if exists(&target)? {
        return Ok(report(
            record,
            "retained",
            Some("destination exists; stage retained for inspection".into()),
            0,
        ));
    }
    let stage_id = match &record.state {
        State::Building { stage_id }
        | State::Abandoned {
            stage_id: Some(stage_id),
        } => Some(stage_id.clone()),
        State::Accepted | State::Abandoned { stage_id: None } => None,
        _ => return Err("unexpected recovery state".into()),
    };
    let Some(stage_id) = stage_id else {
        let job = jobs.abandon(&record.identity, crate::vault_job_bridge::now()?)?;
        return Ok(report(job, "absent", None, 0));
    };
    let uuid = uuid::Uuid::parse_str(&stage_id).map_err(|e| e.to_string())?;
    let stage = root.join(format!(
        "{}-{}",
        record.identity.destination_graph,
        uuid.simple()
    ));
    match real_directory(&stage) {
        Ok(false) => {
            let job = jobs.abandon(&record.identity, crate::vault_job_bridge::now()?)?;
            return Ok(report(job, "absent", None, 0));
        }
        Err(error) => return Ok(report(record, "retained", Some(error), 0)),
        Ok(true) => {}
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if fs::symlink_metadata(&stage)
            .map_err(|e| e.to_string())?
            .dev()
            != fs::symlink_metadata(&root)
                .map_err(|e| e.to_string())?
                .dev()
        {
            return Ok(report(
                record,
                "retained",
                Some("stage root is on another filesystem".into()),
                0,
            ));
        }
    }
    let children = fs::read_dir(&stage)
        .map_err(|e| e.to_string())?
        .map(|e| e.map(|e| e.path()).map_err(|e| e.to_string()))
        .take(MAX_TREE_ENTRIES + 1)
        .collect::<Result<Vec<_>, _>>()?;
    if children.len() > MAX_TREE_ENTRIES {
        return Ok(report(
            record,
            "retained",
            Some("stage inventory exceeds safety bound".into()),
            0,
        ));
    }
    let ownership = stage.join("vault-job.json");
    if !children.is_empty() {
        let marker = match marker(&ownership) {
            Ok(marker) => marker,
            Err(error) => {
                return Ok(report(
                    record,
                    "retained",
                    Some(format!("cannot verify stage ownership: {error}")),
                    0,
                ))
            }
        };
        if marker.version != 1
            || marker.stage_id != stage_id
            || marker.binding != crate::vault_job_bridge::committed(&record.identity)?
        {
            return Ok(report(
                record,
                "retained",
                Some("stage ownership mismatch".into()),
                0,
            ));
        }
    }
    if exists(&stage.join("graph.json"))? {
        let graph = crate::vault_job_controller::read_destination(&stage)?;
        if graph
            .as_ref()
            .is_some_and(|g| g.status != crate::runtime_config::GRAPH_STATUS_DELETED)
        {
            return Ok(report(
                record,
                "retained",
                Some("stage manifest conflicts with pre-publication job state".into()),
                0,
            ));
        }
    }
    let bytes = match inventory(&stage, MAX_TREE_ENTRIES) {
        Ok(bytes) => bytes,
        Err(error) => return Ok(report(record, "retained", Some(error), 0)),
    };
    let job = jobs.abandon(&record.identity, crate::vault_job_bridge::now()?)?;
    crate::rdf_store_service::evict_graph_store(&stage)?;
    if let Err(error) =
        crate::graph_duplicate_storage::remove_stage_payloads(&stage, &mut remove_child)
    {
        return Ok(report(
            job,
            "retained",
            Some(format!("stage removal incomplete: {error}")),
            bytes,
        ));
    }
    Ok(report(job, "removed", None, bytes))
}

#[cfg(test)]
#[path = "vault_job_recovery_tests.rs"]
mod tests;
