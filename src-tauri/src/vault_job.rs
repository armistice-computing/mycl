//! Durable job outcomes, not an authorization surface or executor. Callers must
//! authenticate source consent and hold publication/lifecycle locks before
//! observing a destination. This module acquires only its own per-job file lock.
use crate::{graph_record_store::GraphRecord, vault_capture::Binding};
use fs4::fs_std::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Identity {
    pub job_id: String,
    pub operation_id: String,
    pub source: Binding,
    pub scan_id: String,
    pub capture_fingerprint: String,
    pub plan_digest: String,
    pub preparation_digest: String,
    pub destination_graph: String,
    pub destination_incarnation: String,
    pub consent_expires_at: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    fn identity() -> Identity {
        Identity {
            job_id: uuid::Uuid::new_v4().to_string(),
            operation_id: "garden-admit-job".into(),
            source: Binding {
                graph: "source".into(),
                incarnation: uuid::Uuid::new_v4().to_string(),
                principal: "human:owner".into(),
            },
            scan_id: uuid::Uuid::new_v4().to_string(),
            capture_fingerprint: "a".repeat(64),
            plan_digest: "b".repeat(64),
            preparation_digest: "c".repeat(64),
            destination_graph: "new-vault".into(),
            destination_incarnation: uuid::Uuid::new_v4().to_string(),
            consent_expires_at: 10000,
        }
    }
    fn observed(id: &Identity) -> GraphRecord {
        serde_json::from_value(serde_json::json!({"graphId":id.destination_graph,"title":"Vault","status":"active","origin":"local","providerId":"local-profile","localPath":"unused","createdAt":"1","updatedAt":"1","capabilities":[],"incarnationId":id.destination_incarnation,"createdByOperationId":id.operation_id})).unwrap()
    }
    #[test]
    fn identity_conflicts_refuse_without_changing_history() {
        let profile = tempfile::tempdir().unwrap();
        let store = Store::open(profile.path()).unwrap();
        let id = identity();
        let original = store.create(id.clone(), 1).unwrap();
        let mut wrong = id.clone();
        wrong.plan_digest = "d".repeat(64);
        assert!(store.create(wrong.clone(), 2).is_err());
        assert!(store
            .building(&wrong, &uuid::Uuid::new_v4().to_string(), 3)
            .is_err());
        assert_eq!(store.read(&id.job_id).unwrap(), original);
    }
    #[test]
    fn missing_destination_after_intent_is_uncertain_not_rebuildable() {
        let profile = tempfile::tempdir().unwrap();
        let store = Store::open(profile.path()).unwrap();
        let id = identity();
        store.create(id.clone(), 1).unwrap();
        let stage = uuid::Uuid::new_v4().to_string();
        store.building(&id, &stage, 2).unwrap();
        store.publication_intent(&id, 3).unwrap();
        drop(store);
        let store = Store::open(profile.path()).unwrap();
        assert!(matches!(
            store.observe_publication(&id, None, 4).unwrap().state,
            State::Uncertain { .. }
        ));
        assert!(store.building(&id, &stage, 5).is_err());
        assert!(store.publication_intent(&id, 5).is_err());
        assert!(store.abandon(&id, 5).is_err());
    }
    #[test]
    fn published_is_historical_even_after_destination_disappears() {
        let profile = tempfile::tempdir().unwrap();
        let store = Store::open(profile.path()).unwrap();
        let id = identity();
        store.create(id.clone(), 1).unwrap();
        store
            .building(&id, &uuid::Uuid::new_v4().to_string(), 2)
            .unwrap();
        store.publication_intent(&id, 3).unwrap();
        assert_eq!(
            store
                .observe_publication(&id, Some(&observed(&id)), 4)
                .unwrap()
                .state,
            State::Published
        );
        assert_eq!(
            store.observe_publication(&id, None, 5).unwrap().state,
            State::Published
        );
        assert_eq!(
            store.create(id.clone(), 20000).unwrap().state,
            State::Published
        );
        assert!(store
            .building(&id, &uuid::Uuid::new_v4().to_string(), 20001)
            .is_err());
    }
    #[test]
    fn wrong_incarnation_never_counts_as_publication() {
        let profile = tempfile::tempdir().unwrap();
        let store = Store::open(profile.path()).unwrap();
        let id = identity();
        store.create(id.clone(), 1).unwrap();
        store
            .building(&id, &uuid::Uuid::new_v4().to_string(), 2)
            .unwrap();
        store.publication_intent(&id, 3).unwrap();
        let mut graph = observed(&id);
        graph.incarnation_id = Some(uuid::Uuid::new_v4().to_string());
        assert!(matches!(
            store
                .observe_publication(&id, Some(&graph), 4)
                .unwrap()
                .state,
            State::Uncertain { .. }
        ));
        assert_eq!(
            store
                .observe_publication(&id, Some(&observed(&id)), 5)
                .unwrap()
                .state,
            State::Published
        );
    }
    #[test]
    fn cancellation_preserves_stage_identity_and_expiry_prevents_publication() {
        let profile = tempfile::tempdir().unwrap();
        let store = Store::open(profile.path()).unwrap();
        let id = identity();
        store.create(id.clone(), 1).unwrap();
        let stage = uuid::Uuid::new_v4().to_string();
        store.building(&id, &stage, 2).unwrap();
        assert!(store.publication_intent(&id, 10000).is_err());
        assert_eq!(
            store.abandon(&id, 10001).unwrap().state,
            State::Abandoned {
                stage_id: Some(stage)
            }
        );
    }
    #[test]
    fn malformed_records_are_not_treated_as_absence() {
        let profile = tempfile::tempdir().unwrap();
        let store = Store::open(profile.path()).unwrap();
        let id = identity();
        store.create(id.clone(), 1).unwrap();
        fs::write(store.path(&id.job_id), b"broken").unwrap();
        assert!(store.create(id.clone(), 2).is_err());
        assert!(store.read(&id.job_id).is_err());
        assert_eq!(fs::read(store.path(&id.job_id)).unwrap(), b"broken");
    }

    #[test]
    fn reading_unknown_job_does_not_create_lock_or_record() {
        let profile = tempfile::tempdir().unwrap();
        let store = Store::open(profile.path()).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        assert!(store.read(&id).is_err());
        assert_eq!(fs::read_dir(store.root.join("locks")).unwrap().count(), 0);
        assert!(!store.path(&id).exists());
    }

    #[test]
    fn link_compensation_cannot_cancel_an_advanced_job() {
        let profile = tempfile::tempdir().unwrap();
        let store = Store::open(profile.path()).unwrap();
        let id = identity();
        store.create(id.clone(), 1).unwrap();
        let stage = uuid::Uuid::new_v4().to_string();
        store.building(&id, &stage, 2).unwrap();
        assert_eq!(
            store.abandon_unlinked(&id, 3).unwrap().state,
            State::Building {
                stage_id: stage.clone()
            }
        );
        store.publication_intent(&id, 4).unwrap();
        assert_eq!(
            store.abandon_unlinked(&id, 5).unwrap().state,
            State::Publishing { stage_id: stage }
        );
    }

    #[test]
    fn wrong_operation_or_unpublished_status_cannot_establish_success() {
        let profile = tempfile::tempdir().unwrap();
        let store = Store::open(profile.path()).unwrap();
        for wrong_operation in [true, false] {
            let id = identity();
            store.create(id.clone(), 1).unwrap();
            store
                .building(&id, &uuid::Uuid::new_v4().to_string(), 2)
                .unwrap();
            store.publication_intent(&id, 3).unwrap();
            let mut graph = observed(&id);
            if wrong_operation {
                graph.created_by_operation_id = Some("another-operation".into());
            } else {
                graph.status = "staging".into();
            }
            assert!(matches!(
                store
                    .observe_publication(&id, Some(&graph), 4)
                    .unwrap()
                    .state,
                State::Uncertain { .. }
            ));
        }
    }

    #[test]
    fn snapshot_hardlinks_remain_readable_and_are_not_modified_by_updates() {
        let profile = tempfile::tempdir().unwrap();
        let store = Store::open(profile.path()).unwrap();
        let id = identity();
        store.create(id.clone(), 1).unwrap();
        let snapshot = profile.path().join("snapshot-record.json");
        fs::hard_link(store.path(&id.job_id), &snapshot).unwrap();
        let before = fs::read(&snapshot).unwrap();
        assert_eq!(store.read(&id.job_id).unwrap().state, State::Accepted);
        store
            .building(&id, &uuid::Uuid::new_v4().to_string(), 2)
            .unwrap();
        assert_eq!(fs::read(&snapshot).unwrap(), before);
        assert!(matches!(
            store.read(&id.job_id).unwrap().state,
            State::Building { .. }
        ));
    }
    #[cfg(unix)]
    #[test]
    fn linked_store_or_record_is_refused() {
        use std::os::unix::fs::symlink;
        let profile = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        symlink(other.path(), profile.path().join(".vault-jobs")).unwrap();
        assert!(Store::open(profile.path()).is_err());
        fs::remove_file(profile.path().join(".vault-jobs")).unwrap();
        let store = Store::open(profile.path()).unwrap();
        let id = identity();
        store.create(id.clone(), 1).unwrap();
        fs::remove_file(store.path(&id.job_id)).unwrap();
        let sentinel = other.path().join("sentinel");
        fs::write(&sentinel, b"keep").unwrap();
        symlink(&sentinel, store.path(&id.job_id)).unwrap();
        assert!(store.read(&id.job_id).is_err());
        assert!(store.create(id, 2).is_err());
        assert_eq!(fs::read(sentinel).unwrap(), b"keep");
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum State {
    Accepted,
    Building { stage_id: String },
    Publishing { stage_id: String },
    Published,
    Uncertain { stage_id: String },
    Abandoned { stage_id: Option<String> },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Record {
    version: u32,
    pub identity: Identity,
    pub state: State,
    pub created_at: u64,
    pub updated_at: u64,
}

fn uuid(raw: &str) -> Result<(), String> {
    if uuid::Uuid::parse_str(raw)
        .map_err(|_| "invalid job UUID")?
        .to_string()
        != raw
    {
        return Err("noncanonical job UUID".into());
    }
    Ok(())
}
impl Identity {
    fn validate(&self) -> Result<(), String> {
        for id in [
            &self.job_id,
            &self.scan_id,
            &self.destination_incarnation,
            &self.source.incarnation,
        ] {
            uuid(id)?;
        }
        for id in [
            &self.destination_graph,
            &self.source.graph,
            &self.operation_id,
        ] {
            crate::ids::validate_local_id(id, "job identity")?;
        }
        if self.source.principal.is_empty()
            || self.source.principal.len() > 256
            || self.source.principal.chars().any(char::is_control)
        {
            return Err("invalid job principal".into());
        }
        for digest in [
            &self.capture_fingerprint,
            &self.plan_digest,
            &self.preparation_digest,
        ] {
            if digest.len() != 64
                || !digest
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return Err("invalid job digest".into());
            }
        }
        Ok(())
    }
}

pub(crate) struct Store {
    root: PathBuf,
}

fn directory(path: &Path) -> Result<(), String> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.to_string()),
    }
    if !fs::symlink_metadata(path)
        .map_err(|e| e.to_string())?
        .is_dir()
    {
        return Err("job store directory is linked or non-directory".into());
    }
    crate::storage_atomic::sync_parent_dir(path.parent().ok_or("job directory has no parent")?)?;
    Ok(())
}
fn open(path: &Path, create: bool) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.read(true).write(create).create(create);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    let file = options.open(path).map_err(|e| e.to_string())?;
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_file() {
        return Err("job store entry is not regular".into());
    }
    // Profile snapshots hard-link atomic-replace JSON records. Reading those
    // inodes is safe; updates replace the name, never modify a snapshot inode.
    Ok(file)
}

impl Store {
    pub(crate) fn require_profile(&self, profile: &Path) -> Result<(), String> {
        if self.root
            != fs::canonicalize(profile)
                .map_err(|e| e.to_string())?
                .join(".vault-jobs")
        {
            return Err("job store belongs to another profile".into());
        }
        Ok(())
    }
    pub(crate) fn open(profile: &Path) -> Result<Self, String> {
        if !cfg!(any(target_os = "macos", target_os = "linux")) {
            return Err("vault job file safety is not verified on this platform".into());
        }
        let root = fs::canonicalize(profile)
            .map_err(|e| e.to_string())?
            .join(".vault-jobs");
        let _durability = crate::cell_durability::write_guard();
        directory(&root)?;
        directory(&root.join("locks"))?;
        Ok(Self { root })
    }
    fn lock(&self, id: &str, create: bool) -> Result<File, String> {
        uuid(id)?;
        let file = open(&self.root.join("locks").join(format!("{id}.lock")), create)?;
        if create {
            file.sync_all().map_err(|e| e.to_string())?;
            crate::storage_atomic::sync_parent_dir(&self.root.join("locks"))?;
        }
        file.lock_exclusive().map_err(|e| e.to_string())?;
        Ok(file)
    }
    fn path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.json"))
    }
    fn read_locked(&self, id: &str) -> Result<Record, String> {
        let file = open(&self.path(id), false)?;
        let mut bytes = vec![];
        file.take(65537)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        if bytes.len() > 65536 {
            return Err("job record exceeds bound".into());
        }
        let record: Record = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        record.identity.validate()?;
        if record.version != 1
            || record.identity.job_id != id
            || record.updated_at < record.created_at
            || record.created_at >= record.identity.consent_expires_at
        {
            return Err("invalid job record".into());
        }
        match &record.state {
            State::Building { stage_id }
            | State::Publishing { stage_id }
            | State::Uncertain { stage_id } => uuid(stage_id)?,
            State::Abandoned {
                stage_id: Some(stage_id),
            } => uuid(stage_id)?,
            _ => {}
        }
        Ok(record)
    }
    fn write_locked(&self, record: &Record) -> Result<(), String> {
        crate::storage::write_json(&self.path(&record.identity.job_id), record)
            .map_err(|e| e.to_string())
    }
    pub(crate) fn create(&self, identity: Identity, now: u64) -> Result<Record, String> {
        identity.validate()?;
        let _durability = crate::cell_durability::write_guard();
        let index = open(&self.root.join("locks/index.lock"), true)?;
        index.lock_exclusive().map_err(|e| e.to_string())?;
        match fs::symlink_metadata(self.path(&identity.job_id)) {
            Ok(_) => {
                let _lock = self.lock(&identity.job_id, true)?;
                let record = self.read_locked(&identity.job_id)?;
                if record.identity != identity {
                    return Err("job identity conflict".into());
                }
                return Ok(record);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()),
        }
        if now >= identity.consent_expires_at {
            return Err("source consent expired".into());
        }
        // A ceiling, not eviction: old historical outcomes are never forgotten
        // merely to permit another job with uncertain replay history.
        if fs::read_dir(&self.root)
            .map_err(|e| e.to_string())?
            .take(4097)
            .count()
            >= 4097
        {
            return Err("job history capacity reached".into());
        }
        let record = Record {
            version: 1,
            identity,
            state: State::Accepted,
            created_at: now,
            updated_at: now,
        };
        let _lock = self.lock(&record.identity.job_id, true)?;
        self.write_locked(&record)?;
        Ok(record)
    }
    pub(crate) fn read(&self, id: &str) -> Result<Record, String> {
        let _lock = self.lock(id, false)?;
        self.read_locked(id)
    }
    pub(crate) fn recent_owner(&self, limit: usize) -> Result<(Vec<Record>, usize), String> {
        if !(1..=50).contains(&limit) {
            return Err("job history limit must be 1 through 50".into());
        }
        let mut rows = vec![];
        let mut total = 0usize;
        for (n, entry) in fs::read_dir(&self.root)
            .map_err(|e| e.to_string())?
            .enumerate()
        {
            if n >= 4096 {
                return Err("job history inventory exceeds bound".into());
            }
            let entry = entry.map_err(|e| e.to_string())?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let id = path
                .file_stem()
                .and_then(|s| s.to_str())
                .ok_or("invalid history entry")?;
            let record = self.read(id)?;
            if record.identity.source.principal != "human:owner" {
                continue;
            }
            total += 1;
            rows.push(record);
            rows.sort_by(|a, b| {
                b.updated_at
                    .cmp(&a.updated_at)
                    .then_with(|| a.identity.job_id.cmp(&b.identity.job_id))
            });
            rows.truncate(limit);
        }
        Ok((rows, total))
    }
    fn change(
        &self,
        identity: &Identity,
        now: u64,
        f: impl FnOnce(&Record) -> Result<State, String>,
    ) -> Result<Record, String> {
        identity.validate()?;
        let _durability = crate::cell_durability::write_guard();
        let _lock = self.lock(&identity.job_id, false)?;
        let mut record = self.read_locked(&identity.job_id)?;
        if record.identity != *identity {
            return Err("job identity conflict".into());
        }
        if now < record.updated_at {
            return Err("job clock moved backwards".into());
        }
        record.state = f(&record)?;
        record.updated_at = now;
        self.write_locked(&record)?;
        Ok(record)
    }
    pub(crate) fn building(
        &self,
        identity: &Identity,
        stage_id: &str,
        now: u64,
    ) -> Result<Record, String> {
        uuid(stage_id)?;
        self.change(identity, now, |r| match &r.state {
            State::Accepted if now < identity.consent_expires_at => Ok(State::Building {
                stage_id: stage_id.into(),
            }),
            State::Building { stage_id: old } if old == stage_id => Ok(r.state.clone()),
            _ => Err("job cannot begin or replace stage".into()),
        })
    }
    pub(crate) fn publication_intent(
        &self,
        identity: &Identity,
        now: u64,
    ) -> Result<Record, String> {
        self.change(identity, now, |r| match &r.state {
            State::Building { stage_id } if now < identity.consent_expires_at => {
                Ok(State::Publishing {
                    stage_id: stage_id.clone(),
                })
            }
            _ => Err("job cannot attempt publication".into()),
        })
    }
    /// Caller reads canonical destination while holding its publication lock.
    /// A missing/mismatched destination after intent is never permission to retry.
    pub(crate) fn observe_publication(
        &self,
        identity: &Identity,
        observed: Option<&GraphRecord>,
        now: u64,
    ) -> Result<Record, String> {
        self.change(identity, now, |r| {
            if r.state == State::Published {
                return Ok(State::Published);
            }
            let stage_id = match &r.state {
                State::Publishing { stage_id } | State::Uncertain { stage_id } => stage_id,
                _ => return Err("no publication intent to reconcile".into()),
            };
            let matches = observed.is_some_and(|g| {
                matches!(
                    g.status.as_str(),
                    crate::runtime_config::GRAPH_STATUS_ACTIVE
                        | crate::runtime_config::GRAPH_STATUS_DELETED
                ) && g.graph_id == identity.destination_graph
                    && g.incarnation_id.as_deref()
                        == Some(identity.destination_incarnation.as_str())
                    && g.created_by_operation_id.as_deref() == Some(identity.operation_id.as_str())
            });
            Ok(if matches {
                State::Published
            } else {
                State::Uncertain {
                    stage_id: stage_id.clone(),
                }
            })
        })
    }
    pub(crate) fn abandon(&self, identity: &Identity, now: u64) -> Result<Record, String> {
        self.change(identity, now, |r| match &r.state {
            State::Accepted => Ok(State::Abandoned { stage_id: None }),
            State::Building { stage_id } => Ok(State::Abandoned {
                stage_id: Some(stage_id.clone()),
            }),
            State::Abandoned { .. } => Ok(r.state.clone()),
            _ => Err("publication may have happened; cannot report cancellation".into()),
        })
    }
    /// Compensate a failed capture link without cancelling a job that already
    /// advanced. The capture guard is held by the acceptance caller.
    pub(crate) fn abandon_unlinked(&self, identity: &Identity, now: u64) -> Result<Record, String> {
        self.change(identity, now, |record| {
            Ok(match record.state {
                State::Accepted => State::Abandoned { stage_id: None },
                _ => record.state.clone(),
            })
        })
    }
}
