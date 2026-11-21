use crate::{
    clock::parse_timestamp, document_service::DocumentRecord, json_utils::json_string,
    salience_value_store::VALUE_UNKNOWN_AGE_DAYS, wire_state::wire_counts_toward_salience,
};
use std::collections::BTreeMap;

fn parse_value_timestamp(value: &str) -> Option<u128> {
    if let Some(value) = parse_timestamp(value.trim()) {
        return Some(if value < 10_000_000_000 {
            value * 1000
        } else {
            value
        });
    }
    chrono::DateTime::parse_from_rfc3339(value.trim())
        .ok()
        .and_then(|value| u128::try_from(value.timestamp_millis()).ok())
}

pub(super) fn valid_value_timestamp(value: &str) -> bool {
    parse_value_timestamp(value).is_some()
}

pub(super) fn newer_value_timestamp(
    current: Option<String>,
    candidate: Option<String>,
) -> Option<String> {
    let Some(candidate) = candidate else {
        return current;
    };
    match current {
        Some(current) => {
            let current_num = parse_value_timestamp(&current).unwrap_or(0);
            let candidate_num = parse_value_timestamp(&candidate).unwrap_or(0);
            if candidate_num > current_num {
                Some(candidate)
            } else {
                Some(current)
            }
        }
        None => Some(candidate),
    }
}

pub(super) fn value_wire_timestamp(wire: &serde_json::Value) -> Option<String> {
    json_string(
        wire.get("createdAt")
            .or_else(|| wire.get("created_at"))
            .or_else(|| wire.get("snapshotAt"))
            .or_else(|| wire.get("updatedAt")),
    )
}

pub(super) fn value_wire_stats(
    wires: &[serde_json::Value],
    document_id: &str,
    block_id: &str,
) -> (usize, usize, Option<String>) {
    let mut doc_count = 0usize;
    let mut block_count = 0usize;
    let mut newest_wire = None;
    for wire in wires {
        if !wire_counts_toward_salience(wire) {
            continue;
        }
        if json_string(wire.get("id"))
            .map(|id| id.ends_with("-inv"))
            .unwrap_or(false)
        {
            continue;
        }
        let source_document_id = json_string(wire.get("sourceDocumentId"));
        let target_document_id = json_string(wire.get("targetDocumentId"));
        let source_block_id = json_string(wire.get("sourceBlockId"));
        let target_block_id = json_string(wire.get("targetBlockId"));
        let document_matches = source_document_id.as_deref() == Some(document_id)
            || target_document_id.as_deref() == Some(document_id);
        if document_matches {
            doc_count += 1;
            newest_wire = newer_value_timestamp(newest_wire, value_wire_timestamp(wire));
        }
        let block_matches = (source_document_id.as_deref() == Some(document_id)
            && source_block_id.as_deref() == Some(block_id))
            || (target_document_id.as_deref() == Some(document_id)
                && target_block_id.as_deref() == Some(block_id));
        if block_matches {
            block_count += 1;
        }
    }
    (block_count, doc_count, newest_wire)
}

pub(super) fn value_timestamp_age_days(value: Option<&str>, now_ms: u128) -> f64 {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return VALUE_UNKNOWN_AGE_DAYS;
    };
    let Some(timestamp) = parse_value_timestamp(value) else {
        return VALUE_UNKNOWN_AGE_DAYS;
    };
    if timestamp > now_ms {
        return 0.0;
    }
    (now_ms - timestamp) as f64 / 86_400_000.0
}

#[cfg(test)]
mod import_timestamp_tests {
    use super::*;

    #[test]
    fn record_wires_never_add_weight_and_dream_wires_do() {
        let wire = |id: &str, predicate: &str| {
            serde_json::json!({
                "id": id, "sourceDocumentId": "a", "targetDocumentId": "doc", "targetBlockId": "blk", "predicate": predicate,
            })
        };
        let wires = vec![
            wire("w1", "supports"),
            wire("w2", "cites"),
            wire("w3", "http://mnemosyne.ai/vocab#recalls"),
            wire("w4", "authoredBy"),
            {
                let mut dream = wire("w5", "dreamtOf");
                dream["extra"] = serde_json::json!({ "expiresAt": "2999-01-01T00:00:00.000Z" });
                dream
            },
            // No server-stamped expiry: not the server's, weighs nothing.
            wire("w6", "dreamtOf"),
        ];
        let (block, doc, _) = value_wire_stats(&wires, "doc", "blk");
        assert_eq!((block, doc), (2, 2));
    }

    #[test]
    fn legacy_rfc3339_and_native_epoch_have_equal_age() {
        let instant = "2026-09-08T00:00:00.123456+00:00";
        let millis = chrono::DateTime::parse_from_rfc3339(instant)
            .unwrap()
            .timestamp_millis() as u128;
        let now = millis + 86_400_000;
        assert_eq!(value_timestamp_age_days(Some(instant), now), 1.0);
        assert_eq!(
            value_timestamp_age_days(Some(&millis.to_string()), now),
            1.0
        );
        assert_eq!(
            value_timestamp_age_days(Some("nonsense"), now),
            VALUE_UNKNOWN_AGE_DAYS
        );
        assert_eq!(value_timestamp_age_days(None, now), VALUE_UNKNOWN_AGE_DAYS);
    }

    #[test]
    fn newest_wire_compares_mixed_timestamp_formats() {
        assert_eq!(
            newer_value_timestamp(
                Some("1788825600000".into()),
                Some("2026-09-09T00:00:00Z".into())
            ),
            Some("2026-09-09T00:00:00Z".into())
        );
    }
}

/// Document id → creation timestamp, as the document record stores it. The
/// only fact scoring needs from a document.
pub(super) type DocumentCreatedAt = BTreeMap<String, String>;

pub(super) fn with_graph_creation_fallback(
    mut dates: DocumentCreatedAt,
    graph_created_at: &str,
) -> DocumentCreatedAt {
    for date in dates.values_mut() {
        if !valid_value_timestamp(date) {
            *date = graph_created_at.to_string();
        }
    }
    dates
}

pub(super) fn document_created_at_map<'a>(
    documents: impl IntoIterator<Item = &'a DocumentRecord>,
) -> DocumentCreatedAt {
    documents
        .into_iter()
        .map(|document| (document.document_id.clone(), document.created_at.clone()))
        .collect()
}

pub(super) fn document_created_age_days(
    created_at: &DocumentCreatedAt,
    document_id: &str,
    fallback_timestamp: Option<&str>,
    now_ms: u128,
) -> f64 {
    let timestamp = fallback_timestamp
        .filter(|value| parse_value_timestamp(value).is_some())
        .or_else(|| created_at.get(document_id).map(String::as_str));
    value_timestamp_age_days(timestamp, now_ms)
}

#[cfg(test)]
mod attention_date_tests {
    use super::*;
    #[test]
    fn invalid_or_missing_attention_dates_fall_back_to_creation_not_now() {
        let dates = BTreeMap::from([("d".into(), "2026-01-01T00:00:00Z".into())]);
        let now = parse_value_timestamp("2026-01-11T00:00:00Z").unwrap();
        assert_eq!(document_created_age_days(&dates, "d", None, now), 10.0);
        assert_eq!(
            document_created_age_days(&dates, "d", Some("invalid"), now),
            10.0
        );
        assert_eq!(
            document_created_age_days(&dates, "d", Some("2026-01-10T00:00:00Z"), now),
            1.0
        );
        let dates = with_graph_creation_fallback(
            BTreeMap::from([("d".into(), "invalid".into())]),
            "2026-01-01T00:00:00Z",
        );
        assert_eq!(document_created_age_days(&dates, "d", None, now), 10.0);
    }
}
