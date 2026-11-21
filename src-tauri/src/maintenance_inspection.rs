//! Bounded, file-only diagnostics. No owner startup, database, repair or pruning.
//! Receipts are observations, never deletion plans or proof of recoverability.

use crate::{
    crdt_operation_journal::{
        CrdtOperationJournalEvent, CrdtOperationJournalStatus as Status,
        LOCAL_CRDT_JOURNAL_RETENTION_DAYS,
    },
    operation_completion_ledger::{
        OperationCompletionEntry, LOCAL_COMPLETION_LEDGER_RETENTION_DAYS,
    },
};
use serde_json::{json, Value};
use std::{collections::BTreeMap, fs, io::Read, path::Path};

const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
const DAY_MS: u128 = 86_400_000;

struct Observation {
    receipt: Value,
    text: Option<String>,
}

fn observe(path: &Path) -> Observation {
    let result = (|| -> Result<Option<String>, &'static str> {
        let before = match fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Some(String::new()))
            }
            Err(_) => return Err("unreadable"),
        };
        if !before.file_type().is_file() {
            return Err("not_regular_file");
        }
        if before.len() > MAX_FILE_BYTES {
            return Err("byte_budget_exceeded");
        }
        let file = fs::File::open(path).map_err(|_| "unreadable")?;
        let mut bytes = Vec::new();
        file.take(MAX_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "unreadable")?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err("byte_budget_exceeded");
        }
        let after = fs::symlink_metadata(path).map_err(|_| "changed_during_read")?;
        if !after.file_type().is_file()
            || before.len() != after.len()
            || before.modified().ok() != after.modified().ok()
            || bytes.len() as u64 != after.len()
        {
            return Err("changed_during_read");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if before.ino() != after.ino() || before.dev() != after.dev() {
                return Err("changed_during_read");
            }
        }
        String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| "invalid_utf8")
    })();
    match result {
        Ok(text) => Observation {
            receipt: json!({"status":"observed","bytesRead":text.as_ref().map_or(0, |s|s.len())}),
            text,
        },
        Err(reason) => Observation {
            receipt: json!({"status":"unknown","reason":reason}),
            text: None,
        },
    }
}

/// Inspect an explicitly selected profile, without opening Garden or taking its
/// lock. Bounded best-effort reads can run alongside the owner; no atomic
/// cross-file snapshot is promised. The caller may save the returned receipt.
pub fn inspect_profile(profile: &Path) -> Result<Value, String> {
    inspect_at(profile, crate::clock::epoch_millis())
}

fn inspect_at(profile: &Path, now: u128) -> Result<Value, String> {
    if !profile.is_dir() {
        return Err("maintenance inspection requires an existing profile directory".into());
    }
    let worklog = profile.join("worklog");
    match fs::symlink_metadata(&worklog) {
        Ok(meta) if !meta.file_type().is_dir() => {
            return Err("worklog must be a real directory, not a symlink".into())
        }
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            return Err("cannot inspect worklog directory".into())
        }
        _ => {}
    }
    let journal = observe(&worklog.join("crdt-operations.jsonl"));
    let ledger = observe(&worklog.join("operation-completion.jsonl"));
    let mut states: BTreeMap<String, Option<u128>> = BTreeMap::new();
    let mut journal_unknown = 0usize;
    for line in journal
        .text
        .as_deref()
        .unwrap_or("")
        .lines()
        .filter(|line| !line.trim().is_empty())
    {
        let event = match serde_json::from_str::<CrdtOperationJournalEvent>(line) {
            Ok(event) if event.schema_version == 1 && !event.operation_id.is_empty() => event,
            _ => {
                journal_unknown += 1;
                continue;
            }
        };
        let Some(at) = crate::clock::parse_timestamp(&event.timestamp) else {
            journal_unknown += 1;
            continue;
        };
        match event.status {
            Status::Queued => {
                if event.operation.as_ref().map(|op| op.operation_id.as_str())
                    != Some(event.operation_id.as_str())
                {
                    journal_unknown += 1;
                }
                states.insert(event.operation_id, None);
            }
            Status::CallerTimedOut => {
                states.entry(event.operation_id).or_insert(None);
            }
            Status::Succeeded | Status::Failed => {
                states.insert(event.operation_id, Some(at));
            }
        }
    }
    let pending = states.values().filter(|v| v.is_none()).count();
    let journal_complete = journal.text.is_some() && journal_unknown == 0;
    let mut ledger_unknown = 0usize;
    let mut old_markers = 0usize;
    let mut protected_markers = 0usize;
    for line in ledger
        .text
        .as_deref()
        .unwrap_or("")
        .lines()
        .filter(|line| !line.trim().is_empty())
    {
        let entry = match serde_json::from_str::<OperationCompletionEntry>(line) {
            Ok(entry) if entry.schema_version == 1 && !entry.operation_id.is_empty() => entry,
            _ => {
                ledger_unknown += 1;
                continue;
            }
        };
        let Some(at) = crate::clock::parse_timestamp(&entry.completed_at) else {
            ledger_unknown += 1;
            continue;
        };
        if now.saturating_sub(at) > u128::from(LOCAL_COMPLETION_LEDGER_RETENTION_DAYS) * DAY_MS {
            old_markers += 1;
            if matches!(states.get(&entry.operation_id), Some(None)) {
                protected_markers += 1;
            }
        }
    }
    let complete = journal_complete && ledger.text.is_some() && ledger_unknown == 0;
    let old_terminal = states
        .values()
        .flatten()
        .filter(|at| {
            now.saturating_sub(**at) > u128::from(LOCAL_CRDT_JOURNAL_RETENTION_DAYS) * DAY_MS
        })
        .count();
    Ok(json!({
        "schemaVersion":1,"mode":"inspect","scope":"profile","routine":"retention-observation",
        "observedAt":now.to_string(),"status":if complete {"observed"} else {"incomplete"},
        "consistency":"best_effort_not_atomic","mutationsPerformed":0,"pruneAuthorized":false,
        "budget":{"maxBytesPerFile":MAX_FILE_BYTES},
        "journal":{"file":journal.receipt,"unknownLines":journal_unknown,"unresolvedOperationIds":pending,
            "retentionDays":LOCAL_CRDT_JOURNAL_RETENTION_DAYS,"oldTerminalOperationIds":if journal_complete {Some(old_terminal)} else {None}},
        "completionLedger":{"file":ledger.receipt,"unknownLines":ledger_unknown,"retentionDays":LOCAL_COMPLETION_LEDGER_RETENTION_DAYS,
            "oldEntriesObserved":old_markers,"oldEntriesReferencedByUnresolvedOperations":protected_markers},
        "notes":["Age is not deletion permission. Pending work and its completion markers need protection.",
            "No graph history, CRDT, RDF, vector index, or jobs database was opened. Missing files are empty observations, not proof of healthy recovery.",
            "Existing startup pruning is unchanged; this receipt is not its dry-run plan."]
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_logs_do_not_create_anything() {
        let dir = std::env::temp_dir().join(format!("inspect-empty-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        let report = inspect_at(&dir, 20 * DAY_MS).unwrap();
        assert_eq!(report["mutationsPerformed"], 0);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn uncertain_lines_and_pending_markers_are_not_cleanup_permission() {
        let dir = std::env::temp_dir().join(format!("inspect-log-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(dir.join("worklog")).unwrap();
        let journal = "{\"schemaVersion\":1,\"eventId\":\"e\",\"timestamp\":\"1\",\"operationId\":\"pending\",\"status\":\"callerTimedOut\"}\nBROKEN\n";
        let ledger = "{\"schemaVersion\":1,\"operationId\":\"pending\",\"kind\":\"import.vault\",\"completedAt\":\"1\"}\n";
        fs::write(dir.join("worklog/crdt-operations.jsonl"), journal).unwrap();
        fs::write(dir.join("worklog/operation-completion.jsonl"), ledger).unwrap();
        let report = inspect_at(&dir, 20 * DAY_MS).unwrap();
        assert_eq!(report["status"], "incomplete");
        assert_eq!(report["journal"]["unknownLines"], 1);
        assert_eq!(report["journal"]["unresolvedOperationIds"], 1);
        assert!(report["journal"]["oldTerminalOperationIds"].is_null());
        assert_eq!(
            report["completionLedger"]["oldEntriesReferencedByUnresolvedOperations"],
            1
        );
        assert_eq!(
            fs::read_to_string(dir.join("worklog/crdt-operations.jsonl")).unwrap(),
            journal
        );
        assert_eq!(
            fs::read_to_string(dir.join("worklog/operation-completion.jsonl")).unwrap(),
            ledger
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn budgets_and_symlinks_fail_closed() {
        let dir = std::env::temp_dir().join(format!("inspect-bound-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(dir.join("worklog")).unwrap();
        let path = dir.join("worklog/crdt-operations.jsonl");
        fs::File::create(&path)
            .unwrap()
            .set_len(MAX_FILE_BYTES + 1)
            .unwrap();
        assert_eq!(
            inspect_at(&dir, 0).unwrap()["journal"]["file"]["reason"],
            "byte_budget_exceeded"
        );
        #[cfg(unix)]
        {
            fs::remove_file(&path).unwrap();
            std::os::unix::fs::symlink("missing-target", &path).unwrap();
            assert_eq!(
                inspect_at(&dir, 0).unwrap()["journal"]["file"]["reason"],
                "not_regular_file"
            );
        }
        fs::remove_dir_all(dir).unwrap();
    }
}
