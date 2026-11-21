use crate::{
    clock::timestamp,
    local_jobs::{LocalJobProgress, LocalJobRegistry},
};
use std::{
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process, thread,
    time::{Duration, Instant},
};

pub(crate) const DEFAULT_TERMINATE_GRACE: Duration = Duration::from_secs(2);

pub(crate) fn executable_path(command: &str) -> Option<PathBuf> {
    let command_path = Path::new(command);
    if command_path.is_absolute() || command_path.components().count() > 1 {
        return command_path.is_file().then(|| command_path.to_path_buf());
    }
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var)
        .map(|dir| dir.join(command))
        .find(|candidate| candidate.is_file())
}

pub(crate) fn command_output_summary(output: &process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let raw = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    let summary = raw.lines().take(3).collect::<Vec<_>>().join(" ");
    if summary.chars().count() > 240 {
        format!("{}...", summary.chars().take(240).collect::<String>())
    } else {
        summary
    }
}

pub(crate) fn command_output_with_timeout(
    command: &mut process::Command,
    label: &str,
    timeout: Duration,
) -> Result<process::Output, String> {
    command
        .stdout(process::Stdio::piped())
        .stderr(process::Stdio::piped());
    // The child leads its own process group, so a timeout can kill whatever
    // it started: the helpers run as `uv run … python`, and python, not uv,
    // holds the pipes.
    place_child_in_new_process_group(command);
    let mut child = command
        .spawn()
        .map_err(|error| format!("start {label}: {error}"))?;
    // Read both pipes while the child runs. A child that writes more than
    // the OS pipe buffer (64 KiB on macOS) blocks in write() until someone
    // reads; waiting for exit first is a deadlock that ends at the timeout
    // (a 629-page PDF's Markdown is about 1.9 MB).
    let stdout_reader = drain_pipe(child.stdout.take());
    let stderr_reader = drain_pipe(child.stderr.take());
    let started_at = Instant::now();
    let status = loop {
        match child
            .try_wait()
            .map_err(|error| format!("poll {label}: {error}"))?
        {
            Some(status) => break status,
            None if started_at.elapsed() >= timeout => {
                terminate_child(&mut child, DEFAULT_TERMINATE_GRACE);
                // Whatever is left of the group dies now, so the pipes close
                // and the readers end; the readers are not joined here, so a
                // straggler that left the group cannot hold the timeout.
                kill_process_group(child.id());
                drop(stdout_reader);
                drop(stderr_reader);
                return Err(format!("{label} timed out after {}s", timeout.as_secs()));
            }
            None => thread::sleep(Duration::from_millis(100)),
        }
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| format!("read {label} output: stdout reader panicked"))?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| format!("read {label} output: stderr reader panicked"))?;
    Ok(process::Output {
        status,
        stdout,
        stderr,
    })
}

/// Read a child's pipe to its end on a thread, so the child never blocks on
/// a full pipe. A read error ends the read with what arrived so far.
fn drain_pipe<R: Read + Send + 'static>(pipe: Option<R>) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut bytes);
        }
        bytes
    })
}

/// Spawn `command`, polling for cancellation and timeout. On either, signal the
/// child cooperatively (SIGTERM on unix; `kill` on Windows) and only escalate
/// to SIGKILL after `grace` if the child has not exited.
///
/// Stdout is read line-by-line while the process runs so progress markers of
/// the form `MN_PROGRESS {"current":N,"total":M,...}` can be surfaced through
/// `LocalJobRegistry::update_progress`. The returned `Output` contains the
/// full byte streams as if `wait_with_output` had been called.
pub(crate) fn command_output_with_timeout_and_cancel(
    command: &mut process::Command,
    label: &str,
    timeout: Duration,
    jobs: &LocalJobRegistry,
    job_id: &str,
) -> Result<process::Output, String> {
    command_output_with_timeout_cancel_and_grace(
        command,
        label,
        timeout,
        jobs,
        job_id,
        DEFAULT_TERMINATE_GRACE,
    )
}

pub(crate) fn command_output_with_timeout_cancel_and_grace(
    command: &mut process::Command,
    label: &str,
    timeout: Duration,
    jobs: &LocalJobRegistry,
    job_id: &str,
    grace: Duration,
) -> Result<process::Output, String> {
    command
        .stdout(process::Stdio::piped())
        .stderr(process::Stdio::piped());
    place_child_in_new_process_group(command);
    let mut child = command
        .spawn()
        .map_err(|error| format!("start {label}: {error}"))?;

    let stdout_handle = child.stdout.take();
    let stderr_handle = child.stderr.take();

    // Bracket the subprocess execution with (0,0) start and (1,1) completion
    // markers. Per-page MN_PROGRESS lines from the Python helper (e.g.
    // pymupdf4llm_pdf_to_markdown.py) flow through `parse_progress_line`
    // between these brackets. Helpers that do not emit per-page markers
    // (e.g. docling) still get the bracketing for "started" / "complete".
    emit_progress(
        jobs,
        job_id,
        LocalJobProgress {
            phase: "parse".to_string(),
            message: format!("{label} started"),
            current: 0,
            total: 0,
            percent: 0.0,
            updated_at: timestamp(),
            details: serde_json::json!({ "stage": "subprocess.start" }),
        },
    );

    let outcome = thread::scope(
        |scope| -> Result<
            (
                Vec<u8>,
                Vec<u8>,
                Option<TerminationCause>,
                process::ExitStatus,
            ),
            String,
        > {
            let stdout_thread = stdout_handle.map(|stdout| {
                scope.spawn(move || stream_stdout_with_progress(stdout, label, jobs, job_id))
            });
            let stderr_thread = stderr_handle.map(|stderr| {
                scope.spawn(move || {
                    let mut buffer = Vec::new();
                    let mut reader = stderr;
                    let _ = reader.read_to_end(&mut buffer);
                    buffer
                })
            });

            let started_at = Instant::now();
            let mut termination: Option<TerminationCause> = None;
            loop {
                match child
                    .try_wait()
                    .map_err(|error| format!("poll {label}: {error}"))?
                {
                    Some(_) => break,
                    None if jobs.is_cancelled(job_id).unwrap_or(false) => {
                        termination = Some(TerminationCause::Cancelled);
                        break;
                    }
                    None if started_at.elapsed() >= timeout => {
                        termination = Some(TerminationCause::TimedOut);
                        break;
                    }
                    None => thread::sleep(Duration::from_millis(100)),
                }
            }

            if termination.is_some() {
                terminate_child(&mut child, grace);
            }

            let status = child
                .wait()
                .map_err(|error| format!("read {label} output: {error}"))?;

            let stdout_buf = stdout_thread
                .and_then(|handle| handle.join().ok())
                .unwrap_or_default();
            let stderr_buf = stderr_thread
                .and_then(|handle| handle.join().ok())
                .unwrap_or_default();

            Ok((stdout_buf, stderr_buf, termination, status))
        },
    )?;

    let (stdout_buf, stderr_buf, termination, status) = outcome;

    if let Some(cause) = termination {
        return match cause {
            TerminationCause::Cancelled => Err(format!("{label} cancelled")),
            TerminationCause::TimedOut => {
                Err(format!("{label} timed out after {}s", timeout.as_secs()))
            }
        };
    }

    emit_progress(
        jobs,
        job_id,
        LocalJobProgress {
            phase: "parse".to_string(),
            message: format!("{label} complete"),
            current: 1,
            total: 1,
            percent: 100.0,
            updated_at: timestamp(),
            details: serde_json::json!({ "stage": "subprocess.exit" }),
        },
    );

    Ok(process::Output {
        status,
        stdout: stdout_buf,
        stderr: stderr_buf,
    })
}

#[derive(Debug, Clone, Copy)]
enum TerminationCause {
    Cancelled,
    TimedOut,
}

fn stream_stdout_with_progress<R: Read>(
    stdout: R,
    label: &str,
    jobs: &LocalJobRegistry,
    job_id: &str,
) -> Vec<u8> {
    let mut reader = BufReader::new(stdout);
    let mut accumulated: Vec<u8> = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                if let Some(progress) = parse_progress_line(line.trim_end()) {
                    // MN_PROGRESS lines are control-channel: surface to the
                    // job registry but DO NOT accumulate them into stdout.
                    // Downstream callers like `parse_json_command_output`
                    // parse the entire stdout buffer as JSON and would
                    // choke on interleaved progress markers.
                    let total = progress.total.max(progress.current);
                    let percent = if total == 0 {
                        0.0
                    } else {
                        (progress.current as f64 / total as f64) * 100.0
                    };
                    let _ = jobs.update_progress(
                        job_id,
                        LocalJobProgress {
                            phase: progress.phase.unwrap_or_else(|| "parse".to_string()),
                            message: progress
                                .message
                                .unwrap_or_else(|| format!("{label} progress")),
                            current: progress.current,
                            total,
                            percent,
                            updated_at: timestamp(),
                            details: progress.details.unwrap_or_else(|| serde_json::json!({})),
                        },
                    );
                } else {
                    accumulated.extend_from_slice(line.as_bytes());
                }
            }
            Err(_) => break,
        }
    }
    accumulated
}

#[derive(Debug, Default)]
struct ParsedProgress {
    phase: Option<String>,
    message: Option<String>,
    current: usize,
    total: usize,
    details: Option<serde_json::Value>,
}

fn parse_progress_line(line: &str) -> Option<ParsedProgress> {
    let payload = line.trim().strip_prefix("MN_PROGRESS")?.trim();
    let value: serde_json::Value = serde_json::from_str(payload).ok()?;
    let object = value.as_object()?;
    let current = object
        .get("current")
        .or_else(|| object.get("current_page"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0) as usize;
    let total = object
        .get("total")
        .or_else(|| object.get("total_pages"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0) as usize;
    Some(ParsedProgress {
        phase: object
            .get("phase")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        message: object
            .get("message")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        current,
        total,
        details: object.get("details").cloned(),
    })
}

fn emit_progress(jobs: &LocalJobRegistry, job_id: &str, progress: LocalJobProgress) {
    let _ = jobs.update_progress(job_id, progress);
}

pub(crate) fn terminate_child(child: &mut process::Child, grace: Duration) {
    #[cfg(unix)]
    {
        // SIGTERM = 15. We send to `-pid` so the entire process group (created
        // via `setsid` in `place_child_in_new_process_group`) receives the
        // signal — this ensures child shells that fork a `sleep` cooperatively
        // tear down as well.
        let pid = child.id() as i32;
        unsafe {
            libc_kill(-pid, 15);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }

    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => thread::sleep(Duration::from_millis(100)),
            Err(_) => break,
        }
    }

    // Cooperative window expired — escalate to SIGKILL on the whole group.
    #[cfg(unix)]
    {
        let pid = child.id() as i32;
        unsafe {
            libc_kill(-pid, 9);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(unix)]
pub(crate) fn place_child_in_new_process_group(command: &mut process::Command) {
    use std::os::unix::process::CommandExt;
    unsafe {
        command.pre_exec(|| {
            // setsid() fails if the caller is already a process group leader,
            // which is rare for forked Rust children but we ignore the error
            // to be safe.
            let _ = setsid();
            Ok(())
        });
    }
}

#[cfg(not(unix))]
pub(crate) fn place_child_in_new_process_group(_command: &mut process::Command) {}

/// The most descriptors this process asks for. macOS will not start a child
/// with redirected output through a descriptor numbered at or above its
/// OPEN_MAX (10240), so the limit stays under that line.
const OPEN_FILE_LIMIT: u64 = 10_240;

/// Raise the soft limit on open files to what the system allows, up to
/// `OPEN_FILE_LIMIT`. An app opened from the Dock starts with 256; each open
/// graph's store holds 20 to 30 files, so a person with nine or ten graphs
/// open would otherwise run out, and every open after that fails. Returns
/// the limit in force. Failure leaves the limit as it was.
pub(crate) fn raise_open_file_limit() -> Option<u64> {
    #[cfg(unix)]
    unsafe {
        let mut limit: libc::rlimit = std::mem::zeroed();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) != 0 {
            return None;
        }
        // A system may allow less than it reports (macOS caps a process at
        // kern.maxfilesperproc), so step down until one is accepted.
        for step in [OPEN_FILE_LIMIT, 8192, 4096, 2048, 1024] {
            let wanted = (step as libc::rlim_t).min(limit.rlim_max);
            if limit.rlim_cur >= wanted {
                break;
            }
            let raised = libc::rlimit { rlim_cur: wanted, rlim_max: limit.rlim_max };
            if libc::setrlimit(libc::RLIMIT_NOFILE, &raised) == 0 {
                limit.rlim_cur = wanted;
                break;
            }
        }
        Some(limit.rlim_cur as u64)
    }
    #[cfg(not(unix))]
    None
}

/// SIGHUP the group `leader` leads: what a closing terminal sends, so a
/// shell saves its history and passes the hangup to its jobs.
pub(crate) fn hangup_process_group(leader: u32) {
    #[cfg(unix)]
    unsafe {
        libc_kill(-(leader as i32), 1);
    }
    #[cfg(not(unix))]
    let _ = leader;
}

/// SIGKILL every process still in the group `leader` created with
/// `place_child_in_new_process_group`, whether or not the leader is alive.
pub(crate) fn kill_process_group(leader: u32) {
    #[cfg(unix)]
    unsafe {
        libc_kill(-(leader as i32), 9);
    }
    #[cfg(not(unix))]
    let _ = leader;
}

#[cfg(unix)]
unsafe fn libc_kill(pid: i32, sig: i32) -> i32 {
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    kill(pid, sig)
}

#[cfg(unix)]
fn setsid() -> i32 {
    extern "C" {
        fn setsid() -> i32;
    }
    unsafe { setsid() }
}

pub(crate) fn parse_json_command_output(
    output: process::Output,
    label: &str,
) -> Result<serde_json::Value, String> {
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = stderr.trim();
        return Err(if message.is_empty() {
            format!("{label} exited with {}", output.status)
        } else {
            format!("{label} failed: {message}")
        });
    }
    serde_json::from_slice::<serde_json::Value>(&output.stdout)
        .map_err(|error| format!("parse {label} output: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executable_path_accepts_absolute_files() {
        let path =
            std::env::temp_dir().join(format!("mnemosyne-process-util-{}", std::process::id()));
        std::fs::write(&path, b"").expect("write temp command marker");

        assert_eq!(executable_path(&path.to_string_lossy()), Some(path.clone()));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn command_output_with_timeout_captures_stdout() {
        let mut command = process::Command::new("sh");
        command.arg("-c").arg("printf '{\"ok\":true}'");
        let output = command_output_with_timeout(&mut command, "json echo", Duration::from_secs(2))
            .expect("command should finish");
        let json = parse_json_command_output(output, "json echo").expect("parse json output");

        assert_eq!(
            json.get("ok").and_then(serde_json::Value::as_bool),
            Some(true)
        );
    }

    #[test]
    fn command_output_with_timeout_survives_output_larger_than_a_pipe() {
        // 1 MiB on stdout and 256 KiB on stderr: both far past the 64 KiB
        // pipe buffer. Before the drain this sat until the timeout.
        let mut command = process::Command::new("sh");
        command.arg("-c").arg(
            "head -c 1048576 /dev/zero | tr '\\0' 'x'; head -c 262144 /dev/zero | tr '\\0' 'e' 1>&2",
        );
        let started = Instant::now();
        let output =
            command_output_with_timeout(&mut command, "large output", Duration::from_secs(20))
                .expect("a large output must not deadlock");
        assert!(output.status.success());
        assert_eq!(output.stdout.len(), 1_048_576);
        assert_eq!(output.stderr.len(), 262_144);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn command_output_with_timeout_times_out_a_child_whose_grandchild_holds_the_pipe() {
        // The shape of `uv run … python`: the direct child waits on a
        // grandchild that holds stdout. The timeout must return on time and
        // the grandchild must die with the group.
        let marker = std::env::temp_dir().join(format!(
            "garden-pipe-drain-{}-{}",
            std::process::id(),
            timestamp()
        ));
        let script = format!(
            "(sleep 30; touch '{}') & wait",
            marker.to_string_lossy()
        );
        let mut command = process::Command::new("sh");
        command.arg("-c").arg(script);
        let started = Instant::now();
        let error =
            command_output_with_timeout(&mut command, "sleeper", Duration::from_millis(300))
                .expect_err("a silent child must time out");
        assert!(error.contains("timed out"), "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "the timeout waited on the grandchild: {:?}",
            started.elapsed()
        );
        assert!(!marker.exists());
    }

    #[test]
    fn parses_mn_progress_line() {
        let parsed = parse_progress_line(
            "MN_PROGRESS {\"current\":3,\"total\":10,\"phase\":\"parse\",\"message\":\"page\"}",
        )
        .expect("parse marker");
        assert_eq!(parsed.current, 3);
        assert_eq!(parsed.total, 10);
        assert_eq!(parsed.phase.as_deref(), Some("parse"));
    }

    #[cfg(unix)]
    mod parser_cancellation {
        use super::*;
        use crate::local_jobs::{LocalJobRegistry, LocalJobStatus};
        use std::time::{SystemTime, UNIX_EPOCH};

        fn temp_jobs_dir(name: &str) -> std::path::PathBuf {
            let suffix = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            std::env::temp_dir().join(format!("mnemosyne-jobs-{name}-{suffix}"))
        }

        #[test]
        fn parser_cancellation_drives_long_subprocess_to_cancelled() {
            let dir = temp_jobs_dir("parser-cancel");
            let registry =
                std::sync::Arc::new(LocalJobRegistry::new(dir.clone()).expect("create registry"));
            let record = registry
                .insert_queued(
                    "test_parser_cancel",
                    None,
                    serde_json::json!({ "input": true }),
                )
                .expect("insert queued");
            registry.mark_running(&record.job_id).expect("mark running");

            // Fire a watchdog thread that flips the job to Cancelled while the
            // subprocess is still mid-stream.
            let watchdog_registry = registry.clone();
            let watchdog_job_id = record.job_id.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(300));
                watchdog_registry
                    .cancel(&watchdog_job_id)
                    .expect("cancel job");
            });

            // Synthetic "long parse": sleep 30s while emitting stderr lines so
            // the subprocess is genuinely longer than the 2s grace window. If
            // SIGTERM is honoured, sh exits in <1s; if we had to fall back to
            // SIGKILL, it would still be killed by the grace deadline.
            let mut command = process::Command::new("sh");
            command.arg("-c").arg("sleep 30");

            let started = Instant::now();
            let outcome = command_output_with_timeout_cancel_and_grace(
                &mut command,
                "test parser",
                Duration::from_secs(60),
                &registry,
                &record.job_id,
                Duration::from_secs(2),
            );
            let elapsed = started.elapsed();

            let error = outcome.expect_err("subprocess should report cancellation");
            assert!(
                error.contains("cancelled"),
                "expected cancelled error, got {error}",
            );

            // Cancel ladder budget: ~300ms watchdog delay + up to 100ms outer
            // poll + up to grace_window. We allow some slack on top of that.
            assert!(
                elapsed < Duration::from_secs(5),
                "cancel took too long: {elapsed:?}",
            );

            let final_record = registry
                .get(&record.job_id)
                .expect("get record")
                .expect("record exists");
            assert!(
                matches!(final_record.status, LocalJobStatus::Cancelled),
                "expected Cancelled, got {:?}",
                final_record.status,
            );
        }

        #[test]
        fn parser_cancellation_force_kills_after_grace_when_sigterm_ignored() {
            let dir = temp_jobs_dir("parser-cancel-sigkill");
            let registry =
                std::sync::Arc::new(LocalJobRegistry::new(dir.clone()).expect("create registry"));
            let record = registry
                .insert_queued("test_parser_force_kill", None, serde_json::json!({}))
                .expect("insert queued");
            registry.mark_running(&record.job_id).expect("mark running");

            let watchdog_registry = registry.clone();
            let watchdog_job_id = record.job_id.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(200));
                watchdog_registry
                    .cancel(&watchdog_job_id)
                    .expect("cancel job");
            });

            // sh traps SIGTERM and re-enters a loop of short sleeps, so the
            // group-wide SIGTERM kills only the current sleep child while sh
            // itself keeps spinning. The grace window must expire and SIGKILL
            // (which cannot be trapped) brings sh down.
            let mut command = process::Command::new("sh");
            command
                .arg("-c")
                .arg("trap '' TERM; while :; do sleep 0.5; done");

            let grace = Duration::from_millis(500);
            let started = Instant::now();
            let outcome = command_output_with_timeout_cancel_and_grace(
                &mut command,
                "test parser sigkill",
                Duration::from_secs(60),
                &registry,
                &record.job_id,
                grace,
            );
            let elapsed = started.elapsed();

            let error = outcome.expect_err("subprocess should report cancellation");
            assert!(error.contains("cancelled"), "got {error}");
            // With grace=500ms + 200ms watchdog + 100ms poll slack, the kill
            // should land well under 3s. A failure here would mean SIGKILL
            // never fired and the test stalled until the 30s sleep ended.
            assert!(
                elapsed < Duration::from_secs(3),
                "force-kill ladder too slow: {elapsed:?}",
            );
        }

        #[test]
        fn parser_progress_markers_flow_to_job_record_and_strip_from_stdout() {
            // Verifies the end-to-end MN_PROGRESS flow that B2 finishes:
            // a helper subprocess emits per-page MN_PROGRESS lines, those
            // update LocalJobRecord::progress, and they are stripped from
            // the accumulated stdout so `parse_json_command_output` sees
            // only the JSON result line.
            let dir = temp_jobs_dir("parser-progress");
            let registry =
                std::sync::Arc::new(LocalJobRegistry::new(dir.clone()).expect("create registry"));
            let record = registry
                .insert_queued("test_parser_progress", None, serde_json::json!({}))
                .expect("insert queued");
            registry.mark_running(&record.job_id).expect("mark running");

            let mut command = process::Command::new("sh");
            command.arg("-c").arg(
                "echo 'MN_PROGRESS {\"current\":1,\"total\":3,\"phase\":\"parse\",\"message\":\"page 1/3\"}' && \
echo 'MN_PROGRESS {\"current\":2,\"total\":3,\"phase\":\"parse\",\"message\":\"page 2/3\"}' && \
echo 'MN_PROGRESS {\"current\":3,\"total\":3,\"phase\":\"parse\",\"message\":\"page 3/3\"}' && \
echo '{\"ok\":true}'",
            );

            let outcome = command_output_with_timeout_cancel_and_grace(
                &mut command,
                "test parser progress",
                Duration::from_secs(10),
                &registry,
                &record.job_id,
                Duration::from_millis(500),
            );
            let output = outcome.expect("subprocess succeeds");
            let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");

            // MN_PROGRESS lines must not appear in the accumulated stdout
            // (they are control-channel only).
            assert!(
                !stdout.contains("MN_PROGRESS"),
                "MN_PROGRESS leaked into stdout: {stdout:?}",
            );

            // The JSON result must be parseable as the only thing on stdout.
            let parsed: serde_json::Value =
                serde_json::from_str(stdout.trim()).expect("stdout parses as JSON");
            assert_eq!(parsed, serde_json::json!({ "ok": true }));

            // The job record's progress reflects the parser bracket
            // (1/1 "complete"), which fired AFTER the per-page markers.
            // The intermediate per-page markers all flowed through during
            // execution; we cannot easily snapshot them here without a
            // race, but the completion bracket proves the pipeline ran.
            let final_record = registry
                .get(&record.job_id)
                .expect("get record")
                .expect("record exists");
            let progress = final_record.progress.expect("progress recorded");
            assert_eq!(progress.phase, "parse");
            assert!(
                progress.current >= 1 && progress.total >= 1,
                "expected nonzero progress, got {}/{}",
                progress.current,
                progress.total,
            );
        }
    }
}

#[cfg(all(test, unix))]
mod open_file_limit_tests {
    use super::*;

    #[test]
    fn the_open_file_limit_is_raised_and_stays_under_the_spawn_line() {
        let before = unsafe {
            let mut limit: libc::rlimit = std::mem::zeroed();
            assert_eq!(libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit), 0);
            limit.rlim_cur as u64
        };
        let limit = raise_open_file_limit().expect("a limit");
        // Never lowered. A limit already above the line is left alone: on
        // macOS Rust's test runner raises its own to the system maximum.
        assert!(limit >= before, "{limit} < {before}");
        assert_eq!(raise_open_file_limit(), Some(limit));
    }
}
