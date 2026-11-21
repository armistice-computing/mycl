//! Render known metadata instants only. Never reinterpret user data, duration
//! counters, ordering numbers, RDF bindings, or stored values.
use chrono::{DateTime, Duration, Local, SecondsFormat, Utc};
use serde_json::Value;

/// attune() bundles three tools' results; each part is formatted exactly as
/// its own tool's result would be when called alone.
const ATTUNE_PARTS: &[(&str, &str)] = &[
    ("song", "music"),
    ("recall", "recall"),
    ("importantBlocks", "get_important_blocks"),
];

pub(super) fn format_timestamps(tool: &str, value: &mut Value) {
    if tool == "attune" {
        for (key, part_tool) in ATTUNE_PARTS {
            if let Some(part) = value.get_mut(*key) {
                format_timestamps(part_tool, part);
            }
        }
        return;
    }
    render(tool, value, |instant| {
        instant
            .with_timezone(&Local)
            .to_rfc3339_opts(SecondsFormat::AutoSi, false)
    });
}

fn is_raw_result(tool: &str) -> bool {
    matches!(
        tool,
        "sparql_query"
            | "sparql_query_named"
            | "rdf_dump"
            | "rdf_load"
            | "get_job_result"
            | "crdt_operation"
            | "source_pull"
            | "source_push"
            | "source_rebuild"
    ) || tool.starts_with("emporium_")
        || tool.starts_with("workflow_")
}

fn is_timestamp(key: &str) -> bool {
    // Not a suffix heuristic: haltedAt is a batch index, not an instant.
    matches!(
        key.replace('_', "").to_ascii_lowercase().as_str(),
        "timestamp"
            | "createdat"
            | "updatedat"
            | "lastactive"
            | "lastvaluated"
            | "lastvaluatedat"
            | "archivedat"
            | "observedat"
            | "firstobservedat"
            | "lastobservedat"
            | "firstat"
            | "lastat"
            | "lastforcedat"
            | "witnessedat"
            | "witnessedatiso"
            | "startedat"
            | "endedat"
            | "completedat"
            | "queuedat"
            | "enqueuedat"
            | "submittedat"
            | "requestedat"
            | "expiresat"
            | "revokedat"
            | "deletedat"
            | "tombstonedat"
            | "snapshotat"
            | "capturedat"
            | "capturedatms"
            | "plannedatms"
            | "generatedat"
            | "computedat"
            | "evaluatedat"
            | "materializedat"
            | "projectedat"
            | "describedat"
            | "appliedat"
            | "approvedat"
            | "proposedat"
            | "retractedat"
            | "registeredat"
            | "exportedat"
            | "grantedat"
            | "lastaccessedat"
            | "lastrunat"
            | "lastqueryat"
            | "lastupdateat"
            | "lastupdatedat"
            | "homegraphsetat"
    )
}

fn is_record_container(key: &str, tool: &str) -> bool {
    if key == "tree" {
        return tool == "get_workspace";
    }
    if key == "target" || key == "baseline" {
        return tool == "diff_restore_points";
    }
    matches!(
        key.replace('_', "").to_ascii_lowercase().as_str(),
        "entry"
            | "memories"
            | "diary"
            | "entries"
            | "documents"
            | "document"
            | "folders"
            | "artifacts"
            | "artifact"
            | "graphs"
            | "graph"
            | "blocks"
            | "results"
            | "snapshots"
            | "snapshot"
            | "wires"
            | "workspace"
            | "chronicle"
            | "recentauthors"
            | "recallingmemories"
            | "createdclock"
            | "updatedclock"
            | "children"
            | "freshness"
            | "revisions"
            | "revision"
            | "versions"
            | "items"
            | "jobs"
            | "job"
            | "restorepoints"
            | "restorepoint"
            | "operation"
            | "operations"
            | "history"
            | "confighistory"
            | "deleted"
            | "updated"
            | "session"
            | "sessions"
            | "profile"
            | "connector"
            | "connectors"
            | "configarchive"
    )
}

fn render(tool: &str, value: &mut Value, format: impl Fn(DateTime<Utc>) -> String + Copy) {
    if !is_raw_result(tool) {
        walk(tool, value, format, 0);
    }
}

fn walk(
    tool: &str,
    value: &mut Value,
    format: impl Fn(DateTime<Utc>) -> String + Copy,
    depth: usize,
) {
    if depth > 32 {
        return;
    }
    match value {
        Value::Array(rows) => {
            for row in rows {
                walk(tool, row, format, depth + 1);
            }
        }
        Value::Object(object) => {
            for (key, child) in object {
                if is_timestamp(key) {
                    if let Some(instant) = parse_instant(child) {
                        *child = Value::String(format(instant));
                    }
                } else if is_record_container(key, tool) {
                    walk(tool, child, format, depth + 1);
                }
            }
        }
        _ => {}
    }
}

/// Milliseconds may be a string or a number; legacy imports include fractional
/// milliseconds. Parse the decimal exactly rather than round through f64.
fn parse_instant(value: &Value) -> Option<DateTime<Utc>> {
    let text = match value {
        Value::String(text) => text.clone(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    if let Ok(iso) = DateTime::parse_from_rfc3339(&text) {
        return offset_safe(iso.with_timezone(&Utc));
    }
    let raw = text.trim();
    let negative = raw.starts_with('-');
    let raw = raw.strip_prefix('-').unwrap_or(raw);
    let (whole, fraction) = raw.split_once('.').unwrap_or((raw, ""));
    if whole.is_empty()
        || !whole.bytes().all(|c| c.is_ascii_digit())
        || !fraction.bytes().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let fraction = fraction.trim_end_matches('0');
    if fraction.len() > 6 {
        return None;
    } // Keep unrepresentable precision verbatim.
    let fractional_ns = if fraction.is_empty() {
        0
    } else {
        fraction
            .parse::<i128>()
            .ok()?
            .checked_mul(10i128.pow(6 - fraction.len() as u32))?
    };
    let ns = whole
        .parse::<i128>()
        .ok()?
        .checked_mul(1_000_000)?
        .checked_add(fractional_ns)?;
    let ns = if negative { ns.checked_neg()? } else { ns };
    let seconds: i64 = ns.div_euclid(1_000_000_000).try_into().ok()?;
    DateTime::from_timestamp(seconds, ns.rem_euclid(1_000_000_000) as u32).and_then(offset_safe)
}

fn offset_safe(instant: DateTime<Utc>) -> Option<DateTime<Utc>> {
    // Formatting local time adds an offset. Leave boundary instants verbatim
    // rather than risk overflow after an already-committed mutation.
    instant.checked_add_signed(Duration::days(2))?;
    instant.checked_sub_signed(Duration::days(2))?;
    Some(instant)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;
    use serde_json::json;

    fn pacific_summer(instant: DateTime<Utc>) -> String {
        instant
            .with_timezone(&FixedOffset::west_opt(7 * 3600).unwrap())
            .to_rfc3339_opts(SecondsFormat::AutoSi, false)
    }

    #[test]
    fn attune_parts_are_formatted_as_their_own_tools_are() {
        let recall = json!({
            "memories": [{"number": 1, "createdAt": "2026-09-30T23:44:36.070Z", "lastActive": "2026-09-30T23:44:36.070Z"}],
            "diary": {"entries": [{"number": 3, "createdAt": "2026-09-30T23:46:36.028Z"}]},
        });
        let blocks = json!({"blocks": [{"blockId": "b", "lastValuatedAt": "2026-09-30T23:40:00Z"}]});
        let mut alone_recall = recall.clone();
        format_timestamps("recall", &mut alone_recall);
        let mut alone_blocks = blocks.clone();
        format_timestamps("get_important_blocks", &mut alone_blocks);
        assert_ne!(alone_recall, recall, "the standalone formatter does change these");

        let mut attuned = json!({"graphId": "g", "song": {"verses": ["v"]}, "recall": recall, "importantBlocks": blocks});
        format_timestamps("attune", &mut attuned);
        assert_eq!(attuned["recall"], alone_recall);
        assert_eq!(attuned["importantBlocks"], alone_blocks);
        assert_eq!(attuned["graphId"], "g");
    }

    #[test]
    fn milliseconds_fractional_milliseconds_and_iso_preserve_the_instant() {
        assert!(parse_instant(&json!(DateTime::<Utc>::MIN_UTC.timestamp_millis())).is_none());
        assert!(parse_instant(&json!(DateTime::<Utc>::MAX_UTC.timestamp_millis())).is_none());
        for value in [
            json!(1789832555813i64),
            json!("1789832555813"),
            json!(1789832555813.125),
            json!("1789832555813.125"),
            json!("2026-09-19T15:42:35.813125Z"),
            json!("-0.5"),
        ] {
            let instant = parse_instant(&value).unwrap();
            let local = pacific_summer(instant);
            assert_eq!(
                DateTime::parse_from_rfc3339(&local)
                    .unwrap()
                    .with_timezone(&Utc),
                instant
            );
        }
        assert_eq!(
            parse_instant(&json!("1789832555813.125"))
                .unwrap()
                .timestamp_subsec_nanos(),
            813_125_000
        );
        assert_eq!(
            parse_instant(&json!("-0.5"))
                .unwrap()
                .timestamp_subsec_nanos(),
            999_500_000
        );
        for value in [
            json!(null),
            json!("unknown"),
            json!("2026-09-19"),
            json!("999999999999999999999999999999999999999999"),
            json!("1.1234567"),
        ] {
            assert!(parse_instant(&value).is_none());
        }
    }

    #[test]
    fn metadata_is_readable_but_user_bags_and_numeric_controls_are_untouched() {
        let mut value = json!({"createdAt":"1789832555813","order":1789832555813i64,"haltedAt":2,
          "durationMs":300,"cursor":"1789832555813","content":"1789832555813",
          "attrs":{"createdAt":1789832555813i64},"metadata":{"updatedAt":1789832555813i64},
          "chronicle":{"updatedClock":{"witnessedAt":"2026-09-19T15:42:35.813Z","stanza":3.5}},
          "recallingMemories":{"memories":[{"createdAt":"1789832555813"}]}});
        let original = value.clone();
        render("read_document", &mut value, pacific_summer);
        assert!(value["createdAt"].as_str().unwrap().ends_with("-07:00"));
        for key in [
            "order",
            "haltedAt",
            "durationMs",
            "cursor",
            "content",
            "attrs",
            "metadata",
        ] {
            assert_eq!(value[key], original[key]);
        }
        assert!(value["recallingMemories"]["memories"][0]["createdAt"]
            .as_str()
            .unwrap()
            .contains('T'));
        let again = value.clone();
        render("read_document", &mut value, pacific_summer);
        assert_eq!(again, value);
    }

    #[test]
    fn tool_families_and_aliases_share_the_formatter() {
        for tool in [
            "recall",
            "remember",
            "music",
            "get_workspace",
            "read_document",
            "document_digest",
            "get_block_values",
            "value",
            "search_documents",
            "get_document_history",
            "read_document_at_snapshot",
            "get_wires",
            "get_user_location",
            "get_job_status",
            "list_restore_points",
            "list_artifact_revisions",
            "read_diary",
        ] {
            let mut value = json!({"timestamp":"1789832555813","freshness":{"updated_at":"1789832555813"},"blocks":[{"lastValuated":"1789832555813","last_valuated_at":"1789832555813"}],"folders":[{"updatedAt":1789832555813.125}]});
            render(tool, &mut value, pacific_summer);
            assert!(value["timestamp"].as_str().unwrap().contains('T'), "{tool}");
            assert_eq!(
                value["blocks"][0]["lastValuated"],
                value["blocks"][0]["last_valuated_at"]
            );
            assert!(value["freshness"]["updated_at"]
                .as_str()
                .unwrap()
                .ends_with("-07:00"));
        }
    }

    #[test]
    fn raw_query_and_job_payloads_are_not_reinterpreted() {
        let original =
            json!({"createdAt":1789832555813i64,"results":[{"timestamp":"1789832555813"}]});
        for tool in [
            "sparql_query",
            "get_job_result",
            "crdt_operation",
            "source_pull",
            "emporium_read",
        ] {
            let mut value = original.clone();
            render(tool, &mut value, pacific_summer);
            assert_eq!(value, original, "{tool}");
        }
    }

    #[test]
    fn local_offsets_and_fractional_instants_survive_machine_agent_round_trip() {
        let original = json!({"createdAt":"1789832555813.125","timestamp":1789832555813i64,"content":"1789832555813"});
        let machine = crate::mcp_agent_presentation::ResponseView::Machine
            .envelope("read_document", original.clone());
        let agent = crate::mcp_agent_presentation::ResponseView::AgentV1
            .envelope("read_document", original.clone());
        assert_eq!(machine["structuredContent"], original);
        let expected = parse_instant(&original["createdAt"]).unwrap();
        let actual =
            DateTime::parse_from_rfc3339(agent["structuredContent"]["createdAt"].as_str().unwrap())
                .unwrap();
        assert_eq!(actual.with_timezone(&Utc), expected);
        assert_eq!(
            actual.offset().local_minus_utc(),
            expected.with_timezone(&Local).offset().local_minus_utc()
        );
        assert_eq!(agent["structuredContent"]["content"], original["content"]);
        assert_eq!(
            serde_json::from_str::<Value>(agent["content"][0]["text"].as_str().unwrap()).unwrap(),
            agent["structuredContent"]
        );
    }
}
