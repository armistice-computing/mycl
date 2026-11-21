//! Diagnose lost in-process workers once, during locked owner startup.
//! This does not replay jobs or cancel their separately durable CRDT operations.

use crate::{
    clock::epoch_millis,
    local_job_store::{load_job_records, persist_job_record},
    local_job_types::{local_job_status_label, LocalJobRecord, LocalJobStatus},
};
use std::path::Path;

const MESSAGE: &str = "Job worker was interrupted by an owner restart. Its outcome is unknown; changes may already have committed or may still complete through durable recovery. Inspect before retrying.";

/// Must run under the profile lock, before any new workers or API are started.
/// Never call from a registry constructor: polling also constructs registries.
pub(crate) fn recover_interrupted_jobs_on_startup(jobs_dir: &Path) -> Result<usize, String> {
    let records = load_job_records(jobs_dir)?;
    let now = epoch_millis().to_string();
    let mut recovered = 0;
    for mut record in records.into_values() {
        if interrupt_record(&mut record, &now) {
            persist_job_record(jobs_dir, &record)?;
            recovered += 1;
        }
    }
    Ok(recovered)
}

fn interrupt_record(record: &mut LocalJobRecord, now: &str) -> bool {
    if !matches!(
        record.status,
        LocalJobStatus::Queued | LocalJobStatus::Running
    ) {
        return false;
    }
    let previous_status = local_job_status_label(&record.status);
    if !record.detail.is_object() {
        record.detail = serde_json::json!({"previousDetail": record.detail});
    }
    record.detail["interruption"] = serde_json::json!({
        "reason": "owner_restarted", "previousStatus": previous_status,
        "detectedAt": now, "outcome": "unknown", "automaticallyRetried": false,
        "previousProgress": record.progress,
    });
    record.detail["reason"] = serde_json::json!("owner_restarted");
    record.detail["error_code"] = serde_json::json!("JOB_INTERRUPTED");
    record.detail["message"] = serde_json::json!(MESSAGE);
    record.detail["result_ready"] = serde_json::json!(false);
    record.status = LocalJobStatus::Failed;
    record.error = Some(MESSAGE.to_string());
    record.updated_at = now.to_string();
    // This closes the job wrapper at diagnosis, not at an invented crash time.
    // Keep started_at and processing_time_ms: downtime is not processing time.
    record.completed_at = now.to_string();
    if let Some(progress) = record.progress.as_mut() {
        progress.phase = "interrupted".to_string();
        progress.message = MESSAGE.to_string();
        progress.updated_at = now.to_string();
        record.detail["progress"] = serde_json::to_value(progress).expect("serializable progress");
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_jobs::LocalJobRegistry;

    #[test]
    fn interruption_preserves_last_progress_and_does_not_claim_completion() {
        let mut record = crate::local_job_record_builder::queued_job_record(
            "job".into(),
            "test",
            None,
            serde_json::json!({"input":"keep"}),
            "10".into(),
        )
        .unwrap();
        record.status = LocalJobStatus::Running;
        record.started_at = "11".into();
        record.processing_time_ms = 4;
        record.progress = Some(crate::local_jobs::LocalJobProgress {
            phase: "writing".into(),
            message: "working".into(),
            current: 2,
            total: 10,
            percent: 20.0,
            updated_at: "12".into(),
            details: serde_json::json!({}),
        });
        assert!(interrupt_record(&mut record, "9999"));
        assert_eq!(record.processing_time_ms, 4);
        assert_eq!(record.detail["input"], "keep");
        assert_eq!(
            record.detail["interruption"]["previousProgress"]["phase"],
            "writing"
        );
        assert_eq!(record.progress.as_ref().unwrap().percent, 20.0);
        assert_eq!(record.progress.as_ref().unwrap().phase, "interrupted");
        assert!(!interrupt_record(&mut record, "10000"));
        assert_eq!(record.completed_at, "9999");
    }

    #[test]
    fn startup_closes_only_orphans_and_does_not_replay_or_delete_results() {
        let dir = std::env::temp_dir().join(format!("job-recovery-{}", uuid::Uuid::new_v4()));
        let jobs = LocalJobRegistry::new(dir.clone()).unwrap();
        let queued = jobs
            .insert_queued("test", Some("g".into()), serde_json::json!({}))
            .unwrap();
        let running = jobs
            .insert_queued("test", Some("g".into()), serde_json::json!({}))
            .unwrap();
        jobs.mark_running(&running.job_id).unwrap();
        let succeeded = jobs
            .insert_queued("test", None, serde_json::json!({}))
            .unwrap();
        jobs.finish_existing(
            &succeeded.job_id,
            Ok(serde_json::json!({"ok":true})),
            "application/json",
        )
        .unwrap();
        let failed = jobs
            .insert_queued("test", None, serde_json::json!({}))
            .unwrap();
        jobs.finish_existing(
            &failed.job_id,
            Err("known failure".into()),
            "application/json",
        )
        .unwrap();
        let cancelled = jobs
            .insert_queued("test", None, serde_json::json!({}))
            .unwrap();
        jobs.cancel(&cancelled.job_id).unwrap();
        let before = load_job_records(&dir).unwrap();

        // A normal second registry (including status polling) changes nothing.
        let poll = LocalJobRegistry::new(dir.clone()).unwrap();
        assert!(matches!(
            poll.get(&running.job_id).unwrap().unwrap().status,
            LocalJobStatus::Running
        ));
        let result_path = dir.join("preserved-result.json");
        std::fs::write(&result_path, "evidence").unwrap();
        drop(jobs);
        drop(poll);
        assert_eq!(recover_interrupted_jobs_on_startup(&dir).unwrap(), 2);
        assert_eq!(recover_interrupted_jobs_on_startup(&dir).unwrap(), 0);
        let after = load_job_records(&dir).unwrap();
        for id in [&queued.job_id, &running.job_id] {
            let record = &after[id];
            assert!(matches!(record.status, LocalJobStatus::Failed));
            assert_eq!(record.detail["interruption"]["outcome"], "unknown");
            assert_eq!(record.detail["result_ready"], false);
            assert_eq!(record.started_at, before[id].started_at);
            assert_eq!(record.processing_time_ms, before[id].processing_time_ms);
        }
        for id in [&succeeded.job_id, &failed.job_id, &cancelled.job_id] {
            assert_eq!(
                serde_json::to_value(&before[id]).unwrap(),
                serde_json::to_value(&after[id]).unwrap()
            );
        }
        assert_eq!(std::fs::read_to_string(result_path).unwrap(), "evidence");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
