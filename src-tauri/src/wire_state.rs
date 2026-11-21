use crate::json_utils::json_string;

pub(crate) fn wire_is_active(wire: &serde_json::Value) -> bool {
    json_string(wire.get("deletedAt")).is_none()
        && json_string(wire.get("deleted_at")).is_none()
        && json_string(wire.get("tombstonedAt")).is_none()
        && json_string(wire.get("_tombstonedAt")).is_none()
        && !wire_expired(wire, chrono::Utc::now().timestamp_millis())
}

/// When a wire with a lifetime expires (dreamtOf), as epoch ms. A
/// materialized snapshot keeps it under `extra`; a live Y.Map read keeps it
/// at the top level.
pub(crate) fn wire_expires_at_ms(wire: &serde_json::Value) -> Option<i64> {
    let raw = wire
        .get("expiresAt")
        .filter(|v| !v.is_null())
        .or_else(|| wire.get("extra").and_then(|extra| extra.get("expiresAt")))?;
    if let Some(ms) = raw.as_f64() {
        return Some(ms as i64);
    }
    let text = raw.as_str()?.trim();
    chrono::DateTime::parse_from_rfc3339(text)
        .map(|at| at.timestamp_millis())
        .ok()
        .or_else(|| text.parse::<i64>().ok())
}

/// Past its expiry. Expired wires are deleted by the dream cycle and the
/// scheduler's sweep; until then every reader treats them as gone. An
/// expiry that can't be read never hides a wire.
pub(crate) fn wire_expired(wire: &serde_json::Value, now_ms: i64) -> bool {
    wire_expires_at_ms(wire).is_some_and(|at| now_ms >= at)
}

/// Active, and of a family that counts toward salience (record wires never
/// do). Every salience path counts wires through this.
pub(crate) fn wire_counts_toward_salience(wire: &serde_json::Value) -> bool {
    let family = crate::wire_predicates::WireFamily::of_wire(wire);
    wire_is_active(wire)
        && family.counts_toward_salience()
        // A wire of a family with a lifetime but no readable expiry did not
        // come from the server (a raw CRDT write can set any key): it weighs
        // nothing, and the sweep deletes it.
        && (family.lifetime_ms().is_none() || wire_expires_at_ms(wire).is_some())
}

pub(crate) fn wire_matches_document(
    wire: &serde_json::Value,
    document_id: &str,
    direction: &str,
) -> bool {
    let is_source = json_string(wire.get("sourceDocumentId")).as_deref() == Some(document_id);
    let is_target = json_string(wire.get("targetDocumentId")).as_deref() == Some(document_id);
    match direction {
        "outgoing" => is_source,
        "incoming" => is_target,
        "both" => is_source || is_target,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_activity_recognizes_deletion_aliases() {
        assert!(wire_is_active(&serde_json::json!({ "id": "wire-a" })));
        assert!(!wire_is_active(&serde_json::json!({ "deletedAt": "1000" })));
        assert!(!wire_is_active(
            &serde_json::json!({ "deleted_at": "1000" })
        ));
        assert!(!wire_is_active(
            &serde_json::json!({ "tombstonedAt": "1000" })
        ));
        assert!(!wire_is_active(
            &serde_json::json!({ "_tombstonedAt": "1000" })
        ));
    }

    #[test]
    fn expired_wires_are_inactive_and_record_wires_never_count() {
        let now = chrono::Utc::now();
        let iso = |at: chrono::DateTime<chrono::Utc>| at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let past = iso(now - chrono::Duration::seconds(1));
        let future = iso(now + chrono::Duration::minutes(1));
        let dream = |at: &str| serde_json::json!({ "predicate": "dreamtOf", "extra": { "expiresAt": at } });
        assert!(!wire_is_active(&dream(&past)));
        assert!(wire_is_active(&dream(&future)));
        assert!(wire_counts_toward_salience(&dream(&future)));
        // The live map's top-level form and a numeric form read the same way.
        assert!(!wire_is_active(&serde_json::json!({ "expiresAt": past })));
        assert!(!wire_is_active(&serde_json::json!({ "expiresAt": now.timestamp_millis() - 1 })));
        // An unreadable expiry never hides a wire, but a dream wire without
        // a server-stamped expiry weighs nothing.
        assert!(wire_is_active(&serde_json::json!({ "expiresAt": "soon" })));
        assert!(!wire_counts_toward_salience(&serde_json::json!({ "predicate": "dreamtOf" })));
        assert!(!wire_counts_toward_salience(&serde_json::json!({ "predicate": "dreamtOf", "expiresAt": "soon" })));
        assert!(!wire_counts_toward_salience(&serde_json::json!({ "predicate": "cites" })));
        assert!(!wire_counts_toward_salience(&serde_json::json!({ "predicate": "http://mnemosyne.ai/vocab#recalls" })));
        assert!(wire_counts_toward_salience(&serde_json::json!({ "predicate": "supports" })));
    }

    #[test]
    fn wire_matching_respects_direction_aliases() {
        let wire = serde_json::json!({
            "sourceDocumentId": "doc-a",
            "targetDocumentId": "doc-b",
        });

        assert!(wire_matches_document(&wire, "doc-a", "outgoing"));
        assert!(!wire_matches_document(&wire, "doc-a", "incoming"));
        assert!(wire_matches_document(&wire, "doc-b", "incoming"));
        assert!(wire_matches_document(&wire, "doc-b", "both"));
        assert!(!wire_matches_document(&wire, "doc-c", "both"));
    }
}
