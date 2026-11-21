//! Raw client sync may echo server receipts, but may never change them.
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use yrs::types::ToJson;
use yrs::{updates::decoder::Decode, DeepObservable, Doc, ReadTxn, StateVector, Transact, Update};

pub(crate) const RECEIPT_MAP: &str = "_vaultArtifactReceipts";

/// Restore content, not the permission to repeat a previously applied import.
/// Only copy the reserved records; merging the whole current Doc would undo
/// the requested content rewind. Called by trusted restore under its graph lease.
pub(crate) fn preserve_for_restore(current: &[u8], target: &[u8]) -> Result<Vec<u8>, String> {
    use std::collections::BTreeMap;
    use yrs::{Any, In, Map, Out};
    use yrs::types::AsPrelim;
    fn load(bytes: &[u8]) -> Result<Doc, String> {
        let doc = Doc::new();
        if !bytes.is_empty() {
            doc.transact_mut()
                .apply_update(
                    Update::decode_v1(bytes)
                        .map_err(|e| format!("decode restore receipt history: {e}"))?,
                )
                .map_err(|e| e.to_string())?;
        }
        if doc.transact().has_missing_updates() {
            return Err("Restore receipt history has missing dependencies".into());
        }
        Ok(doc)
    }
    fn records(doc: &Doc) -> Result<BTreeMap<String, String>, String> {
        let txn = doc.transact();
        let mut records = BTreeMap::new();
        // Decoded root collections are untyped in Yrs; get_map is a cast,
        // not validation. Inspect their stored shape before interpreting it.
        if let Some((_, root)) = txn.root_refs().find(|(name, _)| *name == RECEIPT_MAP) {
            if !matches!(root.as_prelim(&txn), In::Map(_)) {
                return Err("Restore cannot preserve malformed receipt root".into());
            }
        }
        if let Some(map) = txn.get_map(RECEIPT_MAP) {
            for (key, value) in map.iter(&txn) {
                let Out::Any(Any::String(raw)) = value else {
                    return Err("Restore cannot preserve malformed receipt storage".into());
                };
                records.insert(key.to_string(), raw.to_string());
            }
        }
        Ok(records)
    }
    let current = load(current)?;
    let current_records = records(&current)?;
    // Preserve legacy opaque snapshot behavior only after proving there is no
    // current receipt history to lose. Corrupt current state is never proof.
    if current_records.is_empty() {
        return Ok(target.to_vec());
    }
    let target = load(target)?;
    let target_records = records(&target)?;
    for (key, value) in &current_records {
        if target_records.get(key).is_some_and(|past| past != value) {
            return Err("Restore contains conflicting import receipt history".into());
        }
    }
    {
        let map = target.get_or_insert_map(RECEIPT_MAP);
        let mut txn = target.transact_mut();
        for (key, value) in current_records {
            if !target_records.contains_key(&key) {
                map.insert(&mut txn, key, value);
            }
        }
    }
    let result = target
        .transact()
        .encode_state_as_update_v1(&StateVector::default());
    Ok(result)
}

/// Legacy native saves replace state; omission must not erase server evidence.
pub(crate) fn validate_replacement(current: &[u8], replacement: &[u8]) -> Result<(), String> {
    fn load(bytes: &[u8]) -> Result<Doc, String> {
        let doc = Doc::new();
        if !bytes.is_empty() {
            doc.transact_mut()
                .apply_update(Update::decode_v1(bytes).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
        }
        Ok(doc)
    }
    let current = load(current)?;
    let replacement_doc = load(replacement)?;
    if replacement_doc.transact().has_missing_updates() {
        return Err("Workspace replacement has missing dependencies".into());
    }
    let a = current
        .get_or_insert_map(RECEIPT_MAP)
        .to_json(&current.transact());
    let b = replacement_doc
        .get_or_insert_map(RECEIPT_MAP)
        .to_json(&replacement_doc.transact());
    if a != b {
        return Err("Workspace replacement cannot change server-owned import receipts".into());
    }
    let encoded = replacement_doc
        .transact()
        .encode_state_as_update_v1(&StateVector::default());
    validate_client_update(&current, &encoded)
}

/// Called while the live workspace Doc is locked. Trial mutations never enter
/// its pending-update buffers or reach persistence/broadcast on refusal.
pub(crate) fn validate_client_update(doc: &Doc, bytes: &[u8]) -> Result<(), String> {
    let scratch = Doc::new();
    let current = doc
        .transact()
        .encode_state_as_update_v1(&StateVector::default());
    scratch
        .transact_mut()
        .apply_update(Update::decode_v1(&current).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    let receipts = scratch.get_or_insert_map(RECEIPT_MAP);
    let changed = Arc::new(AtomicBool::new(false));
    let observed = changed.clone();
    let _subscription = receipts.observe_deep(move |_, _| {
        observed.store(true, Ordering::Relaxed);
    });
    {
        let mut txn = scratch.transact_mut();
        txn.apply_update(Update::decode_v1(bytes).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        // Buffering a partial update could let an innocent later update make
        // a forbidden write visible. Require a complete sync retry instead.
        if txn.has_missing_updates() {
            return Err("Workspace update has missing dependencies; resync complete state".into());
        }
    }
    if changed.load(Ordering::Relaxed) {
        return Err("Workspace import receipts are server-owned".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use yrs::{Map, MapPrelim, Text, WriteTxn};

    fn full(doc: &Doc) -> Vec<u8> {
        doc.transact()
            .encode_state_as_update_v1(&StateVector::default())
    }
    fn clone_doc(doc: &Doc) -> Doc {
        let copy = Doc::new();
        copy.transact_mut()
            .apply_update(Update::decode_v1(&full(doc)).unwrap())
            .unwrap();
        copy
    }
    fn server() -> Doc {
        let doc = Doc::new();
        doc.get_or_insert_map(RECEIPT_MAP)
            .insert(&mut doc.transact_mut(), "entry", "receipt");
        doc
    }

    #[test]
    fn echo_and_unrelated_edits_are_allowed() {
        let doc = server();
        let remote = clone_doc(&doc);
        assert!(validate_client_update(&doc, &full(&remote)).is_ok());
        remote
            .get_or_insert_map("folders")
            .insert(&mut remote.transact_mut(), "folder", "name");
        assert!(validate_client_update(&doc, &full(&remote)).is_ok());
    }

    #[test]
    fn replacement_cannot_omit_or_forge_receipts() {
        let doc = server();
        assert!(validate_replacement(&full(&doc), &full(&doc)).is_ok());
        assert!(validate_replacement(&full(&doc), &full(&Doc::new())).is_err());
        assert!(validate_replacement(&full(&Doc::new()), &full(&doc)).is_err());
        assert!(validate_replacement(&full(&doc), &[]).is_err());
        assert!(validate_replacement(&[], &full(&Doc::new())).is_ok());
    }

    #[test]
    fn restore_preserves_history_but_rewinds_content() {
        let current = server();
        current.get_or_insert_map("documents").insert(
            &mut current.transact_mut(),
            "doc",
            "new content",
        );
        let target = Doc::new();
        target.get_or_insert_map("documents").insert(
            &mut target.transact_mut(),
            "doc",
            "old content",
        );
        let restored = Doc::new();
        restored
            .transact_mut()
            .apply_update(
                Update::decode_v1(&preserve_for_restore(&full(&current), &full(&target)).unwrap())
                    .unwrap(),
            )
            .unwrap();
        let txn = restored.transact();
        assert_eq!(
            txn.get_map("documents")
                .unwrap()
                .get(&txn, "doc")
                .unwrap()
                .to_string(&txn),
            "old content"
        );
        assert_eq!(
            txn.get_map(RECEIPT_MAP)
                .unwrap()
                .get(&txn, "entry")
                .unwrap()
                .to_string(&txn),
            "receipt"
        );
    }

    #[test]
    fn restore_refuses_conflicts_and_unreadable_history() {
        let current = server();
        let target = server();
        target.get_or_insert_map(RECEIPT_MAP).insert(
            &mut target.transact_mut(),
            "entry",
            "different",
        );
        assert!(preserve_for_restore(&full(&current), &full(&target))
            .unwrap_err()
            .contains("conflicting"));
        assert!(preserve_for_restore(b"invalid current", &full(&target)).is_err());
        assert!(preserve_for_restore(&full(&current), b"opaque target").is_err());
        assert_eq!(
            preserve_for_restore(&full(&Doc::new()), b"opaque target").unwrap(),
            b"opaque target"
        );
    }

    #[test]
    fn restore_refuses_non_map_receipt_roots() {
        use yrs::{Array, XmlElementPrelim, XmlFragment};
        let text = Doc::new();
        text.get_or_insert_text(RECEIPT_MAP)
            .insert(&mut text.transact_mut(), 0, "not a map");
        let array = Doc::new();
        array
            .get_or_insert_array(RECEIPT_MAP)
            .push_back(&mut array.transact_mut(), "not a map");
        let xml = Doc::new();
        xml.get_or_insert_xml_fragment(RECEIPT_MAP).insert(
            &mut xml.transact_mut(),
            0,
            XmlElementPrelim::empty("not-a-map"),
        );
        for malformed in [text, array, xml] {
            assert!(preserve_for_restore(&full(&malformed), &full(&Doc::new()))
                .unwrap_err()
                .contains("malformed receipt root"));
        }
    }

    #[test]
    #[ignore = "manual release-build timing probe"]
    fn workspace_receipt_guard_timing() {
        let doc = server();
        let map = doc.get_or_insert_map("documents");
        {
            let mut txn = doc.transact_mut();
            for i in 0..10_000 {
                map.insert(&mut txn, format!("doc-{i}"), "x".repeat(400));
            }
        }
        let remote = clone_doc(&doc);
        let delta = {
            let mut txn = remote.transact_mut();
            txn.get_or_insert_map("folders")
                .insert(&mut txn, "new", "name");
            txn.encode_update_v1()
        };
        let start = std::time::Instant::now();
        for _ in 0..10 {
            validate_client_update(&doc, &delta).unwrap();
        }
        eprintln!(
            "workspace bytes={}, mean guard={:?}",
            full(&doc).len(),
            start.elapsed() / 10
        );
    }

    #[test]
    fn forged_removed_and_same_value_replacement_are_rejected() {
        let doc = server();
        let before = full(&doc);
        for action in 0..3 {
            let remote = clone_doc(&doc);
            let map = remote.get_or_insert_map(RECEIPT_MAP);
            let update = {
                let mut txn = remote.transact_mut();
                match action {
                    0 => {
                        map.insert(&mut txn, "forged", "well-formed attacker receipt");
                    }
                    1 => {
                        map.remove(&mut txn, "entry");
                    }
                    _ => {
                        map.insert(&mut txn, "entry", "receipt");
                    }
                }
                txn.encode_update_v1()
            };
            assert!(
                validate_client_update(&doc, &update).is_err(),
                "action {action}"
            );
            assert_eq!(full(&doc), before);
        }
    }

    #[test]
    fn first_receipt_and_alternate_root_type_are_rejected() {
        let doc = Doc::new();
        let remote = server();
        assert!(validate_client_update(&doc, &full(&remote)).is_err());
        let remote = Doc::new();
        remote
            .get_or_insert_text(RECEIPT_MAP)
            .insert(&mut remote.transact_mut(), 0, "forged");
        assert!(validate_client_update(&doc, &full(&remote)).is_err());
    }

    #[test]
    fn deferred_insertion_and_deletion_never_enter_live_state() {
        let doc = Doc::new();
        let remote = Doc::new();
        let nested = remote.get_or_insert_map(RECEIPT_MAP).insert(
            &mut remote.transact_mut(),
            "entry",
            MapPrelim::default(),
        );
        let update = {
            let mut txn = remote.transact_mut();
            nested.insert(&mut txn, "digest", "forged");
            txn.encode_update_v1()
        };
        let error = validate_client_update(&doc, &update).unwrap_err();
        assert!(error.contains("dependencies"), "{error}");
        let deleted = {
            let mut txn = remote.transact_mut();
            txn.get_or_insert_map(RECEIPT_MAP).remove(&mut txn, "entry");
            txn.encode_update_v1()
        };
        assert!(validate_client_update(&doc, &deleted)
            .unwrap_err()
            .contains("dependencies"));
        assert!(!doc.transact().has_missing_updates());
    }
}
