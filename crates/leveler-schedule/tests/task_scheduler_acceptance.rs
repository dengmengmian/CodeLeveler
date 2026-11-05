//! Real Task Scheduler acceptance for the Windows wakeup backend.
//!
//! The test mutates the host's Task Scheduler, so it only runs when
//! `LEVELER_TASK_SCHEDULER_ACCEPTANCE=1`. It registers a per-user task with a
//! unique label and an isolated home, starts it on demand, and verifies the
//! side effect the task itself writes. Registration, query and removal are
//! always verified; execution is reported as NOT_VERIFIED when Task Scheduler
//! refuses to start an `InteractiveToken` task because no user has an
//! interactive session.
//!
//! Every check writes an evidence JSON document to
//! `LEVELER_TASK_SCHEDULER_EVIDENCE_DIR` (the temp directory by default).

#![cfg(windows)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use leveler_schedule::wakeup::{self, WAKE_INTERVAL, WakeupSpec, WakeupStatus};
use serde_json::{Value, json};

const ENABLE_ENV: &str = "LEVELER_TASK_SCHEDULER_ACCEPTANCE";
const EVIDENCE_ENV: &str = "LEVELER_TASK_SCHEDULER_EVIDENCE_DIR";
const RUN_TIMEOUT: Duration = Duration::from_secs(45);

/// `SCHED_S_TASK_RUNNING`: the task is running and produced no result yet.
const TASK_RUNNING: i32 = 267_009;
/// `SCHED_S_TASK_HAS_NOT_RUN`: the scheduler never started the task.
const TASK_HAS_NOT_RUN: i32 = 267_011;

const SCRIPT: &str = "@echo off\r\n\
setlocal\r\n\
>>\"%~dp0marker.txt\" echo started\r\n\
ping -n 10 127.0.0.1 >nul\r\n\
>>\"%~dp0marker.txt\" echo done\r\n\
exit /b 0\r\n";

#[tokio::test]
async fn the_real_task_scheduler_registers_runs_dedups_and_removes() {
    if std::env::var(ENABLE_ENV).as_deref() != Ok("1") {
        eprintln!("TASK_SCHEDULER_ACCEPTANCE: SKIP (set {ENABLE_ENV}=1 to run)");
        return;
    }

    let report = run_acceptance().await;
    let evidence = write_evidence(&report);
    println!(
        "{}",
        serde_json::to_string_pretty(&report).expect("evidence JSON")
    );
    println!("TASK_SCHEDULER_ACCEPTANCE_EVIDENCE: {}", evidence.display());

    let verdict = report["verdict"].as_str().unwrap_or("FAIL");
    println!("TASK_SCHEDULER_ACCEPTANCE: {verdict}");
    match verdict {
        "PASS" => {}
        "PARTIAL" | "NOT_VERIFIED" => {
            println!(
                "::warning title=Task Scheduler acceptance::{verdict}: see the evidence above"
            );
        }
        _ => panic!(
            "Task Scheduler acceptance failed; evidence: {}",
            evidence.display()
        ),
    }
}

async fn run_acceptance() -> Value {
    // The home contains spaces on purpose: the script path must survive both
    // Task Scheduler's XML and `cmd.exe`'s argument handling.
    let home = std::env::temp_dir().join(format!(
        "leveler ts acceptance {}",
        leveler_core::new_uuid_string()
    ));
    std::fs::create_dir_all(&home).expect("create the isolated home");
    let script = home.join("wakeup.cmd");
    std::fs::write(&script, SCRIPT).expect("write the task script");
    let marker = home.join("marker.txt");
    let label = wakeup::label_for(&home);
    let _cleanup = Cleanup {
        label: label.clone(),
        home: home.clone(),
    };

    let executable = std::env::var_os("ComSpec")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows\System32\cmd.exe"));
    let spec = WakeupSpec {
        home: home.clone(),
        executable: executable.clone(),
        arguments: vec!["/c".into(), script.to_string_lossy().into_owned()],
        environment: BTreeMap::new(),
        interval: WAKE_INTERVAL,
        log_dir: home.join("logs"),
    };

    let mut report = json!({
        "schema": "leveler.task-scheduler-acceptance/1",
        "started_at": chrono::Utc::now().to_rfc3339(),
        "label": label,
        "home": home.to_string_lossy(),
        "user": std::env::var("USERNAME").unwrap_or_default(),
        "command": executable.to_string_lossy(),
        "script": script.to_string_lossy(),
        "marker": marker.to_string_lossy(),
    });
    let mut failures: Vec<String> = Vec::new();
    let mut execution = "NOT_RUN";
    let mut execution_detail = String::new();
    let mut dedup = "NOT_RUN";
    let mut dedup_detail = String::new();

    // 1. Register with the real scheduler and read the record back.
    let enable = match wakeup::enable(&spec).await {
        Ok(status) => status,
        Err(error) => {
            failures.push(format!("enable failed: {error}"));
            report["enable"] = json!({ "error": error.to_string() });
            return finish(
                report,
                failures,
                execution,
                execution_detail,
                dedup,
                dedup_detail,
            );
        }
    };
    report["enable"] = status_json(&enable);
    if enable.backend != "task-scheduler" {
        failures.push(format!("unexpected backend `{}`", enable.backend));
    }
    if !enable.supported {
        failures.push("the backend reports itself unsupported on Windows".into());
    }
    if !enable.registered || !enable.loaded {
        failures.push("enable did not leave a registered task".into());
    }
    match &enable.registration {
        Some(registration) => {
            if !same_path(&registration.executable, &executable) {
                failures.push(format!(
                    "the scheduler stored `{}` instead of `{}`",
                    registration.executable.display(),
                    executable.display()
                ));
            }
            if registration.arguments != spec.arguments {
                failures.push(format!(
                    "the scheduler stored arguments {:?} instead of {:?}",
                    registration.arguments, spec.arguments
                ));
            }
            if registration.interval != Some(WAKE_INTERVAL) {
                failures.push(format!(
                    "the scheduler stored interval {:?} instead of {:?}",
                    registration.interval, WAKE_INTERVAL
                ));
            }
            if registration.working_directory.as_deref() != Some(home.as_path()) {
                failures.push(format!(
                    "the scheduler stored working directory {:?} instead of `{}`",
                    registration.working_directory,
                    home.display()
                ));
            }
        }
        None => failures.push("enable reported no registration record".into()),
    }

    // A repetition trigger can fire once immediately after registration. Let
    // such an instance finish and remove its evidence so the marker below can
    // only describe the run this test starts.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    if marker.exists() {
        let mut ignored = None;
        let settled = wait_until(
            &marker,
            Duration::from_secs(20),
            &home,
            &mut ignored,
            marker_completed,
        )
        .await;
        report["automatic_run_after_enable"] =
            json!({ "settled": settled, "marker": read_marker(&marker) });
    }
    let _ = std::fs::remove_file(&marker);

    // 2. Start the task on demand and require a side effect, not just exit 0.
    let started = schtasks(&["/Run", "/TN", &label]);
    let started_stderr = decode_output(&started.stderr);
    report["on_demand_run"] = json!({
        "exit_code": started.status.code(),
        "stdout": decode_output(&started.stdout),
        "stderr": started_stderr,
    });

    if !started.status.success() {
        if started_stderr
            .to_ascii_lowercase()
            .contains("not logged on")
        {
            execution = "NOT_VERIFIED";
            execution_detail = format!(
                "Task Scheduler refused the InteractiveToken task because no interactive session exists: {}",
                started_stderr.trim()
            );
        } else {
            execution = "FAILED";
            failures.push(format!(
                "schtasks /Run failed ({:?}): {}",
                started.status.code(),
                started_stderr.trim()
            ));
        }
    } else {
        let mut last_status = None;
        let done = wait_until(
            &marker,
            RUN_TIMEOUT,
            &home,
            &mut last_status,
            marker_completed,
        )
        .await;
        if let Ok(status) = wakeup::status(&home).await {
            last_status = Some(status);
        }
        report["marker_after_run"] = json!(read_marker(&marker));
        if let Some(status) = &last_status {
            report["status_after_run"] = status_json(status);
        }

        if done {
            execution = "VERIFIED";
            if let Some(code) = last_status
                .as_ref()
                .and_then(|status| status.last_exit_code)
                && code != 0
            {
                execution = "FAILED";
                failures.push(format!(
                    "the task wrote its marker but Task Scheduler reports last result {code}"
                ));
            }
        } else {
            match last_status
                .as_ref()
                .and_then(|status| status.last_exit_code)
            {
                Some(TASK_RUNNING) => {
                    execution = "FAILED";
                    failures.push(format!(
                        "the task was still running after {} seconds",
                        RUN_TIMEOUT.as_secs()
                    ));
                }
                Some(TASK_HAS_NOT_RUN) => {
                    execution = "NOT_VERIFIED";
                    execution_detail = format!(
                        "Task Scheduler reports the task has never run; the registration itself is valid. {}",
                        last_status
                            .as_ref()
                            .and_then(|status| status.detail.clone())
                            .unwrap_or_default()
                    );
                }
                Some(code) => {
                    execution = "FAILED";
                    failures.push(format!(
                        "the task did not write its marker; Task Scheduler reports last result {code}"
                    ));
                }
                None => {
                    execution = "NOT_VERIFIED";
                    execution_detail = format!(
                        "the task did not write its marker and Task Scheduler exposed no last result: {}",
                        last_status
                            .as_ref()
                            .and_then(|status| status.detail.clone())
                            .unwrap_or_default()
                    );
                }
            }
        }
    }

    // 3. `IgnoreNew` must keep a second on-demand start from running while the
    // first instance is alive. The marker counts instances, not invocations.
    if execution == "VERIFIED" {
        let _ = std::fs::remove_file(&marker);
        tokio::time::sleep(Duration::from_secs(1)).await;

        let first = schtasks(&["/Run", "/TN", &label]);
        let mut ignored = None;
        let first_started = first.status.success()
            && wait_until(
                &marker,
                Duration::from_secs(30),
                &home,
                &mut ignored,
                |path| marker_has(path, "started"),
            )
            .await;
        report["dedup_first_run"] = json!({
            "exit_code": first.status.code(),
            "stderr": decode_output(&first.stderr),
            "started": first_started,
        });

        if !first_started {
            dedup = "NOT_VERIFIED";
            dedup_detail = "the first dedup instance did not start".into();
        } else {
            tokio::time::sleep(Duration::from_millis(800)).await;
            let second = schtasks(&["/Run", "/TN", &label]);
            report["dedup_second_run"] = json!({
                "exit_code": second.status.code(),
                "stderr": decode_output(&second.stderr),
            });

            let mut ignored = None;
            let finished =
                wait_until(&marker, RUN_TIMEOUT, &home, &mut ignored, marker_completed).await;
            // Leave time for a wrongly admitted second instance to write its
            // own `started` line before counting.
            tokio::time::sleep(Duration::from_secs(3)).await;
            let text = read_marker(&marker);
            let starts = text.lines().filter(|line| line.trim() == "started").count();
            report["dedup_marker"] = json!(text);
            report["dedup_started_instances"] = json!(starts);

            if !second.status.success() || !finished || starts != 1 {
                dedup = "FAILED";
                failures.push(format!(
                    "IgnoreNew admitted {starts} instance(s) (expected 1); second /Run exit {:?}, finished={finished}",
                    second.status.code()
                ));
            } else {
                dedup = "VERIFIED";
            }
        }
    }

    // 4. Remove the registration and confirm the scheduler agrees.
    match wakeup::disable(&home).await {
        Ok(status) => {
            report["disable"] = status_json(&status);
            if status.registered || status.loaded {
                failures.push("disable left the task registered".into());
            }
        }
        Err(error) => failures.push(format!("disable failed: {error}")),
    }
    let query_after = schtasks(&["/Query", "/TN", &label]);
    report["query_after_disable"] = json!({
        "exit_code": query_after.status.code(),
        "stderr": decode_output(&query_after.stderr),
    });
    if query_after.status.success() {
        failures.push("the task still exists after disable".into());
    }

    finish(
        report,
        failures,
        execution,
        execution_detail,
        dedup,
        dedup_detail,
    )
}

fn finish(
    mut report: Value,
    failures: Vec<String>,
    execution: &str,
    execution_detail: String,
    dedup: &str,
    dedup_detail: String,
) -> Value {
    let verdict = if !failures.is_empty() || execution == "FAILED" || dedup == "FAILED" {
        "FAIL"
    } else if execution == "VERIFIED" && dedup == "VERIFIED" {
        "PASS"
    } else if execution == "VERIFIED" {
        "PARTIAL"
    } else {
        "NOT_VERIFIED"
    };
    report["execution"] = json!({ "verdict": execution, "detail": execution_detail });
    report["dedup"] = json!({ "verdict": dedup, "detail": dedup_detail });
    report["failures"] = json!(failures);
    report["verdict"] = json!(verdict);
    report["finished_at"] = json!(chrono::Utc::now().to_rfc3339());
    report
}

fn status_json(status: &WakeupStatus) -> Value {
    json!({
        "backend": status.backend,
        "supported": status.supported,
        "label": status.label,
        "registered": status.registered,
        "loaded": status.loaded,
        "registration": status.registration.as_ref().map(|registration| json!({
            "executable": registration.executable.to_string_lossy(),
            "arguments": registration.arguments,
            "interval_secs": registration.interval.map(|interval| interval.as_secs()),
            "working_directory": registration
                .working_directory
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
        })),
        "last_exit_code": status.last_exit_code,
        "run_count": status.run_count,
        "detail": status.detail,
    })
}

async fn wait_until(
    path: &Path,
    timeout: Duration,
    home: &Path,
    last_status: &mut Option<WakeupStatus>,
    matched: fn(&Path) -> bool,
) -> bool {
    let deadline = Instant::now() + timeout;
    let mut tick = 0_u32;
    loop {
        if matched(path) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        if tick.is_multiple_of(4)
            && let Ok(status) = wakeup::status(home).await
        {
            *last_status = Some(status);
        }
        tick += 1;
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn marker_has(path: &Path, needle: &str) -> bool {
    std::fs::read_to_string(path)
        .map(|text| text.lines().any(|line| line.trim() == needle))
        .unwrap_or(false)
}

/// A complete instance: a `started` line followed by a later `done` line. A
/// leftover `done` from an unrelated run does not count.
fn marker_completed(path: &Path) -> bool {
    let mut seen_started = false;
    for line in read_marker(path).lines() {
        match line.trim() {
            "started" => seen_started = true,
            "done" if seen_started => return true,
            _ => {}
        }
    }
    false
}

fn read_marker(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

fn schtasks(args: &[&str]) -> Output {
    Command::new("schtasks.exe")
        .args(args)
        .output()
        .expect("schtasks.exe must be executable")
}

fn same_path(left: &Path, right: &Path) -> bool {
    left.to_string_lossy()
        .eq_ignore_ascii_case(&right.to_string_lossy())
}

/// `schtasks /XML` emits UTF-16LE; list output usually uses the console code
/// page. Evidence must stay readable either way.
fn decode_output(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return decode_utf16le(rest);
    }
    if bytes.len() >= 4 && bytes.len().is_multiple_of(2) {
        let high_nuls = bytes
            .iter()
            .skip(1)
            .step_by(2)
            .filter(|byte| **byte == 0)
            .count();
        if high_nuls * 4 >= bytes.len() {
            return decode_utf16le(bytes);
        }
    }
    String::from_utf8_lossy(bytes).into_owned()
}

fn decode_utf16le(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    String::from_utf16_lossy(&units)
}

fn write_evidence(report: &Value) -> PathBuf {
    let directory = std::env::var_os(EVIDENCE_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("leveler-task-scheduler-evidence"));
    std::fs::create_dir_all(&directory).expect("create the evidence directory");
    let name = format!(
        "task-scheduler-{}.json",
        report["label"].as_str().unwrap_or("unknown")
    );
    let path = directory.join(name);
    std::fs::write(
        &path,
        serde_json::to_string_pretty(report).expect("serialize evidence"),
    )
    .expect("write evidence");
    path
}

/// Run on every exit path, including a panic: an acceptance test must not
/// leave a registered task or an isolated home behind.
struct Cleanup {
    label: String,
    home: PathBuf,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = Command::new("schtasks.exe")
            .args(["/Delete", "/F", "/TN", &self.label])
            .output();
        let _ = std::fs::remove_dir_all(&self.home);
    }
}
