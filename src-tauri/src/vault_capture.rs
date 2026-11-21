//! Private, profile-owned capture state. No request may supply this store's paths.
//! Native/API adapters and document commit are deliberately not wired up yet.
use crate::{
    profile_lock::ProfileLock,
    storage_atomic, storage_file_ops,
    vault_preflight::{Kind, Limits},
    vault_staging::{self, Snapshot},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
};
use uuid::Uuid;
type Result<T> = std::result::Result<T, String>;
const MAX_TTL: u64 = 60 * 60 * 1000;
const MAX_RECORDS: usize = 64;
const MAX_READY_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_JSON: u64 = 16 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Binding {
    pub principal: String,
    pub graph: String,
    pub incarnation: String,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub(crate) enum SourceKind {
    Folder,
    Zip,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Grant {
    version: u32,
    id: String,
    binding: Binding,
    expires: u64,
    revoked: bool,
    scan: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum State {
    Capturing,
    Ready,
    Cancelled,
    Failed,
    Expired,
    Revoked,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CapturedEntry {
    pub path: String,
    pub kind: String,
    pub bytes: u64,
    pub sha256: String,
    payload: Option<String>,
}

/// A frozen description, not ongoing permission. Only this store constructs it;
/// admission must recheck the live grant/receipt while holding the store mutex.
#[derive(Clone)]
pub(crate) struct EntryPermit {
    binding: Binding,
    scan: String,
    grant: String,
    index: usize,
    fingerprint: String,
    captured_at: u64,
    entry: CapturedEntry,
}
impl EntryPermit {
    pub(crate) fn binding(&self) -> &Binding { &self.binding }
    pub(crate) fn scan(&self) -> &str { &self.scan }
    pub(crate) fn index(&self) -> usize { self.index }
    pub(crate) fn fingerprint(&self) -> &str { &self.fingerprint }
    pub(crate) fn captured_at(&self) -> u64 { self.captured_at }
    pub(crate) fn entry(&self) -> &CapturedEntry { &self.entry }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Receipt {
    version: u32,
    pub scan_id: String,
    grant_id: String,
    pub binding: Binding,
    pub state: State,
    pub started: u64,
    pub finished: Option<u64>,
    pub expires: u64,
    pub entries: Vec<CapturedEntry>,
    pub fingerprint: Option<String>,
    pub error: Option<String>,
    #[serde(default)]
    pub committed_job: Option<CommittedJob>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CommittedJob {
    pub job_id: String,
    pub identity_digest: String,
}

pub(crate) struct Ticket {
    excluded_profile: Option<(u64, u64)>,
    scan_id: String,
    source: File,
    kind: SourceKind,
    expires: u64,
    cancelled: Arc<AtomicBool>,
    in_flight: Arc<AtomicUsize>,
    _lease: Arc<()>,
}
impl Drop for Ticket {
    fn drop(&mut self) {
        self.in_flight.fetch_sub(1, Ordering::AcqRel);
    }
}
impl Ticket {
    pub(crate) fn capture(&mut self) -> Result<Snapshot> {
        wall_now()?;
        self.capture_at(|| wall_now().unwrap_or(u64::MAX))
    }
    pub(crate) fn id(&self) -> &str {
        &self.scan_id
    }
    pub(crate) fn cancellation(&self) -> Arc<AtomicBool> {
        self.cancelled.clone()
    }
    /// Runs outside the store's lock. Dropping a ticket leaves a recoverable
    /// interrupted receipt; it never reopens a filename or restarts capture.
    fn capture_at(&mut self, now: impl Fn() -> u64) -> Result<Snapshot> {
        let cancelled = || self.cancelled.load(Ordering::Acquire) || now() >= self.expires;
        match self.kind {
            SourceKind::Folder => {
                vault_staging::stage_folder_excluding(&self.source, Limits::default(), cancelled, self.excluded_profile)
            }
            SourceKind::Zip => {
                vault_staging::stage_zip(&mut self.source, Limits::default(), cancelled)
            }
        }
    }
}

pub(crate) struct CaptureStore {
    root: PathBuf,
    _lock: ProfileLock,
    sources: HashMap<String, (File, SourceKind)>,
    active: HashMap<String, Arc<AtomicBool>>,
    leases: HashMap<String, std::sync::Weak<()>>,
    warnings: Vec<String>,
    in_flight: Arc<AtomicUsize>,
}
fn uuid(id: &str) -> Result<()> {
    if Uuid::parse_str(id)
        .map_err(|_| "invalid capture id")?
        .to_string()
        != id
    {
        return Err("noncanonical capture id".into());
    }
    Ok(())
}
fn open_regular(path: &Path) -> Result<File> {
    let mut opts = OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        opts.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        let file = opts.open(path).map_err(|e| e.to_string())?;
        let meta = file.metadata().map_err(|e| e.to_string())?;
        if !meta.is_file() || meta.nlink() != 1 {
            return Err("capture file is not a private regular file".into());
        }
        Ok(file)
    }
    #[cfg(not(unix))]
    {
        let _ = opts;
        Err("capture store platform not verified".into())
    }
}
fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let mut bytes = vec![];
    open_regular(path)?
        .take(MAX_JSON + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_JSON {
        return Err("capture metadata limit exceeded".into());
    }
    serde_json::from_slice(&bytes).map_err(|e| e.to_string())
}
fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_JSON {
        return Err("capture metadata limit exceeded".into());
    }
    storage_file_ops::write_secret_bytes(path, &bytes).map_err(|e| e.to_string())
}
fn fingerprint(entries: &[CapturedEntry]) -> Result<String> {
    let material: Vec<_> = entries
        .iter()
        .map(|e| (&e.path, &e.kind, e.bytes, &e.sha256))
        .collect();
    let mut hash = Sha256::new();
    hash.update(b"vault-capture-v1\0");
    hash.update(serde_json::to_vec(&material).map_err(|e| e.to_string())?);
    Ok(format!("{:x}", hash.finalize()))
}
impl CaptureStore {
    /// The existing application profile lock must already be held by the owner.
    /// This second lock prevents concurrent capture-store recovery/instances.
    /// The private profile is trusted against same-user malicious replacement;
    /// unlike source paths it is never accepted from an MCP request.
    fn open_at(profile: &Path, now: u64) -> Result<Self> {
        if !cfg!(any(target_os = "macos", target_os = "linux")) {
            return Err("durable capture is not verified on this platform".into());
        }
        let _guard = crate::cell_durability::write_guard();
        let root = profile.join("vault-capture");
        if !root.exists() {
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(&root).map_err(|e| e.to_string())?;
            storage_atomic::sync_parent_dir(profile)?;
        }
        let meta = fs::symlink_metadata(&root).map_err(|e| e.to_string())?;
        if !meta.is_dir() {
            return Err("capture root must be an owned directory".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if meta.mode() & 0o077 != 0 {
                return Err("capture root permissions must be private".into());
            }
        }
        let lock = ProfileLock::acquire(&root)?;
        let mut store = Self {
            root,
            _lock: lock,
            sources: HashMap::new(),
            active: HashMap::new(),
            leases: HashMap::new(),
            warnings: vec![],
            in_flight: Arc::new(AtomicUsize::new(0)),
        };
        // This is the boot-only recovery path; never call it during active work.
        store.recover_inner(now)?;
        Ok(store)
    }
    fn path(&self, id: &str, suffix: &str) -> Result<PathBuf> {
        uuid(id)?;
        Ok(self.root.join(format!("{id}.{suffix}")))
    }
    fn grant(&self, id: &str) -> Result<Grant> {
        let g: Grant = read_json(&self.path(id, "grant")?)?;
        if g.version != 1 || g.id != id {
            return Err("invalid grant record".into());
        }
        Ok(g)
    }
    fn receipt(&self, id: &str) -> Result<Receipt> {
        let r: Receipt = read_json(&self.path(id, "scan")?)?;
        if r.version != 1 || r.scan_id != id {
            return Err("invalid receipt".into());
        }
        uuid(&r.grant_id)?;
        Ok(r)
    }
    fn ids(&self, suffix: &str) -> Result<Vec<String>> {
        let mut ids = vec![];
        for e in fs::read_dir(&self.root).map_err(|e| e.to_string())? {
            let name = e.map_err(|e| e.to_string())?.file_name();
            let name = name.to_string_lossy();
            if let Some(id) = name.strip_suffix(&format!(".{suffix}")) {
                if uuid(id).is_err() {
                    continue;
                }
                ids.push(id.to_owned());
            }
        }
        Ok(ids)
    }
    fn authorize(&self, g: &Grant, b: &Binding, now: u64) -> Result<()> {
        if &g.binding != b {
            return Err("capture binding mismatch".into());
        }
        if g.revoked {
            return Err("capture grant revoked".into());
        }
        if now >= g.expires {
            return Err("capture grant expired".into());
        }
        Ok(())
    }
    /// Only the native owner selection adapter may call this with a freshly
    /// authorized handle. Possessing graphs.import does not authorize selection.
    fn grant_selected_at(
        &mut self,
        binding: Binding,
        file: File,
        kind: SourceKind,
        now: u64,
        ttl: u64,
    ) -> Result<String> {
        self.expire_at(now)?;
        if ttl == 0
            || ttl > MAX_TTL
            || binding.principal.is_empty()
            || binding.graph.is_empty()
            || binding.incarnation.is_empty()
            || [&binding.principal, &binding.graph, &binding.incarnation]
                .iter()
                .any(|s| s.len() > 256)
        {
            return Err("invalid grant binding or lifetime".into());
        }
        if self.ids("grant")?.len() >= MAX_RECORDS {
            return Err("capture grant capacity reached; expire old grants".into());
        }
        let meta = file.metadata().map_err(|e| e.to_string())?;
        if !match kind {
            SourceKind::Folder => meta.is_dir(),
            SourceKind::Zip => meta.is_file(),
        } {
            return Err("selected source kind mismatch".into());
        }
        let id = Uuid::new_v4().to_string();
        let grant = Grant {
            version: 1,
            id: id.clone(),
            binding,
            expires: now.checked_add(ttl).ok_or("expiry overflow")?,
            revoked: false,
            scan: None,
        };
        let _guard = crate::cell_durability::write_guard();
        write_json(&self.path(&id, "grant")?, &grant)?;
        self.sources.insert(id.clone(), (file, kind));
        Ok(id)
    }
    fn begin_at(&mut self, id: &str, b: &Binding, now: u64) -> Result<Ticket> {
        #[cfg(unix)]
        let excluded_profile = {
            use std::os::unix::fs::MetadataExt;
            let metadata = fs::metadata(self.root.parent().ok_or("capture profile unavailable")?).map_err(|e| e.to_string())?;
            Some((metadata.dev(), metadata.ino()))
        };
        #[cfg(not(unix))]
        let excluded_profile = None;
        self.expire_at(now)?;
        let mut g = self.grant(id)?;
        self.authorize(&g, b, now)?;
        if g.scan.is_some() {
            return Err("grant already used".into());
        }
        if !self.active.is_empty() || self.in_flight.load(Ordering::Acquire) != 0 {
            return Err("another capture is active".into());
        }
        if !self.sources.contains_key(id) {
            return Err("source capability lost; select the source again".into());
        }
        let scan_id = Uuid::new_v4().to_string();
        g.scan = Some(scan_id.clone());
        let receipt = Receipt {
            version: 1,
            scan_id: scan_id.clone(),
            grant_id: id.to_owned(),
            binding: b.clone(),
            state: State::Capturing,
            started: now,
            finished: None,
            expires: g.expires,
            entries: vec![],
            fingerprint: None,
            error: None,
            committed_job: None,
        };
        let _guard = crate::cell_durability::write_guard();
        write_json(&self.path(id, "grant")?, &g)?;
        write_json(&self.path(&scan_id, "scan")?, &receipt)?;
        let (source, kind) = self.sources.remove(id).ok_or("source capability lost")?;
        let cancelled = Arc::new(AtomicBool::new(false));
        self.active.insert(scan_id.clone(), cancelled.clone());
        self.in_flight.fetch_add(1, Ordering::AcqRel);
        let lease = Arc::new(());
        self.leases.insert(scan_id.clone(), Arc::downgrade(&lease));
        Ok(Ticket {
            excluded_profile,
            scan_id,
            source,
            kind,
            expires: g.expires,
            cancelled,
            in_flight: self.in_flight.clone(),
            _lease: lease,
        })
    }
    fn check_ready(&self, r: &Receipt, b: &Binding, now: u64) -> Result<()> {
        let g = self.grant(&r.grant_id)?;
        self.authorize(&g, b, now)?;
        if r.binding != *b || g.scan.as_deref() != Some(&r.scan_id) || r.expires != g.expires {
            return Err("receipt binding mismatch".into());
        }
        Ok(())
    }
    fn finish_at(
        &mut self,
        ticket: Ticket,
        b: &Binding,
        capture: Result<Snapshot>,
        now: impl Fn() -> u64,
    ) -> Result<Receipt> {
        let mut r = self.receipt(ticket.id())?;
        if r.binding != *b {
            return Err("capture binding mismatch".into());
        }
        if r.state != State::Capturing {
            return Err("capture is already terminal".into());
        }
        let flag = self
            .active
            .get(ticket.id())
            .ok_or("ticket does not belong to this process")?;
        if !Arc::ptr_eq(flag, &ticket.cancelled) {
            return Err("ticket identity mismatch".into());
        }
        let _guard = crate::cell_durability::write_guard();
        let result: Result<()> = (|| {
            self.check_ready(&r, b, now())?;
            let mut snapshot = capture?;
            let wanted = snapshot.entries().iter().try_fold(0u64, |n, e| {
                n.checked_add(e.bytes).ok_or("capture quota overflow")
            })?;
            let used = self
                .ids("scan")?
                .into_iter()
                .try_fold(0u64, |sum, id| -> Result<u64> {
                    let other = self.receipt(&id)?;
                    let bytes = if other.state == State::Ready {
                        other.entries.iter().try_fold(0u64, |n, e| {
                            n.checked_add(e.bytes).ok_or("capture quota overflow")
                        })?
                    } else {
                        0
                    };
                    sum.checked_add(bytes)
                        .ok_or("capture quota overflow".into())
                })?;
            if used
                .checked_add(wanted)
                .filter(|&n| n <= MAX_READY_BYTES)
                .is_none()
            {
                return Err("capture disk quota exceeded".into());
            }
            let mut indexes: Vec<_> = (0..snapshot.entries().len()).collect();
            indexes.sort_by(|&a, &b| snapshot.entries()[a].path.cmp(&snapshot.entries()[b].path));
            for (number, index) in indexes.into_iter().enumerate() {
                if ticket.cancelled.load(Ordering::Acquire) {
                    return Err("capture cancelled".into());
                }
                self.check_ready(&r, b, now())?;
                let e = &snapshot.entries()[index];
                let mut entry = CapturedEntry {
                    path: e.path.clone(),
                    kind: match e.kind {
                        Kind::Directory => "directory",
                        Kind::Note => "note",
                        Kind::Attachment => "attachment",
                    }
                    .into(),
                    bytes: e.bytes,
                    sha256: if e.kind == Kind::Directory {
                        String::new()
                    } else {
                        e.sha256.clone()
                    },
                    payload: None,
                };
                if e.kind != Kind::Directory {
                    let name = format!("{}-{number}.blob", r.scan_id);
                    let prefix = format!(".pending-{}-", r.scan_id);
                    let mut temp = tempfile::Builder::new()
                        .prefix(&prefix)
                        .tempfile_in(&self.root)
                        .map_err(|e| e.to_string())?;
                    let mut writer = CaptureWriter {
                        file: temp.as_file_mut(),
                        hash: Sha256::new(),
                        cancelled: &ticket.cancelled,
                        expires: r.expires,
                        now: &now,
                    };
                    snapshot.copy_entry(index, &mut writer)?;
                    if format!("{:x}", writer.hash.finalize()) != entry.sha256 {
                        return Err("captured hash mismatch".into());
                    }
                    temp.as_file().sync_all().map_err(|e| e.to_string())?;
                    temp.persist_noclobber(self.root.join(&name))
                        .map_err(|e| e.to_string())?;
                    entry.payload = Some(name);
                }
                r.entries.push(entry);
            }
            storage_atomic::sync_parent_dir(&self.root)?;
            self.check_ready(&r, b, now())?;
            if ticket.cancelled.load(Ordering::Acquire) {
                return Err("capture cancelled".into());
            }
            r.fingerprint = Some(fingerprint(&r.entries)?);
            r.finished = Some(now());
            r.state = State::Ready;
            write_json(&self.path(&r.scan_id, "scan")?, &r)?;
            Ok(())
        })();
        self.active.remove(&r.scan_id);
        if let Err(error) = result {
            r.state = if now() >= r.expires {
                State::Expired
            } else if self.grant(&r.grant_id).map(|g| g.revoked).unwrap_or(true) {
                State::Revoked
            } else if ticket.cancelled.load(Ordering::Acquire) {
                State::Cancelled
            } else {
                State::Failed
            };
            r.error = Some(error.chars().take(512).collect());
            r.finished = Some(now());
            r.entries.clear();
            r.fingerprint = None;
            // Terminal receipt first: a crash during cleanup cannot resurrect it.
            let recorded = write_json(&self.path(&r.scan_id, "scan")?, &r);
            let cleaned = self.purge(&r.scan_id);
            recorded?;
            cleaned?;
        }
        Ok(r)
    }
    fn get_ready_at(&mut self, id: &str, b: &Binding, now: u64) -> Result<Receipt> {
        self.expire_at(now)?;
        let r = self.receipt(id)?;
        self.check_ready(&r, b, now)?;
        if r.state != State::Ready {
            return Err("capture is not ready".into());
        }
        if r.fingerprint.as_deref() != Some(&fingerprint(&r.entries)?) {
            return Err("capture manifest corrupt".into());
        }
        self.validate_payloads(&r, false)?;
        Ok(r)
    }
    /// Internal file capability for a later commit; caller must recheck grant,
    /// graph lifecycle and mutation scopes at commit admission, not just here.
    fn open_entry_at(
        &mut self,
        id: &str,
        index: usize,
        b: &Binding,
        now: impl Fn() -> u64,
    ) -> Result<File> {
        let r = self.get_ready_at(id, b, now())?;
        let e = r.entries.get(index).ok_or("unknown captured entry")?;
        let name = e.payload.as_ref().ok_or("entry has no payload")?;
        if name != &format!("{id}-{index}.blob") {
            return Err("invalid payload identity".into());
        }
        let mut file = open_regular(&self.root.join(name))?;
        if file.metadata().map_err(|e| e.to_string())?.len() != e.bytes {
            return Err("capture size mismatch".into());
        }
        let mut hash = Sha256::new();
        let mut buf = [0; 64 * 1024];
        let mut count = 0u64;
        loop {
            let n = file.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            count += n as u64;
            if count > e.bytes {
                return Err("capture grew".into());
            }
            hash.update(&buf[..n]);
        }
        if count != e.bytes || format!("{:x}", hash.finalize()) != e.sha256 {
            return Err("capture content corrupt".into());
        }
        self.check_ready(&r, b, now())?;
        file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
        Ok(file)
    }
    /// Owner-side revocation. Adapters must authorize this control-plane action.
    fn revoke_at(&mut self, id: &str, now: u64) -> Result<()> {
        let mut g = self.grant(id)?;
        if let Some(scan) = &g.scan {
            if let Some(flag) = self.active.get(scan) {
                flag.store(true, Ordering::Release);
            }
        }
        let _guard = crate::cell_durability::write_guard();
        g.revoked = true;
        write_json(&self.path(id, "grant")?, &g)?;
        self.sources.remove(id);
        if let Some(scan) = g.scan {
            // A crash may have consumed the grant before creating its receipt,
            // or removed an expired receipt before removing the grant record.
            let recorded = (|| -> Result<()> {
                if self
                    .path(&scan, "scan")?
                    .try_exists()
                    .map_err(|e| e.to_string())?
                {
                    let mut r = match self.receipt(&scan) {
                        Ok(r) => r,
                        Err(error) => {
                            self.warn(error);
                            return self.quarantine(&scan, "scan");
                        }
                    };
                    r.state = State::Revoked;
                    r.finished = Some(now);
                    r.entries.clear();
                    r.fingerprint = None;
                    write_json(&self.path(&scan, "scan")?, &r)?;
                }
                Ok(())
            })();
            let cleaned = self.purge(&scan);
            self.active.remove(&scan);
            self.leases.remove(&scan);
            recorded?;
            cleaned?;
        }
        Ok(())
    }
    fn cancel_at(&mut self, id: &str, binding: &Binding, now: u64) -> Result<Receipt> {
        let mut r = self.receipt(id)?;
        if &r.binding != binding {
            return Err("capture binding mismatch".into());
        }
        if !matches!(r.state, State::Capturing | State::Ready) {
            return Ok(r);
        }
        if let Some(flag) = self.active.get(id) {
            flag.store(true, Ordering::Release);
        }
        let _guard = crate::cell_durability::write_guard();
        r.state = State::Cancelled;
        r.finished = Some(now);
        r.entries.clear();
        r.fingerprint = None;
        write_json(&self.path(id, "scan")?, &r)?;
        self.purge(id)?;
        self.active.remove(id);
        Ok(r)
    }
    fn purge(&self, id: &str) -> Result<()> {
        uuid(id)?;
        let mut failure = None;
        for e in fs::read_dir(&self.root).map_err(|e| e.to_string())? {
            let e = e.map_err(|e| e.to_string())?;
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(&format!("{id}-")) && name.ends_with(".blob")
                || name.starts_with(&format!(".pending-{id}-"))
            {
                // Never recurse or follow a link. These names are generated in
                // the private profile, not taken from the source manifest.
                if let Err(e) = fs::remove_file(e.path()) {
                    failure = Some(e.to_string());
                }
            }
        }
        let synced = storage_atomic::sync_parent_dir(&self.root);
        if let Some(error) = failure {
            Err(error)
        } else {
            synced
        }
    }
    fn warn(&mut self, error: impl Into<String>) {
        if self.warnings.len() < 128 {
            self.warnings.push(error.into());
        }
    }
    pub(crate) fn warnings(&self) -> &[String] {
        &self.warnings
    }
    pub(crate) fn has_records(&self) -> Result<bool> {
        Ok(!self.ids("grant")?.is_empty() || !self.ids("scan")?.is_empty())
    }
    pub(crate) fn has_unrevoked_grant(&self, graph: &str, incarnation: &str) -> Result<bool> {
        for id in self.ids("grant")? {
            let grant = self.grant(&id)?;
            if !grant.revoked && grant.binding.graph == graph && grant.binding.incarnation == incarnation { return Ok(true); }
        }
        Ok(false)
    }
    fn quarantine(&mut self, id: &str, suffix: &str) -> Result<()> {
        let _guard = crate::cell_durability::write_guard();
        let source = self.path(id, suffix)?;
        if source.try_exists().map_err(|e| e.to_string())? {
            fs::rename(
                &source,
                self.root
                    .join(format!(".bad-{suffix}-{id}-{}", Uuid::new_v4())),
            )
            .map_err(|e| e.to_string())?;
            storage_atomic::sync_parent_dir(&self.root)?;
        }
        self.warn(format!("quarantined corrupt {suffix} {id}"));
        Ok(())
    }
    fn validate_payloads(&self, r: &Receipt, hash: bool) -> Result<()> {
        if r.fingerprint.as_deref() != Some(&fingerprint(&r.entries)?) {
            return Err("capture manifest corrupt".into());
        }
        let mut admission = crate::vault_preflight::Admission::new(Limits::default());
        for (index, e) in r.entries.iter().enumerate() {
            let directory = e.kind == "directory";
            let (_, kind) = admission.admit(&e.path, directory)?;
            if e.kind
                != match kind {
                    Kind::Directory => "directory",
                    Kind::Note => "note",
                    Kind::Attachment => "attachment",
                }
            {
                return Err("invalid captured kind".into());
            }
            let mut count = 0;
            admission.account(
                kind,
                &mut count,
                usize::try_from(e.bytes).map_err(|_| "capture size overflow")?,
            )?;
            if directory {
                if e.payload.is_some() || !e.sha256.is_empty() {
                    return Err("invalid directory payload".into());
                }
                continue;
            }
            let name = format!("{}-{index}.blob", r.scan_id);
            if e.payload.as_deref() != Some(&name) {
                return Err("invalid payload identity".into());
            }
            let mut f = open_regular(&self.root.join(name))?;
            if f.metadata().map_err(|e| e.to_string())?.len() != e.bytes {
                return Err("capture size mismatch".into());
            }
            if hash {
                let mut h = Sha256::new();
                let mut seen = 0;
                let mut buf = [0; 64 * 1024];
                loop {
                    let n = f.read(&mut buf).map_err(|e| e.to_string())?;
                    if n == 0 {
                        break;
                    }
                    seen += n as u64;
                    if seen > e.bytes {
                        return Err("capture grew".into());
                    }
                    h.update(&buf[..n]);
                }
                if seen != e.bytes || format!("{:x}", h.finalize()) != e.sha256 {
                    return Err("capture content corrupt".into());
                }
            }
        }
        Ok(())
    }
    fn recover_inner(&mut self, now: u64) -> Result<()> {
        self.reconcile(now, true)?;
        // No capture-store writer survives process recovery under the lock.
        for item in fs::read_dir(&self.root).map_err(|e| e.to_string())? {
            let item = item.map_err(|e| e.to_string())?;
            let name = item.file_name();
            let name = name.to_string_lossy();
            if let Some((head, _)) = name.strip_prefix('.').and_then(|n| n.split_once(".tmp-")) {
                let id = head
                    .strip_suffix(".scan")
                    .or_else(|| head.strip_suffix(".grant"));
                if id.is_some_and(|id| uuid(id).is_ok()) {
                    if let Err(e) = fs::remove_file(item.path()) {
                        self.warn(e.to_string());
                    }
                }
            }
        }
        storage_atomic::sync_parent_dir(&self.root)
    }
    fn reconcile(&mut self, now: u64, boot: bool) -> Result<()> {
        let _guard = crate::cell_durability::write_guard();
        for id in self.ids("scan")? {
            let mut r = match self.receipt(&id) {
                Ok(r) => r,
                Err(e) => {
                    self.warn(e);
                    if let Some(flag) = self.active.remove(&id) {
                        flag.store(true, Ordering::Release);
                    }
                    self.leases.remove(&id);
                    if let Err(e) = self.quarantine(&id, "scan") {
                        self.warn(e);
                    }
                    if let Err(e) = self.purge(&id) {
                        self.warn(e);
                    }
                    continue;
                }
            };
            let g = match self.grant(&r.grant_id) {
                Ok(g) => g,
                Err(e) => {
                    self.warn(e);
                    if let Err(e) = self.quarantine(&r.grant_id, "grant") {
                        self.warn(e);
                    }
                    r.state = State::Failed;
                    r.error = Some("capture grant unavailable".into());
                    r.finished = Some(now);
                    r.entries.clear();
                    r.fingerprint = None;
                    let marked = write_json(&self.path(&id, "scan")?, &r);
                    let purged = self.purge(&id);
                    if let Err(e) = marked {
                        self.warn(e);
                    }
                    if let Err(e) = purged {
                        self.warn(e);
                    }
                    if let Some(flag) = self.active.remove(&id) {
                        flag.store(true, Ordering::Release);
                    }
                    self.leases.remove(&id);
                    self.sources.remove(&r.grant_id);
                    continue;
                }
            };
            let alive = self.leases.get(&id).and_then(|l| l.upgrade()).is_some();
            let state = if g.scan.as_deref() != Some(&id)
                || g.binding != r.binding
                || g.expires != r.expires
            {
                State::Failed
            } else if g.revoked {
                State::Revoked
            } else if now >= g.expires {
                State::Expired
            } else if (r.state == State::Capturing && !alive)
                || (r.state == State::Ready && self.validate_payloads(&r, boot).is_err())
            {
                State::Failed
            } else {
                r.state.clone()
            };
            if !matches!(state, State::Ready | State::Capturing) {
                if let Some(flag) = self.active.remove(&id) {
                    flag.store(true, Ordering::Release);
                }
                self.leases.remove(&id);
                if r.state != state {
                    r.state = state;
                    r.finished = Some(now);
                    r.error = Some("capture invalidated during reconciliation".into());
                    r.entries.clear();
                    r.fingerprint = None;
                    if let Err(e) = write_json(&self.path(&id, "scan")?, &r) {
                        self.warn(e);
                    }
                }
                if let Err(e) = self.purge(&id) {
                    self.warn(e);
                }
            }
        }
        Ok(())
    }
    /// Periodic expiry; source capabilities and payloads are removed, terminal
    /// metadata retained only until its consent expires. Never touches sources.
    fn expire_at(&mut self, now: u64) -> Result<()> {
        if now == u64::MAX {
            return Err("invalid wall clock".into());
        }
        self.reconcile(now, false)?;
        for id in self.ids("grant")? {
            let g = match self.grant(&id) {
                Ok(g) => g,
                Err(e) => {
                    self.warn(e);
                    self.sources.remove(&id);
                    if let Err(e) = self.quarantine(&id, "grant") {
                        self.warn(e);
                    }
                    continue;
                }
            };
            if now < g.expires {
                continue;
            }
            if let Err(e) = self.revoke_at(&id, now) {
                self.warn(e);
                continue;
            }
            let _guard = crate::cell_durability::write_guard();
            if let Some(scan) = g.scan {
                if let Err(e) = storage_file_ops::remove_file_if_exists(&self.path(&scan, "scan")?)
                {
                    self.warn(e.to_string());
                    continue;
                }
            }
            if let Err(e) = storage_file_ops::remove_file_if_exists(&self.path(&id, "grant")?) {
                self.warn(e.to_string());
            }
        }
        Ok(())
    }
}

fn wall_ms(time: std::time::SystemTime) -> Result<u64> {
    let duration = time
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| "wall clock precedes epoch")?;
    let value = u64::try_from(duration.as_millis()).map_err(|_| "wall clock overflow")?;
    if value == u64::MAX {
        return Err("invalid wall clock".into());
    }
    Ok(value)
}
fn wall_now() -> Result<u64> {
    wall_ms(std::time::SystemTime::now())
}
impl CaptureStore {
    pub(crate) fn open(profile: &Path) -> Result<Self> {
        Self::open_at(profile, wall_now()?)
    }
    pub(crate) fn require_profile(&self, profile: &Path) -> Result<()> {
        let expected = std::fs::canonicalize(profile.join("vault-capture")).map_err(|e| e.to_string())?;
        let actual = std::fs::canonicalize(&self.root).map_err(|e| e.to_string())?;
        if expected != actual { return Err("capture store belongs to a different owner profile".into()); }
        Ok(())
    }
    pub(crate) fn attachment_for_preparation(&mut self, scan: &str, index: usize, binding: &Binding) -> Result<(EntryPermit, File)> {
        let receipt = self.get_ready(scan, binding)?;
        let entry = receipt.entries.get(index).ok_or("unknown captured entry")?.clone();
        if entry.kind != "attachment" { return Err("captured entry is not an attachment".into()); }
        let name = entry.payload.as_ref().ok_or("attachment has no payload")?;
        if name != &format!("{scan}-{index}.blob") { return Err("invalid payload identity".into()); }
        let file = open_regular(&self.root.join(name))?;
        let permit = EntryPermit {
            binding: binding.clone(), scan: scan.into(), grant: receipt.grant_id,
            index, fingerprint: receipt.fingerprint.ok_or("missing capture fingerprint")?,
            captured_at: receipt.finished.ok_or("missing capture completion time")?, entry,
        };
        // The copy worker verifies content/hash outside the cancellation mutex.
        Ok((permit, file))
    }

    pub(crate) fn authorize_prepared_attachment(&self, permit: &EntryPermit) -> Result<()> {
        let now = wall_now()?;
        let receipt = self.receipt(&permit.scan)?;
        self.check_ready(&receipt, &permit.binding, now)?;
        let current = receipt.entries.get(permit.index).ok_or("captured entry no longer exists")?;
        if receipt.state != State::Ready || receipt.grant_id != permit.grant
            || receipt.fingerprint.as_deref() != Some(permit.fingerprint.as_str())
            || fingerprint(&receipt.entries)? != permit.fingerprint
            || current.path != permit.entry.path || current.kind != permit.entry.kind
            || current.bytes != permit.entry.bytes || current.sha256 != permit.entry.sha256 {
            return Err("prepared attachment no longer matches ready capture".into());
        }
        Ok(())
    }
    pub(crate) fn grant_selected(
        &mut self,
        b: Binding,
        f: File,
        k: SourceKind,
        ttl: u64,
    ) -> Result<String> {
        self.grant_selected_at(b, f, k, wall_now()?, ttl)
    }
    pub(crate) fn begin(&mut self, id: &str, b: &Binding) -> Result<Ticket> {
        self.begin_at(id, b, wall_now()?)
    }
    pub(crate) fn finish(
        &mut self,
        t: Ticket,
        b: &Binding,
        s: Result<Snapshot>,
    ) -> Result<Receipt> {
        wall_now()?;
        self.finish_at(t, b, s, || wall_now().unwrap_or(u64::MAX))
    }
    pub(crate) fn get_ready(&mut self, id: &str, b: &Binding) -> Result<Receipt> {
        self.get_ready_at(id, b, wall_now()?)
    }
    /// The durable job must exist before linking it here. A linked scan may
    /// never be reused to replace missing/corrupt job history with a fresh job.
    pub(crate) fn commit_job(&mut self, scan: &str, binding: &Binding, job: CommittedJob) -> Result<Receipt> {
        self.commit_job_at(scan,binding,job,wall_now()?)
    }
    fn commit_job_at(&mut self, scan:&str, binding:&Binding, job:CommittedJob, now:u64)->Result<Receipt> {
        if Uuid::parse_str(&job.job_id).map_err(|_| "invalid committed job id")?.to_string() != job.job_id
            || job.identity_digest.len()!=64
            || !job.identity_digest.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
            return Err("invalid committed job binding".into());
        }
        let mut receipt=self.get_ready_at(scan,binding,now)?;
        if let Some(existing)=&receipt.committed_job {
            if existing!=&job { return Err("capture scan already committed to a different job".into()); }
            return Ok(receipt);
        }
        receipt.committed_job=Some(job);
        let _durability=crate::cell_durability::write_guard();
        write_json(&self.path(scan,"scan")?,&receipt)?;
        Ok(receipt)
    }
    #[cfg(test)]
    pub(crate) fn commit_expired_job_for_test(&mut self,scan:&str,binding:&Binding,job:CommittedJob)->Result<Receipt>{
        let expired=self.receipt(scan)?.expires.saturating_add(1);
        self.commit_job_at(scan,binding,job,expired)
    }
    /// Native owner status, including terminal states, without returning a
    /// source capability. Preview entries are available only while Ready.
    pub(crate) fn owner_status(&mut self, id: &str, binding: &Binding) -> Result<Receipt> {
        let receipt = self.receipt(id)?;
        if &receipt.binding != binding {
            return Err("capture not found for this owner and graph".into());
        }
        self.expire_at(wall_now()?)?;
        let receipt = self.receipt(id)?;
        if receipt.state == State::Ready {
            return self.get_ready(id, &receipt.binding);
        }
        Ok(receipt)
    }
    pub(crate) fn open_entry(&mut self, id: &str, i: usize, b: &Binding) -> Result<File> {
        wall_now()?;
        self.open_entry_at(id, i, b, || wall_now().unwrap_or(u64::MAX))
    }
    pub(crate) fn revoke(&mut self, id: &str) -> Result<()> {
        self.revoke_at(id, wall_now()?)
    }
    pub(crate) fn cancel(&mut self, id: &str, b: &Binding) -> Result<Receipt> {
        self.cancel_at(id, b, wall_now()?)
    }
    pub(crate) fn reap(&mut self) -> Result<()> {
        self.expire_at(wall_now()?)
    }
    pub(crate) fn revoke_graph(&mut self, graph: &str, incarnation: &str) -> Result<()> {
        let now = wall_now()?;
        let mut failure = None;
        for id in self.ids("grant")? {
            let g = match self.grant(&id) {
                Ok(g) => g,
                Err(e) => {
                    self.warn(e);
                    self.sources.remove(&id);
                    if let Err(e) = self.quarantine(&id, "grant") {
                        failure = Some(e);
                    }
                    continue;
                }
            };
            if g.binding.graph == graph && g.binding.incarnation == incarnation {
                if let Err(e) = self.revoke_at(&id, now) {
                    failure = Some(e);
                }
            }
        }
        failure.map_or(Ok(()), Err)
    }
}

struct CaptureWriter<'a, F: Fn() -> u64> {
    file: &'a mut File,
    hash: Sha256,
    cancelled: &'a AtomicBool,
    expires: u64,
    now: &'a F,
}
impl<F: Fn() -> u64> Write for CaptureWriter<'_, F> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.cancelled.load(Ordering::Acquire) || (self.now)() >= self.expires {
            return Err(std::io::Error::other("capture cancelled or expired"));
        }
        let n = self.file.write(buf)?;
        self.hash.update(&buf[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod tests {
    use super::*;
    use std::cell::Cell;
    fn binding() -> Binding {
        Binding {
            principal: "alice".into(),
            graph: "default".into(),
            incarnation: "generation-1".into(),
        }
    }
    fn source() -> File {
        let mut f = tempfile::tempfile().unwrap();
        {
            let mut z = zip::ZipWriter::new(&mut f);
            for (name, body) in [
                ("z.md", b"last".as_slice()),
                ("a.md", b"---\ntitle: First\n---\n[[z]]".as_slice()),
            ] {
                z.start_file(name, zip::write::SimpleFileOptions::default())
                    .unwrap();
                z.write_all(body).unwrap();
            }
            z.finish().unwrap();
        }
        f
    }
    fn begin(store: &mut CaptureStore) -> (String, Ticket) {
        let grant = store
            .grant_selected_at(binding(), source(), SourceKind::Zip, 10, 1000)
            .unwrap();
        let t = store.begin_at(&grant, &binding(), 11).unwrap();
        (grant, t)
    }
    fn ready(store: &mut CaptureStore) -> (String, Receipt) {
        let (g, mut t) = begin(store);
        let result = t.capture_at(|| 12);
        let r = store.finish_at(t, &binding(), result, || 13).unwrap();
        assert_eq!(r.state, State::Ready);
        (g, r)
    }
    #[test]
    fn survives_restart_with_exact_bytes_and_sorted_manifest() {
        let p = tempfile::tempdir().unwrap();
        let mut s = CaptureStore::open_at(p.path(), 1).unwrap();
        let (_, r) = ready(&mut s);
        assert_eq!(r.entries[0].path, "a.md");
        let expected = r.fingerprint.clone();
        drop(s);
        let mut s = CaptureStore::open_at(p.path(), 14).unwrap();
        assert_eq!(
            s.get_ready_at(&r.scan_id, &binding(), 14)
                .unwrap()
                .fingerprint,
            expected
        );
        let mut body = String::new();
        s.open_entry_at(&r.scan_id, 0, &binding(), || 14)
            .unwrap()
            .read_to_string(&mut body)
            .unwrap();
        assert_eq!(body, "---\ntitle: First\n---\n[[z]]");
    }
    #[test]
    fn grant_is_single_use_and_restarted_selection_cannot_reopen() {
        let p = tempfile::tempdir().unwrap();
        let mut s = CaptureStore::open_at(p.path(), 1).unwrap();
        let g = s
            .grant_selected_at(binding(), source(), SourceKind::Zip, 10, 1000)
            .unwrap();
        drop(s);
        let mut s = CaptureStore::open_at(p.path(), 12).unwrap();
        assert!(s
            .begin_at(&g, &binding(), 13)
            .err()
            .unwrap()
            .contains("select"));
        let (g, t) = begin(&mut s);
        assert!(s.begin_at(&g, &binding(), 12).is_err());
        drop(t);
    }
    #[test]
    fn all_three_binding_dimensions_are_enforced() {
        let p = tempfile::tempdir().unwrap();
        let mut s = CaptureStore::open_at(p.path(), 1).unwrap();
        let (g, r) = ready(&mut s);
        for b in [
            Binding {
                principal: "bob".into(),
                ..binding()
            },
            Binding {
                graph: "elsewhere".into(),
                ..binding()
            },
            Binding {
                incarnation: "generation-2".into(),
                ..binding()
            },
        ] {
            assert!(s.get_ready_at(&r.scan_id, &b, 14).is_err());
            assert!(s.open_entry_at(&r.scan_id, 0, &b, || 14).is_err());
            assert!(s.begin_at(&g, &b, 14).is_err());
            assert!(s.cancel_at(&r.scan_id, &b, 14).is_err());
        }
        assert!(s.get_ready_at(&r.scan_id, &binding(), 14).is_ok());
    }
    #[test]
    fn revocation_is_durable_and_purges_only_capture_files() {
        let p = tempfile::tempdir().unwrap();
        let keep = p.path().join("keep");
        fs::write(&keep, b"outside").unwrap();
        let mut s = CaptureStore::open_at(p.path(), 1).unwrap();
        let (g, r) = ready(&mut s);
        s.revoke_at(&g, 14).unwrap();
        assert!(s.get_ready_at(&r.scan_id, &binding(), 15).is_err());
        assert_eq!(fs::read(&keep).unwrap(), b"outside");
        assert!(!s.root.join(format!("{}-0.blob", r.scan_id)).exists());
        drop(s);
        let mut s = CaptureStore::open_at(p.path(), 16).unwrap();
        assert!(s.get_ready_at(&r.scan_id, &binding(), 16).is_err());
    }
    #[test]
    fn cancel_receipt_precedes_cleanup_and_cannot_publish_later() {
        let p = tempfile::tempdir().unwrap();
        let mut s = CaptureStore::open_at(p.path(), 1).unwrap();
        let (_, mut t) = begin(&mut s);
        let id = t.id().to_owned();
        let captured = t.capture_at(|| 12);
        assert_eq!(
            s.cancel_at(&id, &binding(), 13).unwrap().state,
            State::Cancelled
        );
        assert!(t.cancellation().load(Ordering::Acquire));
        assert!(s.finish_at(t, &binding(), captured, || 14).is_err());
        drop(s);
        let s = CaptureStore::open_at(p.path(), 15).unwrap();
        assert_eq!(s.receipt(&id).unwrap().state, State::Cancelled);
    }
    #[test]
    fn final_cancel_and_expiry_during_publication_are_observed() {
        for expire in [false, true] {
            let p = tempfile::tempdir().unwrap();
            let mut s = CaptureStore::open_at(p.path(), 1).unwrap();
            let (_, mut t) = begin(&mut s);
            let captured = t.capture_at(|| 12);
            let flag = t.cancellation();
            let calls = Cell::new(0);
            let r = s
                .finish_at(t, &binding(), captured, || {
                    calls.set(calls.get() + 1);
                    if calls.get() >= 3 {
                        if expire {
                            return 1010;
                        }
                        flag.store(true, Ordering::Release);
                    }
                    13
                })
                .unwrap();
            assert_eq!(
                r.state,
                if expire {
                    State::Expired
                } else {
                    State::Cancelled
                }
            );
            assert!(!s.root.join(format!("{}-0.blob", r.scan_id)).exists());
        }
    }
    #[test]
    fn recovery_terminalizes_partial_payloads_without_replaying() {
        let p = tempfile::tempdir().unwrap();
        let mut s = CaptureStore::open_at(p.path(), 1).unwrap();
        let (_, t) = begin(&mut s);
        let id = t.id().to_owned();
        fs::write(s.root.join(format!("{id}-0.blob")), b"partial").unwrap();
        fs::write(s.root.join(format!(".pending-{id}-test")), b"partial").unwrap();
        drop(t);
        drop(s);
        let s = CaptureStore::open_at(p.path(), 14).unwrap();
        assert_eq!(s.receipt(&id).unwrap().state, State::Failed);
        assert!(!s.root.join(format!("{id}-0.blob")).exists());
        assert!(!s.root.join(format!(".pending-{id}-test")).exists());
    }
    #[test]
    fn exclusive_store_lock_and_corruption_fail_closed() {
        let p = tempfile::tempdir().unwrap();
        let mut s = CaptureStore::open_at(p.path(), 1).unwrap();
        assert!(CaptureStore::open_at(p.path(), 2).is_err());
        let (_, r) = ready(&mut s);
        let payload = s.root.join(r.entries[0].payload.as_ref().unwrap());
        fs::write(&payload, b"wrong").unwrap();
        assert!(s.open_entry_at(&r.scan_id, 0, &binding(), || 14).is_err());
        let mut corrupted = r.clone();
        corrupted.entries[0].path = "other.md".into();
        write_json(&s.path(&r.scan_id, "scan").unwrap(), &corrupted).unwrap();
        assert!(s.get_ready_at(&r.scan_id, &binding(), 14).is_err());
    }
    #[test]
    fn expiry_and_interrupted_expiry_do_not_leave_reusable_grants() {
        let p = tempfile::tempdir().unwrap();
        let mut s = CaptureStore::open_at(p.path(), 1).unwrap();
        let (g, r) = ready(&mut s);
        assert!(s.check_ready(&r, &binding(), 1010).is_err());
        s.revoke_at(&g, 1010).unwrap();
        fs::remove_file(s.path(&r.scan_id, "scan").unwrap()).unwrap();
        drop(s);
        let mut s = CaptureStore::open_at(p.path(), 1011).unwrap();
        s.expire_at(1011).unwrap();
        assert!(s.grant(&g).is_err());
        let (_, r) = ready(&mut s);
        s.expire_at(1011).unwrap();
        assert!(s.receipt(&r.scan_id).is_err());
    }
    #[test]
    fn source_and_spool_symlinks_never_become_read_authority() {
        let p = tempfile::tempdir().unwrap();
        let mut s = CaptureStore::open_at(p.path(), 1).unwrap();
        let (_, r) = ready(&mut s);
        let outside = p.path().join("outside");
        fs::write(&outside, b"secret").unwrap();
        let payload = s.root.join(r.entries[0].payload.as_ref().unwrap());
        fs::remove_file(&payload).unwrap();
        std::os::unix::fs::symlink(&outside, &payload).unwrap();
        assert!(s.open_entry_at(&r.scan_id, 0, &binding(), || 14).is_err());
        s.cancel_at(&r.scan_id, &binding(), 15).unwrap();
        assert_eq!(fs::read(outside).unwrap(), b"secret");
    }
    #[test]
    fn cancellation_keeps_capacity_until_worker_releases_ticket() {
        let p = tempfile::tempdir().unwrap();
        let mut s = CaptureStore::open_at(p.path(), 1).unwrap();
        let (_, ticket) = begin(&mut s);
        s.cancel_at(ticket.id(), &binding(), 12).unwrap();
        let next = s
            .grant_selected_at(binding(), source(), SourceKind::Zip, 12, 1000)
            .unwrap();
        assert!(s.begin_at(&next, &binding(), 13).is_err());
        drop(ticket);
        assert!(s.begin_at(&next, &binding(), 14).is_ok());
    }
    #[test]
    fn independent_captures_share_content_fingerprint_not_identifiers() {
        let p = tempfile::tempdir().unwrap();
        let mut s = CaptureStore::open_at(p.path(), 1).unwrap();
        let (g1, r1) = ready(&mut s);
        let (g2, r2) = ready(&mut s);
        assert_ne!(g1, g2);
        assert_ne!(r1.scan_id, r2.scan_id);
        assert_eq!(r1.fingerprint, r2.fingerprint);
    }
    #[test]
    fn abandoned_and_rejected_tickets_do_not_wedge_new_captures() {
        let p = tempfile::tempdir().unwrap();
        let mut s = CaptureStore::open_at(p.path(), 1).unwrap();
        let (_, t) = begin(&mut s);
        let old = t.id().to_owned();
        drop(t);
        let (_, mut t) = begin(&mut s);
        assert_eq!(s.receipt(&old).unwrap().state, State::Failed);
        let old = t.id().to_owned();
        let data = t.capture_at(|| 12);
        assert!(s
            .finish_at(
                t,
                &Binding {
                    principal: "wrong".into(),
                    ..binding()
                },
                data,
                || 13
            )
            .is_err());
        let (_, t) = begin(&mut s);
        assert_eq!(s.receipt(&old).unwrap().state, State::Failed);
        drop(t);
    }
    #[test]
    fn missing_and_same_size_corrupt_payloads_are_not_ready_after_restart() {
        for missing in [false, true] {
            let p = tempfile::tempdir().unwrap();
            let mut s = CaptureStore::open_at(p.path(), 1).unwrap();
            let (_, bad) = ready(&mut s);
            let (_, good) = ready(&mut s);
            let path = s.root.join(bad.entries[0].payload.as_ref().unwrap());
            if missing {
                fs::remove_file(path).unwrap();
            } else {
                fs::write(path, vec![b'x'; bad.entries[0].bytes as usize]).unwrap();
            }
            drop(s);
            let mut s = CaptureStore::open_at(p.path(), 14).unwrap();
            assert_eq!(s.receipt(&bad.scan_id).unwrap().state, State::Failed);
            assert!(s.get_ready_at(&bad.scan_id, &binding(), 14).is_err());
            assert!(s.get_ready_at(&good.scan_id, &binding(), 14).is_ok());
        }
    }
    #[test]
    fn malformed_grants_and_receipts_quarantine_without_blocking_healthy_work() {
        let p = tempfile::tempdir().unwrap();
        let mut s = CaptureStore::open_at(p.path(), 1).unwrap();
        let (_, bad) = ready(&mut s);
        let (_, good) = ready(&mut s);
        let unused = s
            .grant_selected_at(binding(), source(), SourceKind::Zip, 10, 1000)
            .unwrap();
        fs::write(s.path(&bad.scan_id, "scan").unwrap(), b"broken JSON").unwrap();
        fs::write(s.path(&unused, "grant").unwrap(), b"broken JSON").unwrap();
        drop(s);
        let mut s = CaptureStore::open_at(p.path(), 14).unwrap();
        assert!(s.get_ready_at(&good.scan_id, &binding(), 14).is_ok());
        assert!(!s.warnings().is_empty());
        let (_, t) = begin(&mut s);
        drop(t);
        assert!(!s.root.join(format!("{}-0.blob", bad.scan_id)).exists());
    }
    #[test]
    fn ready_lookup_reaps_expired_payloads_and_cannot_revive_on_clock_rollback() {
        let p = tempfile::tempdir().unwrap();
        let mut s = CaptureStore::open_at(p.path(), 1).unwrap();
        let (_, r) = ready(&mut s);
        assert!(s.get_ready_at(&r.scan_id, &binding(), 1010).is_err());
        assert!(!s.root.join(format!("{}-0.blob", r.scan_id)).exists());
        assert!(s.get_ready_at(&r.scan_id, &binding(), 14).is_err());
    }
    #[test]
    fn revoke_graph_is_incarnation_specific() {
        let p = tempfile::tempdir().unwrap();
        let mut s = CaptureStore::open_at(p.path(), 1).unwrap();
        let (_, old) = ready(&mut s);
        let new_binding = Binding {
            incarnation: "generation-2".into(),
            ..binding()
        };
        let g = s
            .grant_selected_at(new_binding.clone(), source(), SourceKind::Zip, 10, 1000)
            .unwrap();
        let mut t = s.begin_at(&g, &new_binding, 11).unwrap();
        let bytes = t.capture_at(|| 12);
        let new = s.finish_at(t, &new_binding, bytes, || 13).unwrap();
        s.revoke_graph("default", "generation-1").unwrap();
        assert!(s.get_ready_at(&old.scan_id, &binding(), 14).is_err());
        assert!(s.get_ready_at(&new.scan_id, &new_binding, 14).is_ok());
    }
    #[test]
    fn public_grants_use_the_wall_clock() {
        let p = tempfile::tempdir().unwrap();
        let mut s = CaptureStore::open(p.path()).unwrap();
        let before = wall_now().unwrap();
        let g = s
            .grant_selected(binding(), source(), SourceKind::Zip, 10000)
            .unwrap();
        let after = wall_now().unwrap();
        let expires = s.grant(&g).unwrap().expires;
        assert!(expires >= before + 10000 && expires <= after + 10000);
    }
    #[test]
    fn invalid_clock_does_not_reap_existing_capture() {
        assert!(wall_ms(std::time::UNIX_EPOCH - std::time::Duration::from_secs(1)).is_err());
        let p = tempfile::tempdir().unwrap();
        let mut s = CaptureStore::open_at(p.path(), 1).unwrap();
        let (_, r) = ready(&mut s);
        assert!(s.expire_at(u64::MAX).is_err());
        assert!(s.get_ready_at(&r.scan_id, &binding(), 14).is_ok());
        assert!(s.root.join(format!("{}-0.blob", r.scan_id)).exists());
    }
    #[test]
    fn revoke_corrupt_receipt_still_removes_payloads() {
        let p = tempfile::tempdir().unwrap();
        let mut s = CaptureStore::open_at(p.path(), 1).unwrap();
        let (g, r) = ready(&mut s);
        fs::write(s.path(&r.scan_id, "scan").unwrap(), b"corrupt").unwrap();
        s.revoke_at(&g, 14).unwrap();
        assert!(s.grant(&g).unwrap().revoked);
        assert!(!s.root.join(format!("{}-0.blob", r.scan_id)).exists());
        assert!(!s.warnings().is_empty());
    }
}
