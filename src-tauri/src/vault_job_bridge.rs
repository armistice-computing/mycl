//! Internal capture/job binding. The native owner adapter must derive Binding
//! from authenticated state and hold source/destination lifecycle leases first.
//! No caller-supplied paths, identities or successful closures are authorization.
use crate::{
    vault_capture::{Binding, CommittedJob, Receipt},
    vault_capture_access::{CaptureAccess, CaptureGuard},
    vault_job::{Identity, Record, Store},
    vault_note::PreparedNote,
    vault_plan::{Input, Plan},
};
use sha2::{Digest, Sha256};
use std::path::Path;

fn hash(value: &impl serde::Serialize) -> Result<String, String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(value).map_err(|e| e.to_string())?)
    ))
}
fn stable_uuid(value: &impl serde::Serialize) -> Result<String, String> {
    let digest = Sha256::digest(serde_json::to_vec(value).map_err(|e| e.to_string())?);
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 15) | 0x80;
    bytes[8] = (bytes[8] & 63) | 0x80;
    Ok(uuid::Uuid::from_bytes(bytes).to_string())
}
pub(crate) fn destination(source: &Binding, scan: &str) -> Result<String, String> {
    Ok(format!(
        "vault-{}",
        stable_uuid(&("vault-job-v1", source, scan))?
    ))
}
pub(crate) fn job_id(source:&Binding,scan:&str)->Result<String,String> {
    stable_uuid(&("vault-job-v1",source,scan))
}
pub(crate) fn inline_destination(
    source: &Binding,
    scan: &str,
) -> Result<crate::vault_inline_links::Destination, String> {
    let job = stable_uuid(&("vault-job-v1", source, scan))?;
    Ok(crate::vault_inline_links::Destination {
        graph_id: format!("vault-{job}"),
        incarnation: stable_uuid(&("vault-graph-v1", &job))?,
        operation_id: format!("garden-admit-vault-{job}"),
    })
}
pub(crate) fn now() -> Result<u64, String> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_millis()
        .try_into()
        .map_err(|_| "job clock overflow".into())
}
pub(crate) fn plan_digest(plan: &Plan) -> Result<String, String> {
    hash(&("vault-plan-v1", plan))
}
pub(crate) fn preparation_digest(notes: &[PreparedNote]) -> Result<String, String> {
    let mut entries = std::collections::BTreeMap::new();
    for note in notes {
        note.verify()?;
        if entries
            .insert(&note.document_id, &note.preparation_sha256)
            .is_some()
        {
            return Err("duplicate note preparation".into());
        }
    }
    hash(&("vault-preparation-v1", entries))
}
pub(crate) fn review_digest(plan: &Plan, notes: &[PreparedNote]) -> Result<String, String> {
    hash(&(
        "vault-review-v1",
        plan_digest(plan)?,
        preparation_digest(notes)?,
    ))
}
pub(crate) fn committed(identity: &Identity) -> Result<CommittedJob, String> {
    Ok(CommittedJob {
        job_id: identity.job_id.clone(),
        identity_digest: hash(&("vault-job-identity-v1", identity))?,
    })
}
fn captured_inputs(receipt: &Receipt) -> Vec<Input> {
    receipt
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
        .collect()
}

/// Persist job identity before irrevocably linking the selected scan to it.
/// A committed scan with missing/corrupt history is an error, never a new job.
pub(crate) fn accept(
    profile: &Path,
    capture: &CaptureAccess,
    jobs: &Store,
    source: &Binding,
    scan: &str,
    plan: &Plan,
    notes: &[PreparedNote],
) -> Result<Record, String> {
    accept_with_linker(
        profile,
        capture,
        jobs,
        source,
        scan,
        plan,
        notes,
        |store, scan, binding, link| store.commit_job(scan, binding, link),
    )
}
fn accept_with_linker(
    profile: &Path,
    capture: &CaptureAccess,
    jobs: &Store,
    source: &Binding,
    scan: &str,
    plan: &Plan,
    notes: &[PreparedNote],
    link_capture: impl FnOnce(
        &mut crate::vault_capture::CaptureStore,
        &str,
        &Binding,
        CommittedJob,
    ) -> Result<Receipt, String>,
) -> Result<Record, String> {
    jobs.require_profile(profile)?;
    let plan_digest = plan_digest(plan)?;
    let preparation_digest = preparation_digest(notes)?;
    let mut guard = capture.lock()?;
    guard.require_profile(profile)?;
    let receipt = guard.get_ready(scan, source)?;
    if captured_inputs(&receipt) != plan.capture_entries {
        return Err("reviewed plan differs from selected capture".into());
    }
    let job_id = stable_uuid(&("vault-job-v1", source, scan))?;
    let identity = Identity {
        operation_id: format!("garden-admit-vault-{job_id}"),
        destination_graph: format!("vault-{job_id}"),
        destination_incarnation: stable_uuid(&("vault-graph-v1", &job_id))?,
        job_id,
        source: source.clone(),
        scan_id: scan.into(),
        capture_fingerprint: receipt
            .fingerprint
            .clone()
            .ok_or("capture fingerprint missing")?,
        plan_digest,
        preparation_digest,
        consent_expires_at: receipt.expires,
    };
    let link = committed(&identity)?;
    let record = if let Some(previous) = &receipt.committed_job {
        if previous != &link {
            return Err("scan already committed with different reviewed options".into());
        }
        let record = jobs.read(&link.job_id)?;
        if record.identity != identity {
            return Err("committed job identity mismatch".into());
        }
        return Ok(record);
    } else {
        jobs.create(identity, now()?)?
    };
    if let Err(error) = link_capture(&mut guard, scan, source, link) {
        if let Err(repair) = jobs.abandon_unlinked(&record.identity, now()?) {
            return Err(format!(
                "capture link failed: {error}; job outcome needs repair: {repair}"
            ));
        }
        return Err(format!("capture link failed before staging: {error}"));
    }
    Ok(record)
}

/// Retain the live capture guard through intent persistence and publication.
/// Active graph lifetime must also be checked under the caller's lifecycle lease.
pub(crate) fn authorize<'a>(
    profile: &Path,
    capture: &'a CaptureAccess,
    jobs: &Store,
    identity: &Identity,
) -> Result<CaptureGuard<'a>, String> {
    jobs.require_profile(profile)?;
    let mut guard = capture.lock()?;
    guard.require_profile(profile)?;
    let receipt = guard.get_ready(&identity.scan_id, &identity.source)?;
    if receipt.committed_job.as_ref() != Some(&committed(identity)?)
        || receipt.fingerprint.as_deref() != Some(identity.capture_fingerprint.as_str())
        || receipt.expires != identity.consent_expires_at
    {
        return Err("live capture does not authorize this job".into());
    }
    let record = jobs.read(&identity.job_id)?;
    if record.identity != *identity {
        return Err("job identity mismatch".into());
    }
    Ok(guard)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault_capture::{CaptureStore, SourceKind};
    use std::io::{Read, Write};
    fn fixture(
        profile: &Path,
    ) -> (
        CaptureAccess,
        Store,
        Binding,
        String,
        Plan,
        Vec<PreparedNote>,
    ) {
        let source = Binding {
            principal: "human:owner".into(),
            graph: "source".into(),
            incarnation: uuid::Uuid::new_v4().to_string(),
        };
        let mut zipfile = tempfile::tempfile().unwrap();
        {
            let mut zip = zip::ZipWriter::new(&mut zipfile);
            zip.start_file("note.md", zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(b"# Captured note\n\nOriginal body.").unwrap();
            zip.finish().unwrap();
        }
        let mut store = CaptureStore::open(profile).unwrap();
        let grant = store
            .grant_selected(source.clone(), zipfile, SourceKind::Zip, 60000)
            .unwrap();
        let mut ticket = store.begin(&grant, &source).unwrap();
        let snapshot = ticket.capture();
        let receipt = store.finish(ticket, &source, snapshot).unwrap();
        let scan = receipt.scan_id.clone();
        let entries = captured_inputs(&receipt);
        let mut read = |i| {
            let mut file = store.open_entry(&scan, i, &source)?;
            let mut bytes = vec![];
            file.read_to_end(&mut bytes).map_err(|e| e.to_string())?;
            Ok(bytes)
        };
        let plan =
            crate::vault_plan::build(&entries, &source, &scan, "Imported", &mut read).unwrap();
        let notes = plan
            .documents
            .iter()
            .map(|d| {
                crate::vault_note::prepare(d, &entries[d.entry], read(d.entry).unwrap()).unwrap()
            })
            .collect();
        (
            CaptureAccess::new(store),
            Store::open(profile).unwrap(),
            source,
            scan,
            plan,
            notes,
        )
    }
    #[test]
    fn acceptance_is_stable_and_links_scan_to_immutable_job() {
        let profile = tempfile::tempdir().unwrap();
        let (capture, jobs, source, scan, plan, notes) = fixture(profile.path());
        let first = accept(
            profile.path(),
            &capture,
            &jobs,
            &source,
            &scan,
            &plan,
            &notes,
        )
        .unwrap();
        let second = accept(
            profile.path(),
            &capture,
            &jobs,
            &source,
            &scan,
            &plan,
            &notes,
        )
        .unwrap();
        assert_eq!(first, second);
        assert_eq!(
            capture
                .lock()
                .unwrap()
                .get_ready(&scan, &source)
                .unwrap()
                .committed_job,
            Some(committed(&first.identity).unwrap())
        );
        authorize(profile.path(), &capture, &jobs, &first.identity).unwrap();
    }
    #[test]
    fn committed_scan_does_not_recreate_lost_job_history() {
        let profile = tempfile::tempdir().unwrap();
        let (capture, jobs, source, scan, plan, notes) = fixture(profile.path());
        let first = accept(
            profile.path(),
            &capture,
            &jobs,
            &source,
            &scan,
            &plan,
            &notes,
        )
        .unwrap();
        let path = profile
            .path()
            .join(".vault-jobs")
            .join(format!("{}.json", first.identity.job_id));
        std::fs::remove_file(&path).unwrap();
        assert!(accept(
            profile.path(),
            &capture,
            &jobs,
            &source,
            &scan,
            &plan,
            &notes
        )
        .is_err());
        assert!(!path.exists());
    }
    #[test]
    fn changed_options_and_revoked_capture_cannot_publish() {
        let profile = tempfile::tempdir().unwrap();
        let (capture, jobs, source, scan, mut plan, notes) = fixture(profile.path());
        let first = accept(
            profile.path(),
            &capture,
            &jobs,
            &source,
            &scan,
            &plan,
            &notes,
        )
        .unwrap();
        plan.folder_name = "Different reviewed name".into();
        assert!(accept(
            profile.path(),
            &capture,
            &jobs,
            &source,
            &scan,
            &plan,
            &notes
        )
        .is_err());
        capture.lock().unwrap().cancel(&scan, &source).unwrap();
        assert!(authorize(profile.path(), &capture, &jobs, &first.identity).is_err());
    }

    #[test]
    fn expiration_between_job_creation_and_capture_link_is_terminal() {
        let profile = tempfile::tempdir().unwrap();
        let (capture, jobs, source, scan, plan, notes) = fixture(profile.path());
        let id = stable_uuid(&("vault-job-v1", &source, scan.as_str())).unwrap();
        assert!(accept_with_linker(
            profile.path(),
            &capture,
            &jobs,
            &source,
            &scan,
            &plan,
            &notes,
            |store, scan, binding, link| store.commit_expired_job_for_test(scan, binding, link)
        )
        .is_err());
        let record = jobs.read(&id).unwrap();
        assert_eq!(
            record.state,
            crate::vault_job::State::Abandoned { stage_id: None }
        );
        assert!(authorize(profile.path(), &capture, &jobs, &record.identity).is_err());
    }

    #[test]
    fn lost_link_ack_cannot_leave_an_accepted_job_with_authority() {
        let profile = tempfile::tempdir().unwrap();
        let (capture, jobs, source, scan, plan, notes) = fixture(profile.path());
        assert!(accept_with_linker(
            profile.path(),
            &capture,
            &jobs,
            &source,
            &scan,
            &plan,
            &notes,
            |store, scan, binding, link| {
                store.commit_job(scan, binding, link)?;
                Err("injected failure after capture link".into())
            }
        )
        .is_err());
        let retry = accept(
            profile.path(),
            &capture,
            &jobs,
            &source,
            &scan,
            &plan,
            &notes,
        )
        .unwrap();
        assert_eq!(
            retry.state,
            crate::vault_job::State::Abandoned { stage_id: None }
        );
        assert!(jobs
            .publication_intent(&retry.identity, now().unwrap())
            .is_err());
    }
}
