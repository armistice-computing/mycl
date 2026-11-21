//! Native-owner orchestration; intentionally not registered as a public command.
//! A future UI registry holds Review, never accepts its fields from request JSON.
use crate::{
    app_runtime::AppHandle,
    vault_capture::Binding,
    vault_job::{Record, State, Store},
    vault_note::PreparedNote,
    vault_plan::{Input, Plan},
};
use serde_json::json;
use std::{
    io::Read,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
#[cfg(feature = "desktop")]
use tauri::Manager;

#[cfg(all(test, feature = "headless", not(feature = "desktop")))]
#[path = "vault_job_controller_tests.rs"]
pub(crate) mod tests;

pub(crate) struct Review {
    source: Binding,
    scan: String,
    plan: Plan,
    notes: Vec<PreparedNote>,
    digest: String,
    expires: u64,
}
#[path = "vault_review_view.rs"]
mod view;
const REVIEW_DATA_BUDGET: usize = 128 * 1024 * 1024;
struct Budget<'a>(&'a mut usize);
impl std::io::Write for Budget<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        *self.0 = self
            .0
            .checked_sub(bytes.len())
            .ok_or_else(|| std::io::Error::other("review retained data exceeds 128 MiB budget"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl Review {
    pub(crate) fn digest(&self) -> &str {
        &self.digest
    }
    pub(crate) fn expires(&self) -> u64 {
        self.expires
    }
    pub(crate) fn job_id(&self) -> Result<String, String> {
        crate::vault_job_bridge::job_id(&self.source, &self.scan)
    }
    pub(crate) fn title(&self) -> &str {
        &self.plan.folder_name
    }
}

pub(crate) async fn review(
    app: AppHandle,
    source_graph: String,
    scan: String,
    title: String,
) -> Result<Arc<Review>, String> {
    let capture = app
        .state::<Arc<crate::vault_runtime::VaultRuntime>>()
        .store()?;
    let lease = app
        .state::<crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator>()
        .acquire_lifecycle_shared(&source_graph)
        .await?;
    let activity = app
        .state::<Arc<crate::cell_lifecycle::CellLifecycle>>()
        .begin_background("vault-review")
        .map_err(|_| "app is draining")?;
    crate::app_runtime::async_runtime::spawn_blocking(move || {
        let (_lease, _activity) = (lease, activity);
        let source = crate::vault_selection::owner_binding(&app, &source_graph)?;
        let title = crate::ids::normalize_title(&title)?;
        let receipt = capture.lock()?.get_ready(&scan, &source)?;
        let entries: Vec<_> = receipt
            .entries
            .iter()
            .enumerate()
            .map(|(index, e)| Input {
                index,
                path: e.path.clone(),
                kind: e.kind.clone(),
                bytes: e.bytes,
                sha256: e.sha256.clone(),
            })
            .collect();
        let read = |index| -> Result<Vec<u8>, String> {
            let file = capture.lock()?.open_entry(&scan, index, &source)?;
            let mut bytes = vec![];
            file.take(32 * 1024 * 1024 + 1)
                .read_to_end(&mut bytes)
                .map_err(|e| e.to_string())?;
            if bytes.len() > 32 * 1024 * 1024 {
                return Err("review input exceeds note bound".into());
            }
            Ok(bytes)
        };
        let plan = crate::vault_plan::build(&entries, &source, &scan, &title, read)?;
        let mut remaining = REVIEW_DATA_BUDGET;
        serde_json::to_writer(Budget(&mut remaining), &(&source, &scan, &plan))
            .map_err(|e| e.to_string())?;
        remaining = remaining
            .checked_sub(64)
            .ok_or("review identity exceeds budget")?;
        for d in &plan.documents {
            remaining = remaining
                .checked_sub(
                    usize::try_from(entries[d.entry].bytes).map_err(|_| "review size overflow")?,
                )
                .ok_or("review original data exceeds 128 MiB budget")?;
        }
        let mut notes = Vec::new();
        let inline_destination = crate::vault_job_bridge::inline_destination(&source, &scan)?;
        let image_catalog =
            crate::vault_note::images::Catalog::new(&plan, &inline_destination, read)?;
        let mut inline_resolver = crate::vault_inline_links::Resolver::new(&plan);
        let mut sizes = Vec::new();
        for d in &plan.documents {
            let original = read(d.entry)?;
            let body = std::str::from_utf8(&original)
                .map_err(|_| "invalid image source note")?
                .get(d.metadata.body_start..d.body_end)
                .ok_or("invalid image source body")?;
            let (images, warnings) = image_catalog.for_note(d, body)?;
            let note = crate::vault_note::prepare_with_images(
                d,
                &entries[d.entry],
                original,
                images,
                warnings,
            )?;
            let before = remaining;
            serde_json::to_writer(Budget(&mut remaining), &note).map_err(|e| e.to_string())?;
            sizes.push(before - remaining);
            inline_resolver.register_note(&note)?;
            notes.push(note);
        }
        for (note, size) in notes.iter_mut().zip(sizes) {
            inline_resolver.install(note, &inline_destination)?;
            remaining += size;
            serde_json::to_writer(Budget(&mut remaining), &note).map_err(|e| e.to_string())?;
        }
        let digest = crate::vault_job_bridge::review_digest(&plan, &notes)?;
        Ok(Arc::new(Review {
            expires: receipt.expires,
            source,
            scan,
            plan,
            notes,
            digest,
        }))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Leases move into the blocking worker: dropping the awaiting request cannot
/// release them while assembly or publication is still running.
pub(crate) async fn commit(
    app: AppHandle,
    review: Arc<Review>,
    expected_digest: String,
    cancelled: Arc<AtomicBool>,
) -> Result<Record, String> {
    commit_notified(app, review, expected_digest, cancelled, None).await
}
pub(crate) async fn commit_notified(
    app: AppHandle,
    review: Arc<Review>,
    expected_digest: String,
    cancelled: Arc<AtomicBool>,
    admitted: Option<Box<dyn FnOnce(&Record) + Send>>,
) -> Result<Record, String> {
    if review.digest != expected_digest {
        return Err("review changed; review the import again".into());
    }
    let destination = crate::vault_job_bridge::destination(&review.source, &review.scan)?;
    if destination == review.source.graph {
        return Err("import requires a new graph".into());
    }
    app.state::<Arc<crate::cell_graph_boundary::CellGraphBoundary>>()
        .authorize_graph_id(&destination)
        .map_err(|e| e.mcp_message())?;
    let coordinator =
        app.state::<crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator>();
    let (source_lease, target_lease) = if review.source.graph < destination {
        let source = coordinator
            .acquire_lifecycle_shared(&review.source.graph)
            .await?;
        let target = coordinator
            .acquire_lifecycle_exclusive(&destination)
            .await?;
        (source, target)
    } else {
        let target = coordinator
            .acquire_lifecycle_exclusive(&destination)
            .await?;
        let source = coordinator
            .acquire_lifecycle_shared(&review.source.graph)
            .await?;
        (source, target)
    };
    let activity = app
        .state::<Arc<crate::cell_lifecycle::CellLifecycle>>()
        .begin_background("vault-commit")
        .map_err(|_| "app is draining")?;
    drop(coordinator);
    crate::app_runtime::async_runtime::spawn_blocking(move ||{
        let (_source,_target,_activity)=(source_lease,target_lease,activity);
        if cancelled.load(Ordering::Acquire) {return Err("import cancelled before acceptance".into());}
        if crate::vault_selection::owner_binding(&app,&review.source.graph)?!=review.source {return Err("source graph lifetime changed".into());}
        let profile=crate::profile_paths::profile_dir(&app)?;
        let capture=app.state::<Arc<crate::vault_runtime::VaultRuntime>>().store()?;
        let jobs=Store::open(&profile)?;
        let record=crate::vault_job_bridge::accept(&profile,&capture,&jobs,&review.source,&review.scan,&review.plan,&review.notes)?;
        if let Some(notify)=admitted {notify(&record);}
        if matches!(record.state,State::Publishing{..}|State::Uncertain{..}) {
            let _publication=crate::graph_duplicate_storage::acquire_graph_publication_lock(&profile,&record.identity.destination_graph)?;
            let observed=read_destination(&profile.join("graphs").join(&record.identity.destination_graph))?;
            let outcome=jobs.observe_publication(&record.identity,observed.as_ref(),crate::vault_job_bridge::now()?)?;
            if outcome.state==State::Published {
                if let Some(graph)=observed.as_ref().filter(|g|g.status==crate::runtime_config::GRAPH_STATUS_ACTIVE) {
                    crate::graph_service::repair_published_graph_projections_best_effort(&app,graph);
                }
            }
            return Ok(outcome);
        }
        if record.state!=State::Accepted {return Ok(record);}
        let identity=&record.identity;
        let target=profile.join("graphs").join(&identity.destination_graph);
        let graph:crate::graph_record_store::GraphRecord=serde_json::from_value(json!({
            "graphId":identity.destination_graph,"title":review.plan.folder_name,"description":null,"status":"active",
            "origin":"local","providerId":"local-profile","localPath":crate::storage::display_path(&target),
            "createdAt":record.created_at.to_string(),"updatedAt":record.created_at.to_string(),"incarnationId":identity.destination_incarnation,
            "createdByOperationId":identity.operation_id,"capabilities":["graph.local.open","graph.local.persist","graph.local.query.rdf","graph.local.export.rdf","document.local.persist","document.local.ydoc","workspace.local.ydoc","document.local.tiptap-tree","graph.local.semantic-index"]
        })).map_err(|e|e.to_string())?;
        let attachment_ids:std::collections::BTreeSet<_>=review.plan.attachments.iter().copied().collect();
        let files:Vec<_>=review.plan.capture_entries.iter().filter(|e|attachment_ids.contains(&e.index)).cloned().collect();
        let stage=crate::vault_graph_stage::assemble_tracked(&profile,graph,&review.plan,&review.notes,&files,
            |index|capture.lock()?.open_entry(&review.scan,index,&review.source),
            |path|register_stage(&jobs,identity,path),
            |_|if cancelled.load(Ordering::Acquire){Err("import cancelled".into())}else{Ok(())});
        let stage=match stage {Ok(stage)=>stage,Err(error)=>{
            jobs.abandon(identity,crate::vault_job_bridge::now()?)?;
            return Err(format!("import abandoned before publication: {error}"));
        }};
        let result=stage.publish(||{
            if cancelled.load(Ordering::Acquire){return Err("import cancelled before publication".into());}
            let guard=crate::vault_job_bridge::authorize(&profile,&capture,&jobs,identity)?;
            jobs.publication_intent(identity,crate::vault_job_bridge::now()?)?;
            #[cfg(test)]
            fault(&identity.job_id,1)?;
            Ok(guard)
        });
        match result {
            Ok(graph)=>{
                #[cfg(test)]
                fault(&identity.job_id,2)?;
                // This is our own confirmed publication, not a caller-supplied
                // observation. Recording it remains valid if deletion follows.
                crate::graph_service::repair_published_graph_projections_best_effort(&app,&graph);
                let outcome=jobs.observe_publication(identity,Some(&graph),crate::vault_job_bridge::now()?)
                    .map_err(|e|format!("graph published; job {} needs outcome reconciliation: {e}",identity.job_id))?;
                Ok(outcome)
            }
            Err(error)=>{
                let record=jobs.read(&identity.job_id)?;
                if matches!(record.state,State::Accepted|State::Building{..}) {
                    jobs.abandon(identity,crate::vault_job_bridge::now()?)?;
                    return Err(format!("publication refused: {error}"));
                }
                let _publication=crate::graph_duplicate_storage::acquire_graph_publication_lock(&profile,&identity.destination_graph)?;
                let observed=read_destination(&target)?;
                jobs.observe_publication(identity,observed.as_ref(),crate::vault_job_bridge::now()?)
            }
        }
    }).await.map_err(|e|e.to_string())?
}

/// Native-owner status can reconcile historical publication even after source
/// consent expires. It never obtains source bytes or creates a destination.
pub(crate) async fn status(app: AppHandle, job_id: String) -> Result<Record, String> {
    let profile = crate::profile_paths::profile_dir(&app)?;
    let jobs = Store::open(&profile)?;
    let record = jobs.read(&job_id)?;
    if record.identity.source.principal != "human:owner" {
        return Err("job is not owned by native owner".into());
    }
    app.state::<Arc<crate::cell_graph_boundary::CellGraphBoundary>>()
        .authorize_graph_id(&record.identity.destination_graph)
        .map_err(|e| e.mcp_message())?;
    if !matches!(
        record.state,
        State::Publishing { .. } | State::Uncertain { .. }
    ) {
        return Ok(record);
    }
    let lease = app
        .state::<crate::crdt_engine::persistence_coordinator::GraphPersistenceCoordinator>()
        .acquire_lifecycle_exclusive(&record.identity.destination_graph)
        .await?;
    let activity = app
        .state::<Arc<crate::cell_lifecycle::CellLifecycle>>()
        .begin_background("vault-status")
        .map_err(|_| "app is draining")?;
    crate::app_runtime::async_runtime::spawn_blocking(move || {
        let (_lease, _activity) = (lease, activity);
        let _publication = crate::graph_duplicate_storage::acquire_graph_publication_lock(
            &profile,
            &record.identity.destination_graph,
        )?;
        let observed = read_destination(
            &profile
                .join("graphs")
                .join(&record.identity.destination_graph),
        )?;
        let outcome = jobs.observe_publication(
            &record.identity,
            observed.as_ref(),
            crate::vault_job_bridge::now()?,
        )?;
        if outcome.state == State::Published {
            if let Some(graph) = observed
                .as_ref()
                .filter(|g| g.status == crate::runtime_config::GRAPH_STATUS_ACTIVE)
            {
                crate::graph_service::repair_published_graph_projections_best_effort(&app, graph);
            }
        }
        Ok(outcome)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(test)]
static FAULT: std::sync::Mutex<Option<(String, u8)>> = std::sync::Mutex::new(None);
#[cfg(test)]
fn fault(job: &str, phase: u8) -> Result<(), String> {
    let mut fault = FAULT.lock().unwrap();
    if fault
        .as_ref()
        .is_some_and(|(id, p)| id == job && *p == phase)
    {
        fault.take();
        return Err("injected publication interruption".into());
    }
    Ok(())
}

fn register_stage(
    jobs: &Store,
    identity: &crate::vault_job::Identity,
    path: &Path,
) -> Result<(), String> {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or("invalid stage name")?;
    let suffix = name
        .strip_prefix(&format!("{}-", identity.destination_graph))
        .ok_or("stage belongs to another graph")?;
    let stage = uuid::Uuid::parse_str(suffix)
        .map_err(|e| e.to_string())?
        .to_string();
    jobs.building(identity, &stage, crate::vault_job_bridge::now()?)?;
    crate::storage::write_json(&path.join("vault-job.json"),&json!({"version":1,"stageId":stage,"binding":crate::vault_job_bridge::committed(identity)?})).map_err(|e|e.to_string())
}
pub(crate) fn read_destination(
    path: &Path,
) -> Result<Option<crate::graph_record_store::GraphRecord>, String> {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.to_string()),
        Ok(meta) if !meta.is_dir() => {
            return Err("destination graph directory is linked or not a directory".into())
        }
        Ok(_) => {}
    }
    match std::fs::symlink_metadata(path.join("graph.json")) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
        Ok(meta) if meta.is_file() => crate::storage::read_json(&path.join("graph.json"))
            .map(Some)
            .map_err(|e| e.to_string()),
        Ok(_) => Err("destination graph record is not a regular file".into()),
    }
}
