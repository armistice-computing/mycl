use crate::{
    clock::parse_timestamp,
    document_history_store::{
        LocalDocumentHistoryStore, LocalDocumentSnapshotMeta, LocalDocumentSnapshotPayload,
        HISTORY_TIERS, HISTORY_TIER_MAX_SLOTS,
    },
};
use std::path::Path;

pub(super) use crate::document_history_file_store::{
    read_document_history_store, read_document_snapshot_payload, remove_snapshot_payload,
    write_document_history_store,
};

pub(super) fn history_tier_label(tier: &str) -> &'static str {
    match tier {
        "20min" => "20 min",
        "2h" => "2 hr",
        "12h" => "12 hr",
        "daily" => "daily",
        "weekly" => "weekly",
        _ => "",
    }
}

pub(super) fn history_sort_key(snapshot: &LocalDocumentSnapshotMeta) -> u128 {
    parse_timestamp(&snapshot.created_at).unwrap_or(0)
}

/// The parent window a snapshot belongs to, cut in the owner's LOCAL time:
/// a day is her day, not UTC's (UTC midnight is 4 or 5 pm in California, so
/// an evening's writing used to straddle two "days"). Weeks start on Monday.
fn history_parent_window_key(snapshot: &LocalDocumentSnapshotMeta, from_tier_idx: usize) -> u128 {
    window_key_at(history_sort_key(snapshot), local_offset_millis, from_tier_idx)
}

fn local_offset_millis(utc_millis: u128) -> i64 {
    use chrono::{Local, Offset, TimeZone};
    i64::try_from(utc_millis)
        .ok()
        .and_then(|millis| Local.timestamp_millis_opt(millis).single())
        .map(|local| i64::from(local.offset().fix().local_minus_utc()) * 1000)
        .unwrap_or(0)
}

fn window_key_at(utc_millis: u128, offset_at: impl Fn(u128) -> i64, from_tier_idx: usize) -> u128 {
    let local = (utc_millis as i128 + i128::from(offset_at(utc_millis))).max(0) as u128;
    let hour = 60 * 60 * 1000_u128;
    let day = 24 * hour;
    match from_tier_idx {
        0 => local / (2 * hour),
        1 => local / (12 * hour),
        2 => local / day,
        // 1970-01-01 was a Thursday; shifting by three days starts weeks on Monday.
        3 => (local + 3 * day) / (7 * day),
        _ => local,
    }
}

pub(super) fn newest_snapshot_payload(
    graph_dir: &Path,
    store: &LocalDocumentHistoryStore,
) -> Result<Option<LocalDocumentSnapshotPayload>, String> {
    // `snapshots` is the durable append order. Do not use millisecond
    // timestamps to choose the predecessor: multiple commits may share one
    // clock value.
    for snapshot in store.snapshots.iter().rev() {
        if let Ok(payload) =
            read_document_snapshot_payload(graph_dir, &store.document_id, &snapshot.snapshot_id)
        {
            return Ok(Some(payload));
        }
    }
    Ok(None)
}

fn evict_history_snapshot_upward(
    store: &mut LocalDocumentHistoryStore,
    snapshot_id: &str,
    from_tier_idx: usize,
    obsolete_payloads: &mut Vec<String>,
) -> usize {
    let next_tier_idx = from_tier_idx + 1;
    if next_tier_idx >= HISTORY_TIERS.len() {
        return 0;
    }
    let next_tier = HISTORY_TIERS[next_tier_idx];
    let Some(victim_pos) = store
        .snapshots
        .iter()
        .position(|snapshot| snapshot.snapshot_id == snapshot_id)
    else {
        return 0;
    };
    let victim = store.snapshots[victim_pos].clone();
    let victim_window = history_parent_window_key(&victim, from_tier_idx);

    if let Some(anchor_pos) = store.snapshots.iter().position(|snapshot| {
        snapshot.snapshot_id != snapshot_id
            && snapshot.tier == next_tier
            && history_parent_window_key(snapshot, from_tier_idx) == victim_window
    }) {
        // The window keeps its OLDEST state (Eschaton's design): each coarse
        // entry is how the document looked before that window's work.
        let victim_count = victim.snapshot_count.max(1);
        store.snapshots[anchor_pos].snapshot_count = store.snapshots[anchor_pos]
            .snapshot_count
            .saturating_add(victim_count);
        store.snapshots.remove(victim_pos);
        obsolete_payloads.push(snapshot_id.to_string());
        return 1;
    }

    let auto_in_next = store
        .snapshots
        .iter()
        .filter(|snapshot| snapshot.tier == next_tier && !snapshot.is_manual)
        .count();
    if next_tier == "weekly" || auto_in_next < HISTORY_TIER_MAX_SLOTS {
        if let Some(snapshot) = store
            .snapshots
            .iter_mut()
            .find(|snapshot| snapshot.snapshot_id == snapshot_id)
        {
            snapshot.tier = next_tier.to_string();
        }
        return 0;
    }

    let Some(oldest_next_id) = store
        .snapshots
        .iter()
        .filter(|snapshot| snapshot.tier == next_tier && !snapshot.is_manual)
        .min_by_key(|snapshot| history_sort_key(snapshot))
        .map(|snapshot| snapshot.snapshot_id.clone())
    else {
        return 0;
    };
    let deleted =
        evict_history_snapshot_upward(store, &oldest_next_id, next_tier_idx, obsolete_payloads);
    if let Some(snapshot) = store
        .snapshots
        .iter_mut()
        .find(|snapshot| snapshot.snapshot_id == snapshot_id)
    {
        snapshot.tier = next_tier.to_string();
    }
    deleted
}

pub(super) fn collapse_document_history(store: &mut LocalDocumentHistoryStore) -> Vec<String> {
    let mut obsolete_payloads = Vec::new();
    for tier_idx in 0..HISTORY_TIERS.len().saturating_sub(1) {
        let tier = HISTORY_TIERS[tier_idx];
        for _ in 0..store.snapshots.len() {
            let auto_count = store
                .snapshots
                .iter()
                .filter(|snapshot| snapshot.tier == tier && !snapshot.is_manual)
                .count();
            if auto_count <= HISTORY_TIER_MAX_SLOTS {
                break;
            }
            let Some(oldest_id) = store
                .snapshots
                .iter()
                .filter(|snapshot| snapshot.tier == tier && !snapshot.is_manual)
                .min_by_key(|snapshot| history_sort_key(snapshot))
                .map(|snapshot| snapshot.snapshot_id.clone())
            else {
                break;
            };
            evict_history_snapshot_upward(store, &oldest_id, tier_idx, &mut obsolete_payloads);
        }
    }
    obsolete_payloads
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(
        id: &str,
        tier: &str,
        created_at_millis: u128,
        manual: bool,
    ) -> LocalDocumentSnapshotMeta {
        LocalDocumentSnapshotMeta {
            snapshot_id: id.to_string(),
            graph_id: "g".to_string(),
            document_id: "d".to_string(),
            is_manual: manual,
            document_revision: None,
            tier: tier.to_string(),
            snapshot_count: 1,
            chars_added: 0,
            chars_removed: 0,
            blocks_added: 0,
            blocks_removed: 0,
            blocks_modified: 0,
            created_at: created_at_millis.to_string(),
            label: None,
            opened_at: None,
        }
    }

    fn store(snapshots: Vec<LocalDocumentSnapshotMeta>) -> LocalDocumentHistoryStore {
        LocalDocumentHistoryStore {
            schema_version: 3,
            graph_id: "g".to_string(),
            document_id: "d".to_string(),
            total_count: snapshots.len() as u64,
            latest_automatic_revision: None,
            latest_automatic_snapshot_id: None,
            snapshots,
        }
    }

    // One 2-hour window, aligned to its start.
    const WINDOW: u128 = 1_000 * 2 * 60 * 60 * 1000;
    const MINUTE: u128 = 60 * 1000;

    #[test]
    fn a_coarse_entry_keeps_the_oldest_state_of_its_window() {
        let mut history = store(vec![
            snapshot("anchor", "2h", WINDOW + MINUTE, false),
            snapshot("s1", "20min", WINDOW + 10 * MINUTE, false),
            snapshot("s2", "20min", WINDOW + 20 * MINUTE, false),
            snapshot("s3", "20min", WINDOW + 30 * MINUTE, false),
            snapshot("s4", "20min", WINDOW + 40 * MINUTE, false),
            snapshot("s5", "20min", WINDOW + 50 * MINUTE, false),
        ]);
        let obsolete = collapse_document_history(&mut history);
        assert_eq!(obsolete, vec!["s1".to_string()]);
        let coarse = history.snapshots.iter().find(|s| s.tier == "2h").unwrap();
        assert_eq!(coarse.snapshot_id, "anchor");
        assert_eq!(coarse.snapshot_count, 2);
    }

    #[test]
    fn windows_are_cut_at_local_midnight_and_weeks_start_monday() {
        const HOUR: u128 = 60 * MINUTE;
        const DAY: u128 = 24 * HOUR;
        let pacific = |_: u128| -7 * 60 * 60 * 1000_i64;
        // 2026-10-16 16:30 and 17:30 Pacific straddle UTC midnight but are
        // the same local day.
        let oct16 = 20_742 * DAY; // 2026-10-16 00:00 UTC (a Friday)
        let a = oct16 + 23 * HOUR + 30 * MINUTE; // 16:30 PDT
        let b = oct16 + DAY + 30 * MINUTE; // 17:30 PDT
        assert_eq!(window_key_at(a, pacific, 2), window_key_at(b, pacific, 2));
        assert_ne!(window_key_at(a, |_| 0, 2), window_key_at(b, |_| 0, 2));
        // Sunday 2026-10-18 and Monday 2026-10-19 (local noon) are in
        // different weeks; Monday and Tuesday share one.
        let noon = |day: u128| oct16 + day * DAY + 19 * HOUR;
        assert_ne!(window_key_at(noon(2), pacific, 3), window_key_at(noon(3), pacific, 3));
        assert_eq!(window_key_at(noon(3), pacific, 3), window_key_at(noon(4), pacific, 3));
    }

    #[test]
    fn a_manual_anchor_is_never_removed() {
        let mut history = store(vec![
            snapshot("manual", "2h", WINDOW + MINUTE, true),
            snapshot("s1", "20min", WINDOW + 10 * MINUTE, false),
            snapshot("s2", "20min", WINDOW + 20 * MINUTE, false),
            snapshot("s3", "20min", WINDOW + 30 * MINUTE, false),
            snapshot("s4", "20min", WINDOW + 40 * MINUTE, false),
            snapshot("s5", "20min", WINDOW + 50 * MINUTE, false),
        ]);
        let obsolete = collapse_document_history(&mut history);
        assert!(!obsolete.contains(&"manual".to_string()));
        assert!(history.snapshots.iter().any(|s| s.snapshot_id == "manual"));
    }
}
