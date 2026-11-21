//! Door 1's atomic workspace receipt primitive, not an authorization surface.
//! The caller must hold the graph lifecycle lease, validate the live incarnation
//! and consent, and publish owned bytes before entering this transaction.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
};
use yrs::{Any, Map, Out, ReadTxn, TransactionMut, WriteTxn};

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PublishError {
    Refused(String),
    /// The destination may exist. Retain source and reconcile; do not remove it.
    Uncertain(String),
}

/// Destination is an owner-generated path under the live graph's artifact
/// store, never a request/source path. The caller holds its lifecycle lease.
/// This publishes bytes only; it grants no authority and creates no artifact.
pub(crate) fn publish_owned_payload(
    source: &mut (impl Read + Seek),
    destination: &Path,
    size: u64,
    sha256: &str,
    cancelled: impl Fn() -> bool,
) -> Result<(), PublishError> {
    publish_owned_payload_with_sync(
        source,
        destination,
        size,
        sha256,
        cancelled,
        crate::storage_atomic::sync_parent_dir,
        || Ok(()),
    )
}

/// Copy/hash without the caller's inventory mutex; serialize only final
/// publication so inventory cannot observe an intermediate rename/hardlink.
pub(crate) fn publish_owned_payload_guarded<G>(
    source: &mut (impl Read + Seek),
    destination: &Path,
    size: u64,
    sha256: &str,
    publication_guard: impl FnOnce() -> Result<G, String>,
) -> Result<(), PublishError> {
    publish_owned_payload_with_sync(
        source,
        destination,
        size,
        sha256,
        || false,
        crate::storage_atomic::sync_parent_dir,
        publication_guard,
    )
}

fn publish_owned_payload_with_sync<G>(
    source: &mut (impl Read + Seek),
    destination: &Path,
    size: u64,
    sha256: &str,
    cancelled: impl Fn() -> bool,
    sync_parent: impl FnOnce(&Path) -> Result<(), String>,
    publication_guard: impl FnOnce() -> Result<G, String>,
) -> Result<(), PublishError> {
    use PublishError::{Refused, Uncertain};
    if !cfg!(any(target_os = "macos", target_os = "linux")) {
        return Err(Refused(
            "owned import publication is not verified on this platform".into(),
        ));
    }
    if !is_hash(sha256) || size > 256 * 1024 * 1024 || cancelled() {
        return Err(Refused("invalid or cancelled artifact publication".into()));
    }
    let parent = destination
        .parent()
        .ok_or_else(|| Refused("missing owned destination directory".into()))?;
    let _guard = crate::cell_durability::write_guard();
    let mut temporary =
        tempfile::NamedTempFile::new_in(parent).map_err(|e| Refused(e.to_string()))?;
    source
        .seek(SeekFrom::Start(0))
        .map_err(|e| Refused(e.to_string()))?;
    let mut hash = Sha256::new();
    let mut copied = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        if cancelled() {
            return Err(Refused("artifact publication cancelled".into()));
        }
        let n = source
            .read(&mut buffer)
            .map_err(|e| Refused(e.to_string()))?;
        if n == 0 {
            break;
        }
        copied = copied
            .checked_add(n as u64)
            .ok_or_else(|| Refused("artifact length overflow".into()))?;
        if copied > size {
            return Err(Refused("artifact exceeds captured length".into()));
        }
        hash.update(&buffer[..n]);
        temporary
            .write_all(&buffer[..n])
            .map_err(|e| Refused(e.to_string()))?;
    }
    if copied != size || format!("{:x}", hash.finalize()) != sha256 {
        return Err(Refused("artifact bytes do not match capture".into()));
    }
    temporary
        .as_file()
        .sync_all()
        .map_err(|e| Refused(e.to_string()))?;
    if cancelled() {
        return Err(Refused("artifact publication cancelled".into()));
    }
    let _publication = publication_guard().map_err(Refused)?;
    match temporary.persist_noclobber(destination) {
        Ok(_) => {}
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            // A previous preparation may have survived a crash. Verify rather
            // than overwrite it; NOFOLLOW prevents laundering a linked file.
            let mut existing = open_owned_regular(destination).map_err(Refused)?;
            if existing
                .metadata()
                .map_err(|e| Refused(e.to_string()))?
                .len()
                != size
            {
                return Err(Refused("owned destination length conflict".into()));
            }
            let mut hash = Sha256::new();
            let mut observed = 0u64;
            loop {
                if cancelled() {
                    return Err(Refused("artifact publication cancelled".into()));
                }
                let n = existing
                    .read(&mut buffer)
                    .map_err(|e| Refused(e.to_string()))?;
                if n == 0 {
                    break;
                }
                observed += n as u64;
                if observed > size {
                    return Err(Refused("owned destination grew during verification".into()));
                }
                hash.update(&buffer[..n]);
            }
            if observed != size || format!("{:x}", hash.finalize()) != sha256 {
                return Err(Refused("owned destination hash conflict".into()));
            }
        }
        Err(error) => return Err(Uncertain(error.error.to_string())),
    }
    sync_parent(parent).map_err(Uncertain)
}

fn open_owned_regular(path: &Path) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        let file = options.open(path).map_err(|e| e.to_string())?;
        let metadata = file.metadata().map_err(|e| e.to_string())?;
        if !metadata.is_file() || metadata.nlink() != 1 {
            return Err("owned destination is not a private regular file".into());
        }
        Ok(file)
    }
    #[cfg(not(unix))]
    {
        let _ = (path, options);
        Err("owned destination validation unsupported".into())
    }
}

const RECEIPTS: &str = crate::workspace_receipt_guard::RECEIPT_MAP;
// Refuse new work at capacity: eviction could resurrect a deleted artifact.
// A production retention protocol is required before exposing this primitive.
const MAX_RECEIPTS: u32 = 100_000;

#[derive(Clone, Debug)]
pub(crate) struct Claim {
    key: String,
    digest: String,
    artifact_id: String,
}

impl Claim {
    /// Identity is independent of destination metadata. Changing that metadata
    /// on a retry is a conflict, not permission for a second insertion.
    pub(crate) fn new(
        graph: &str,
        incarnation: &str,
        scan: &str,
        entry: usize,
        fingerprint: &str,
        publication: &serde_json::Value,
    ) -> Result<Self, String> {
        if graph.is_empty()
            || incarnation.is_empty()
            || graph.len() > 1024
            || incarnation.len() > 128
        {
            return Err("invalid import graph binding".into());
        }
        if uuid::Uuid::parse_str(scan)
            .map_err(|_| "invalid scan id")?
            .to_string()
            != scan
            || entry >= 10_000
            || !is_hash(fingerprint)
        {
            return Err("invalid import entry identity".into());
        }
        let key = hash(&serde_json::json!([graph, incarnation, scan, entry]));
        let artifact_id = format!("artifact-{key}");
        let source = &publication["source"];
        if publication["artifactId"] != artifact_id
            || publication["mode"] != "attached"
            || publication["storage"] != "owned"
            || source["kind"] != "import"
            || source["importId"] != scan
            || source["relativePath"].as_str().is_none_or(str::is_empty)
            || publication["sha256"].as_str().is_none_or(|h| !is_hash(h))
            || publication["sizeBytes"]
                .as_u64()
                .is_none_or(|n| n > 256 * 1024 * 1024)
        {
            return Err("import publication must describe this owned attachment".into());
        }
        let digest = hash(&serde_json::json!([fingerprint, publication]));
        Ok(Self {
            key,
            digest,
            artifact_id,
        })
    }

    pub(crate) fn artifact_id(graph: &str, incarnation: &str, scan: &str, entry: usize) -> String {
        format!(
            "artifact-{}",
            hash(&serde_json::json!([graph, incarnation, scan, entry]))
        )
    }
}

fn is_hash(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

fn hash(value: &serde_json::Value) -> String {
    // Do not depend on serde_json's optional preserve_order feature.
    fn canonical(value: &serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::Object(object) => {
                let sorted: std::collections::BTreeMap<_, _> = object.iter().collect();
                serde_json::Value::Object(
                    sorted
                        .into_iter()
                        .map(|(k, v)| (k.clone(), canonical(v)))
                        .collect(),
                )
            }
            serde_json::Value::Array(values) => {
                serde_json::Value::Array(values.iter().map(canonical).collect())
            }
            other => other.clone(),
        }
    }
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&canonical(value)).expect("JSON value serializes"))
    )
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    version: u8,
    digest: String,
    artifact_id: String,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Outcome {
    pub artifact_id: String,
    /// Historical application, not a claim that the artifact still exists.
    pub replayed: bool,
}

/// Call inside Room::update_doc so receipt and mutation share durable state.
/// Like every Yrs mutation, a callback error does not roll back the transaction:
/// the owning Room must evict on error and reload its last durable snapshot.
/// No filesystem work belongs in this closure. Persistence failure means the
/// caller must retain bytes and reconcile, never report definite rejection.
pub(crate) fn apply_once(
    txn: &mut TransactionMut<'_>,
    claim: &Claim,
    mutate: impl FnOnce(&mut TransactionMut<'_>) -> Result<(), String>,
) -> Result<Outcome, String> {
    apply_once_bounded(txn, claim, MAX_RECEIPTS, mutate)
}

fn apply_once_bounded(
    txn: &mut TransactionMut<'_>,
    claim: &Claim,
    capacity: u32,
    mutate: impl FnOnce(&mut TransactionMut<'_>) -> Result<(), String>,
) -> Result<Outcome, String> {
    if let Some(outcome) = preflight_bounded(&*txn, claim, capacity)? {
        return Ok(outcome);
    }
    apply_new(txn, claim, mutate)
}

/// Under the graph lease, validate ownership before publishing any final files.
pub(crate) fn preflight<T: ReadTxn>(txn: &T, claim: &Claim) -> Result<Option<Outcome>, String> {
    preflight_bounded(txn, claim, MAX_RECEIPTS)
}

fn preflight_bounded<T: ReadTxn>(
    txn: &T,
    claim: &Claim,
    capacity: u32,
) -> Result<Option<Outcome>, String> {
    if let Some(outcome) = lookup(txn, claim)? {
        return Ok(Some(outcome));
    }
    if let Some(receipts) = txn.get_map(RECEIPTS) {
        if receipts.len(&*txn) >= capacity {
            return Err("import receipt capacity reached; no receipts were evicted".into());
        }
    } else if capacity == 0 {
        return Err("import receipt capacity reached; no receipts were evicted".into());
    }
    use yrs::types::AsPrelim;
    if let Some((_, root)) = txn.root_refs().find(|(name, _)| *name == "artifacts") {
        if !matches!(root.as_prelim(txn), yrs::In::Map(_)) {
            return Err("invalid artifact ownership map".into());
        }
    }
    if txn
        .get_map("artifacts")
        .is_some_and(|m| m.contains_key(txn, &claim.artifact_id))
    {
        return Err("import artifact already exists without matching receipt".into());
    }
    Ok(None)
}

/// Historical lookup must happen before replay can touch an owned file.
pub(crate) fn lookup<T: ReadTxn>(txn: &T, claim: &Claim) -> Result<Option<Outcome>, String> {
    use yrs::types::AsPrelim;
    if let Some((_, root)) = txn.root_refs().find(|(name, _)| *name == RECEIPTS) {
        if !matches!(root.as_prelim(txn), yrs::In::Map(_)) {
            return Err("invalid import receipt root; reconciliation required".into());
        }
    }
    if let Some(receipts) = txn.get_map(RECEIPTS) {
        if let Some(stored) = receipts.get(&*txn, &claim.key) {
            let Out::Any(Any::String(raw)) = stored else {
                return Err("invalid import receipt; reconciliation required".into());
            };
            let receipt: Receipt = serde_json::from_str(&raw)
                .map_err(|_| "invalid import receipt; reconciliation required")?;
            if receipt.version != 1
                || receipt.digest != claim.digest
                || receipt.artifact_id != claim.artifact_id
            {
                return Err("import receipt conflict; refusing changed or unknown replay".into());
            }
            return Ok(Some(Outcome {
                artifact_id: claim.artifact_id.clone(),
                replayed: true,
            }));
        }
    }
    Ok(None)
}

fn apply_new(
    txn: &mut TransactionMut<'_>,
    claim: &Claim,
    mutate: impl FnOnce(&mut TransactionMut<'_>) -> Result<(), String>,
) -> Result<Outcome, String> {
    // A preexisting artifact without our receipt is a conflict, not evidence
    // that our operation succeeded; it may belong to another operation.
    if txn
        .get_map("artifacts")
        .is_some_and(|m| m.contains_key(&*txn, &claim.artifact_id))
    {
        return Err("import artifact already exists without matching receipt".into());
    }
    let encoded = serde_json::to_string(&Receipt {
        version: 1,
        digest: claim.digest.clone(),
        artifact_id: claim.artifact_id.clone(),
    })
    .map_err(|e| e.to_string())?;
    mutate(txn)?;
    if !txn
        .get_map("artifacts")
        .is_some_and(|m| m.contains_key(&*txn, &claim.artifact_id))
    {
        return Err("import mutation did not create its artifact".into());
    }
    txn.get_or_insert_map(RECEIPTS)
        .insert(txn, claim.key.as_str(), encoded);
    Ok(Outcome {
        artifact_id: claim.artifact_id.clone(),
        replayed: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use yrs::{updates::decoder::Decode, Doc, StateVector, Transact, Update};

    const SCAN: &str = "f7e73c3f-cc36-4e9e-9af6-e6a9df4fb654";
    fn bytes_hash(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    #[test]
    fn publication_preserves_source_and_retries_without_clobbering() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("body");
        let bytes = b"captured body";
        let mut source = std::io::Cursor::new(bytes.to_vec());
        publish_owned_payload(
            &mut source,
            &path,
            bytes.len() as u64,
            &bytes_hash(bytes),
            || false,
        )
        .unwrap();
        publish_owned_payload(
            &mut source,
            &path,
            bytes.len() as u64,
            &bytes_hash(bytes),
            || false,
        )
        .unwrap();
        assert_eq!(source.into_inner(), bytes);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn failed_directory_sync_reports_uncertainty_and_preserves_both_copies() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("body");
        let mut source = std::io::Cursor::new(b"abc".to_vec());
        let result = publish_owned_payload_with_sync(
            &mut source,
            &path,
            3,
            &bytes_hash(b"abc"),
            || false,
            |_| Err("injected sync failure".into()),
            || Ok(()),
        );
        assert!(matches!(result, Err(PublishError::Uncertain(_))));
        assert_eq!(std::fs::read(&path).unwrap(), b"abc");
        assert_eq!(source.get_ref(), b"abc");
        publish_owned_payload(&mut source, &path, 3, &bytes_hash(b"abc"), || false).unwrap();
    }

    #[test]
    fn bad_size_hash_or_cancellation_leaves_no_destination() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("body");
        for (size, hash) in [
            (2, bytes_hash(b"abc")),
            (4, bytes_hash(b"abc")),
            (3, bytes_hash(b"bad")),
        ] {
            assert!(publish_owned_payload(
                &mut std::io::Cursor::new(b"abc"),
                &path,
                size,
                &hash,
                || false
            )
            .is_err());
            assert!(!path.exists());
        }
        let polls = std::cell::Cell::new(0);
        assert!(publish_owned_payload(
            &mut std::io::Cursor::new(b"abc"),
            &path,
            3,
            &bytes_hash(b"abc"),
            || {
                polls.set(polls.get() + 1);
                polls.get() >= 3
            }
        )
        .is_err());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[test]
    fn conflicting_destination_is_never_replaced() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("body");
        std::fs::write(&path, b"old").unwrap();
        assert!(publish_owned_payload(
            &mut std::io::Cursor::new(b"new"),
            &path,
            3,
            &bytes_hash(b"new"),
            || false
        )
        .is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"old");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn linked_destination_is_refused_without_touching_its_target() {
        let directory = tempfile::tempdir().unwrap();
        let original = directory.path().join("original");
        std::fs::write(&original, b"abc").unwrap();
        for hard in [false, true] {
            let path = directory.path().join(if hard { "hard" } else { "soft" });
            if hard {
                std::fs::hard_link(&original, &path).unwrap();
            } else {
                std::os::unix::fs::symlink(&original, &path).unwrap();
            }
            assert!(publish_owned_payload(
                &mut std::io::Cursor::new(b"abc"),
                &path,
                3,
                &bytes_hash(b"abc"),
                || false
            )
            .is_err());
        }
        assert_eq!(std::fs::read(&original).unwrap(), b"abc");
    }
    fn publication(entry: usize) -> serde_json::Value {
        serde_json::json!({
            "artifactId": Claim::artifact_id("graph", "incarnation", SCAN, entry),
            "mode": "attached", "storage": "owned", "sha256": "a".repeat(64), "sizeBytes": 12,
            "source": {"kind":"import", "importId": SCAN, "relativePath":"assets/image.png"}
        })
    }
    fn claim(entry: usize) -> Claim {
        Claim::new(
            "graph",
            "incarnation",
            SCAN,
            entry,
            &"b".repeat(64),
            &publication(entry),
        )
        .unwrap()
    }
    fn insert(txn: &mut TransactionMut<'_>, claim: &Claim) -> Result<(), String> {
        txn.get_or_insert_map("artifacts")
            .insert(txn, claim.artifact_id.as_str(), "body");
        Ok(())
    }
    #[test]
    fn receipt_and_artifact_survive_snapshot_together() {
        let doc = Doc::new();
        let claim = claim(0);
        assert!(
            !apply_once(&mut doc.transact_mut(), &claim, |t| insert(t, &claim))
                .unwrap()
                .replayed
        );
        let bytes = doc
            .transact()
            .encode_state_as_update_v1(&StateVector::default());
        let reopened = Doc::new();
        reopened
            .transact_mut()
            .apply_update(Update::decode_v1(&bytes).unwrap())
            .unwrap();
        assert!(
            apply_once(&mut reopened.transact_mut(), &claim, |_| panic!(
                "replayed mutation"
            ))
            .unwrap()
            .replayed
        );
    }
    #[test]
    fn replay_does_not_resurrect_deleted_artifact() {
        let doc = Doc::new();
        let claim = claim(0);
        let mut txn = doc.transact_mut();
        apply_once(&mut txn, &claim, |t| insert(t, &claim)).unwrap();
        txn.get_map("artifacts")
            .unwrap()
            .remove(&mut txn, &claim.artifact_id);
        assert!(
            apply_once(&mut txn, &claim, |_| panic!("resurrection"))
                .unwrap()
                .replayed
        );
        assert!(!txn
            .get_map("artifacts")
            .unwrap()
            .contains_key(&txn, &claim.artifact_id));
    }
    #[test]
    fn changed_payload_conflicts_without_mutation() {
        let doc = Doc::new();
        let original = claim(0);
        let mut txn = doc.transact_mut();
        apply_once(&mut txn, &original, |t| insert(t, &original)).unwrap();
        let mut payload = publication(0);
        payload["label"] = "changed".into();
        let changed =
            Claim::new("graph", "incarnation", SCAN, 0, &"b".repeat(64), &payload).unwrap();
        assert!(apply_once(&mut txn, &changed, |_| panic!("conflict mutated")).is_err());
    }
    #[test]
    fn corrupt_receipt_fails_closed() {
        let doc = Doc::new();
        let claim = claim(0);
        let mut txn = doc.transact_mut();
        txn.get_or_insert_map(RECEIPTS)
            .insert(&mut txn, claim.key.as_str(), "broken");
        assert!(apply_once(&mut txn, &claim, |_| panic!("corruption mutated")).is_err());
    }
    #[test]
    fn capacity_refuses_without_evicting_replay_guard() {
        let doc = Doc::new();
        let a = claim(0);
        let b = claim(1);
        let mut txn = doc.transact_mut();
        apply_once_bounded(&mut txn, &a, 1, |t| insert(t, &a)).unwrap();
        assert!(apply_once_bounded(&mut txn, &b, 1, |_| panic!("over capacity")).is_err());
        assert!(
            apply_once_bounded(&mut txn, &a, 1, |_| panic!("evicted"))
                .unwrap()
                .replayed
        );
    }
    #[test]
    fn reference_or_borrowed_payload_cannot_be_adopted() {
        for (field, value) in [
            ("storage", "borrowed"),
            ("storage", "external"),
            ("mode", "referenced"),
        ] {
            let mut payload = publication(0);
            payload[field] = value.into();
            assert!(
                Claim::new("graph", "incarnation", SCAN, 0, &"b".repeat(64), &payload).is_err()
            );
        }
    }
    #[test]
    fn identity_is_scoped_to_incarnation_scan_and_entry() {
        assert_ne!(
            Claim::artifact_id("graph", "incarnation", SCAN, 0),
            Claim::artifact_id("graph", "new", SCAN, 0)
        );
        assert_ne!(claim(0).artifact_id, claim(1).artifact_id);
        assert_ne!(
            Claim::artifact_id("graph", "incarnation", SCAN, 0),
            Claim::artifact_id(
                "graph",
                "incarnation",
                "f7e73c3f-cc36-4e9e-9af6-e6a9df4fb655",
                0
            )
        );
    }
    #[test]
    fn missing_artifact_or_failed_mutation_gets_no_receipt() {
        let doc = Doc::new();
        let claim = claim(0);
        let mut txn = doc.transact_mut();
        assert!(apply_once(&mut txn, &claim, |_| Ok(())).is_err());
        assert!(apply_once(&mut txn, &claim, |_| Err("rejected".into())).is_err());
        assert!(txn.get_map(RECEIPTS).is_none());
    }
    #[test]
    fn preexisting_artifact_is_not_inferred_success() {
        let doc = Doc::new();
        let claim = claim(0);
        let mut txn = doc.transact_mut();
        insert(&mut txn, &claim).unwrap();
        assert!(apply_once(&mut txn, &claim, |_| panic!("overwritten")).is_err());
    }
}
