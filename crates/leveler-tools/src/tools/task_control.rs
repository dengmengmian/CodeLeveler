//! get_task / wait_task / kill_task — manage background process tasks.
//!
//! Each tool is CONSTRUCTED with the registry it manages. It is not optional:
//! a background task nobody can observe or stop is an orphan, so the lifecycle
//! tools and the primitive that starts them share one registry by
//! construction rather than by hoping a context field was populated.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_execution::{BackgroundTaskRegistry, BackgroundTaskStatus, RiskLevel};

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

#[derive(Debug, Deserialize, JsonSchema)]
struct TaskIdInput {
    /// Task id returned by `run_command` with background=true.
    task_id: String,
}

/// Default wait bound. `wait_task` waits for the task to reach a terminal
/// status, and this is the ceiling on how long one call blocks. It is long on
/// purpose: a bounded probe that returned "still running" every few seconds
/// made one model round per interval, so a single long command cost dozens of
/// rounds. A caller that wants a short probe passes `timeout_seconds`.
const WAIT_DEFAULT_SECS: u64 = 600;
/// Hard cap. A caller may ask for more; the tool still returns the running
/// status rather than owning the AgentLoop for longer than this.
const WAIT_MAX_SECS: u64 = 600;

/// How long one `until: exit` wait blocks on a task that produces no new
/// information — no output and no terminal status.
///
/// A command that keeps running and keeps printing is still worth one long
/// wait: that is what this mode is for. A command that keeps running and
/// prints nothing has nothing more to tell the waiter, and spending the whole
/// bound (up to ten minutes) discovering that again is how one real run burned
/// nineteen minutes on a service that was ready after 47 seconds. Expiry here
/// stops nothing and claims nothing: the process is untouched, and the report
/// says exactly what was and was not observed, so a caller that is genuinely
/// waiting on silent work can wait again.
const WAIT_IDLE_SECS: u64 = 180;

/// How long a wait lets a failed launcher's process group finish before it
/// reports the failure.
///
/// The direct child's non-zero exit is already the command's result, but the
/// mutation report and the terminal state are produced when the group it left
/// behind is gone. Reporting the failure the instant the child exits loses that
/// account for the files it really changed; the grace is short, and a group that
/// outlives it is still reported as running.
const LAUNCHER_FAILURE_GRACE: Duration = Duration::from_millis(500);

#[derive(Debug, Deserialize, JsonSchema)]
struct WaitInput {
    task_id: String,
    /// Optional log cursor for an independent reader. Pass next_cursor from
    /// the prior result; omit to use the task's default incremental cursor.
    #[serde(default)]
    cursor: Option<u64>,
    /// Seconds to wait for the task to finish before returning its current
    /// status (default 600, max 600). Expiry does not cancel the background
    /// task, and a caller wanting a short probe passes a small value here.
    #[serde(default)]
    timeout_seconds: Option<u64>,
    /// What to wait FOR. `exit` (default) waits for a terminal status and is
    /// the only mode that reports a finite command's outcome. `match` returns
    /// as soon as new output matches `pattern` — the readiness signal a server
    /// prints while it keeps running. `output` returns on any new output.
    /// `quiet` returns when new output has stopped for `quiet_seconds`.
    /// The last three never end the task and never claim it succeeded.
    #[serde(default)]
    until: Option<WaitUntil>,
    /// Regex matched against the task's output for `until: match`. Anchor it on
    /// what the program actually prints (for example `listening on`).
    #[serde(default)]
    pattern: Option<String>,
    /// Quiet window for `until: quiet`, in seconds (default 10, max 600).
    #[serde(default)]
    quiet_seconds: Option<u64>,
}

/// What a `wait_task` call is waiting for. `Exit` owns the finite-command
/// contract; the other modes are for processes that keep running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum WaitUntil {
    Exit,
    Output,
    Match,
    Quiet,
}

/// Default quiet window for `until: quiet`, in seconds.
const QUIET_DEFAULT_SECS: u64 = 10;
const QUIET_MAX_SECS: u64 = 600;
/// Longest pattern a caller may hand to `until: match`. A readiness pattern is
/// a signal, not a log query.
const PATTERN_MAX_LEN: usize = 512;
/// Output kept for pattern matching. Bounded so a chatty service cannot make
/// one wait call hold an unbounded window in memory.
const MATCH_WINDOW_BYTES: usize = 64 * 1024;
/// Longest single observation slice inside a readiness wait. Small enough to
/// notice a quiet window and the caller's deadline promptly, large enough not
/// to spin.
const OBSERVE_SLICE: Duration = Duration::from_millis(250);

/// The bounded interval `wait_task` waits for, shared with a sub-agent wait so
/// both kinds of task id wait the same way.
pub fn wait_interval(timeout_seconds: Option<u64>) -> Duration {
    Duration::from_secs(
        timeout_seconds
            .unwrap_or(WAIT_DEFAULT_SECS)
            .clamp(1, WAIT_MAX_SECS),
    )
}

/// Result of an observation wait that is not waiting for a terminal status.
enum WaitOutcome {
    /// A criterion the caller asked for was met while the task is still
    /// running. The process was not stopped and was not marked finished.
    Reached(ToolOutput),
    /// The task reached a terminal status (or its command already failed).
    /// The shared post-wait path reports it exactly as `until: exit` would.
    Terminal(Box<leveler_execution::BackgroundTaskObservation>),
}

/// How an `until: exit` wait ended without a terminal status.
enum ExitWait {
    /// A terminal status (or the command's own failure) was reached, or the
    /// user typed and the round is being handed back. `true` is the yield.
    Settled(leveler_execution::BackgroundTaskObservation, bool),
    /// The task is still running and this call saw nothing new for the idle
    /// ceiling, or its own bound expired. No process was stopped.
    Idle {
        observation: leveler_execution::BackgroundTaskObservation,
        waited: Duration,
        /// True when the idle ceiling ended the call, false when the caller's
        /// own `timeout_seconds` did.
        idle_ceiling: Option<u64>,
        /// Output bytes this call started from, to report what arrived.
        bytes_at_start: u64,
    },
}

/// Drop the head of `buf` until it holds at most `cap` bytes, on a character
/// boundary. Used for the bounded match window and the reported delta.
fn cap_tail(buf: &mut String, cap: usize) {
    if buf.len() <= cap {
        return;
    }
    let cut = leveler_core::ceil_char_boundary(buf, buf.len() - cap);
    buf.drain(..cut);
}

/// The whole line containing byte offset `at`, trimmed. A readiness signal is
/// evidence the user can read, so the line is kept, not just the match span.
fn matched_line(window: &str, at: usize) -> String {
    let start = window[..at].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let end = window[at..]
        .find('\n')
        .map(|i| at + i)
        .unwrap_or(window.len());
    let line = window[start..end].trim();
    // The registry tags retained chunks with their stream. That annotation is
    // provenance for a log reader, not part of what the program printed, so it
    // does not belong in the one line the user reads as readiness evidence.
    line.strip_prefix("[stdout] ")
        .or_else(|| line.strip_prefix("[stderr] "))
        .unwrap_or(line)
        .to_string()
}

/// Drive one `until: exit` wait.
///
/// The wait ends on a terminal status, on the caller's bound, on the user
/// typing, or once the task has produced nothing new for [`WAIT_IDLE_SECS`].
/// Output restarts the idle clock, so a long command that talks for ten minutes
/// is still waited on in one call; only silence ends it early.
#[allow(clippy::too_many_arguments)]
async fn wait_for_exit(
    tasks: &BackgroundTaskRegistry,
    task_id: &str,
    owner: &str,
    unrestricted: bool,
    cursor: Option<u64>,
    log_budget: usize,
    timeout: Duration,
    idle_bound: u64,
    cancellation: &CancellationToken,
    yield_now: Option<&CancellationToken>,
) -> Result<ExitWait, String> {
    let call_start = tokio::time::Instant::now();
    let deadline = call_start + timeout;
    let idle_ceiling = Duration::from_secs(idle_bound).min(timeout);
    let mut idle_deadline = call_start + idle_ceiling;
    let brief = |interrupted: bool| async move {
        tasks
            .observe_authorized(
                task_id,
                owner,
                unrestricted,
                cursor,
                log_budget,
                Duration::ZERO,
                cancellation,
            )
            .await
            .map(|observation| ExitWait::Settled(observation, interrupted))
    };
    let snap = tasks.get_authorized(task_id, owner, unrestricted).await?;
    if snap.status.is_terminal() {
        return brief(false).await;
    }
    if snap.command_already_failed() {
        let _ = tasks
            .wait_or_output_authorized(
                task_id,
                owner,
                unrestricted,
                LAUNCHER_FAILURE_GRACE,
                cancellation,
            )
            .await;
        return brief(false).await;
    }
    let bytes_at_start = snap.output_bytes;
    let mut last_observed_output = bytes_at_start;
    loop {
        let now = tokio::time::Instant::now();
        let idle_ended = now >= idle_deadline;
        if idle_ended || now >= deadline {
            let observation = tasks
                .observe_authorized(
                    task_id,
                    owner,
                    unrestricted,
                    cursor,
                    log_budget,
                    Duration::ZERO,
                    cancellation,
                )
                .await?;
            if observation.snapshot.status.is_terminal()
                || observation.snapshot.command_already_failed()
            {
                return Ok(ExitWait::Settled(observation, false));
            }
            return Ok(ExitWait::Idle {
                bytes_at_start,
                waited: now.saturating_duration_since(call_start),
                // Only name the idle ceiling when it is what ended the call;
                // a caller whose own bound is shorter is told about its bound.
                idle_ceiling: (idle_ended && idle_ceiling < timeout).then_some(idle_bound),
                observation,
            });
        }
        let slice = deadline
            .min(idle_deadline)
            .saturating_duration_since(now)
            .max(Duration::from_millis(1));
        let waiting =
            tasks.wait_or_output_authorized(task_id, owner, unrestricted, slice, cancellation);
        tokio::pin!(waiting);
        let snap = match yield_now {
            Some(yield_now) => tokio::select! {
                biased;
                observed = &mut waiting => observed?,
                _ = yield_now.cancelled() => {
                    // The user typed. A terminal status observed at the same
                    // instant still wins, so a result is never hidden behind
                    // the interrupt.
                    let snap = tasks.get_authorized(task_id, owner, unrestricted).await?;
                    let interrupted = !(snap.status.is_terminal()
                        || snap.command_already_failed());
                    return brief(interrupted).await;
                }
            },
            None => waiting.await?,
        };
        if snap.status.is_terminal() {
            return brief(false).await;
        }
        if snap.command_already_failed() {
            let _ = tasks
                .wait_or_output_authorized(
                    task_id,
                    owner,
                    unrestricted,
                    LAUNCHER_FAILURE_GRACE,
                    cancellation,
                )
                .await;
            return brief(false).await;
        }
        if snap.output_bytes > last_observed_output {
            // The task is still talking: it is making progress, so give it the
            // whole bound again. Silence is what ends a wait early, never the
            // mere passage of time.
            last_observed_output = snap.output_bytes;
            idle_deadline = tokio::time::Instant::now() + idle_ceiling;
        }
    }
}

/// Report a wait that ended with the task still running.
///
/// Every line here is a fact the runtime observed: what was waited for, what
/// arrived, how long the task has been silent, and how many earlier exit waits
/// already expired on the same task. It never calls the task failed, finished,
/// or stuck — deciding what to do next belongs to the caller.
#[allow(clippy::too_many_arguments)]
fn idle_exit_report(
    snap: &leveler_execution::BackgroundTaskSnapshot,
    log: &str,
    dropped_bytes: u64,
    next_cursor: u64,
    pending: bool,
    waited: Duration,
    idle_ceiling: Option<u64>,
    bytes_at_start: u64,
    expired_exit_waits: u32,
) -> String {
    let arrived = snap.output_bytes.saturating_sub(bytes_at_start);
    let end = match idle_ceiling {
        Some(secs) => format!(
            "This wait ended after {}s because nothing observable changed: no output and no \
             terminal status for {secs}s. The task was NOT stopped, it did not fail, and it is \
             not finished \u{2014} waiting was simply no longer observing anything.",
            waited.as_secs()
        ),
        None => format!(
            "The wait bound ({}s) expired without a terminal task status. \
             The task was NOT stopped and it did not fail.",
            waited.as_secs()
        ),
    };
    let mut text = format!(
        "task_id: {} status: running next_cursor: {next_cursor}\n\
         waited_ms: {} age_ms: {} silent_ms: {} output_bytes: {}\n\
         {end}\n",
        snap.id,
        waited.as_millis(),
        snap.duration_ms,
        snap.silent_ms,
        snap.output_bytes,
    );
    text.push_str(&format!("direct_child_exit_code: {:?}\n", snap.exit_code));
    if let Some(code) = snap.exit_code {
        text.push_str(&format!(
            "The direct child exited with status {code}. Its process group is still running; \
             this is not a terminal task result.\n"
        ));
    }
    text.push_str(&format!(
        "Output during this wait: {arrived} bytes. The task has produced {} bytes in total and \
         has been silent for {}s.\n",
        snap.output_bytes,
        snap.silent_ms / 1000
    ));
    if expired_exit_waits > 1 {
        text.push_str(&format!(
            "Exit waits on this task that have already expired without a result: {} (this one \
             included). A repeat returns the same facts unless the task writes output or exits.\n",
            expired_exit_waits
        ));
    }
    if dropped_bytes > 0 {
        text.push_str(&format!(
            "[log gap: {dropped_bytes} bytes no longer retained]\n"
        ));
    }
    if !log.is_empty() {
        text.push_str("--- new log ---\n");
        text.push_str(log);
        if !log.ends_with('\n') {
            text.push('\n');
        }
    }
    if pending {
        text.push_str("[more log remains; call wait_task again]\n");
    }
    text
}

/// Wait for something that is NOT a terminal status.
///
/// A process that keeps running never reaches one, so waiting for its exit is
/// how a resident service used to block a whole goal for the full bound. These
/// modes read the task's own output instead: any new output, a caller-supplied
/// readiness pattern, or the absence of output for a quiet window. None of
/// them stops the task or claims it succeeded; a terminal exit still wins and
/// is reported through the same path as `until: exit`.
#[allow(clippy::too_many_arguments)]
async fn observe_until(
    tasks: &BackgroundTaskRegistry,
    task_id: &str,
    owner: &str,
    unrestricted: bool,
    input: &WaitInput,
    until: WaitUntil,
    timeout: Duration,
    log_budget: usize,
    cancellation: &CancellationToken,
    yield_now: Option<&CancellationToken>,
) -> Result<WaitOutcome, String> {
    let quiet = Duration::from_secs(
        input
            .quiet_seconds
            .unwrap_or(QUIET_DEFAULT_SECS)
            .clamp(1, QUIET_MAX_SECS),
    );
    let pattern = match until {
        WaitUntil::Match => {
            let raw = input.pattern.as_deref().unwrap_or("").trim();
            if raw.is_empty() {
                return Err(
                    "until \"match\" requires `pattern`: the output line that means ready".into(),
                );
            }
            if raw.len() > PATTERN_MAX_LEN {
                return Err(format!("pattern is longer than {PATTERN_MAX_LEN} bytes"));
            }
            Some(
                regex::Regex::new(raw)
                    .map_err(|error| format!("pattern is not a valid regex: {error}"))?,
            )
        }
        _ => None,
    };
    // A readiness signal may already be in the retained log (a server that
    // started before this call), so match mode reads from the beginning of what
    // the registry still holds. It never consumes the shared read position.
    let mut cursor = match until {
        WaitUntil::Match => Some(input.cursor.unwrap_or(0)),
        _ => input.cursor,
    };
    let deadline = tokio::time::Instant::now() + timeout;
    let mut window = String::new();
    let mut delta = String::new();
    let mut dropped_bytes = 0u64;
    let mut last_output = tokio::time::Instant::now();
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let observe = tasks.observe_authorized(
            task_id,
            owner,
            unrestricted,
            cursor,
            log_budget,
            remaining.min(OBSERVE_SLICE),
            cancellation,
        );
        tokio::pin!(observe);
        // The user typed: hand the round back now. A terminal status observed
        // at the same instant still wins, so this can never hide a result.
        let observation = match yield_now {
            Some(yield_now) => tokio::select! {
                biased;
                observed = &mut observe => observed?,
                _ = yield_now.cancelled() => {
                    let observed = tasks
                        .observe_authorized(
                            task_id,
                            owner,
                            unrestricted,
                            cursor,
                            log_budget,
                            Duration::ZERO,
                            cancellation,
                        )
                        .await?;
                    if observed.snapshot.status.is_terminal()
                        || observed.snapshot.command_already_failed()
                    {
                        return Ok(WaitOutcome::Terminal(Box::new(observed)));
                    }
                    return Ok(WaitOutcome::Reached(yielded_report(
                        &observed,
                        until,
                        &observed.snapshot.log,
                        observed.dropped_bytes,
                        quiet,
                    )));
                }
            },
            None => observe.await?,
        };
        if cursor.is_some() {
            cursor = Some(observation.next_cursor);
        }
        let new_bytes = !observation.snapshot.log.is_empty();
        if new_bytes {
            last_output = tokio::time::Instant::now();
            dropped_bytes = dropped_bytes.saturating_add(observation.dropped_bytes);
            delta.push_str(&observation.snapshot.log);
            cap_tail(&mut delta, MATCH_WINDOW_BYTES);
            window.push_str(&observation.snapshot.log);
            cap_tail(&mut window, MATCH_WINDOW_BYTES);
        }
        // Terminal truth outranks every readiness criterion: a task that exits
        // while printing a ready line did not become a service.
        if observation.snapshot.status.is_terminal()
            || observation.snapshot.command_already_failed()
        {
            return Ok(WaitOutcome::Terminal(Box::new(observation)));
        }
        let reached = match until {
            WaitUntil::Match => pattern.as_ref().and_then(|pattern| {
                pattern
                    .find(&window)
                    .map(|found| matched_line(&window, found.start()))
            }),
            WaitUntil::Output if new_bytes => Some(String::new()),
            WaitUntil::Quiet if !new_bytes && last_output.elapsed() >= quiet => Some(String::new()),
            _ => None,
        };
        if let Some(line) = reached {
            return Ok(WaitOutcome::Reached(readiness_report(
                &observation,
                until,
                &delta,
                dropped_bytes,
                &tasks
                    .mark_ready_authorized(task_id, owner, unrestricted, &line)
                    .await
                    .ok()
                    .filter(|_| until == WaitUntil::Match),
                quiet,
            )));
        }
        if remaining.is_zero() {
            // The bound expired with the task still running and no criterion
            // met. Report the truth; the task keeps running, unstoppable by
            // this call.
            return Ok(WaitOutcome::Reached(readiness_report(
                &observation,
                until,
                &delta,
                dropped_bytes,
                &None,
                quiet,
            )));
        }
    }
}

/// What a wait reports when it stopped observing because the user is waiting.
///
/// The observation is real and the process is untouched: this is the truth the
/// model needs to answer the person, not a failure and not a completion. It
/// also names the one thing that must not happen next — starting the same
/// process a second time.
fn yielded_report(
    observation: &leveler_execution::BackgroundTaskObservation,
    until: WaitUntil,
    delta: &str,
    dropped_bytes: u64,
    quiet: Duration,
) -> ToolOutput {
    let snap = &observation.snapshot;
    let status = match snap.status {
        BackgroundTaskStatus::Running => "running",
        BackgroundTaskStatus::Killing => "killing",
        BackgroundTaskStatus::Exited => "exited",
        BackgroundTaskStatus::Killed => "killed",
        BackgroundTaskStatus::Unknown => "unknown",
    };
    let mut text = format!(
        "task_id: {} status: {status} readiness: none elapsed_ms: {}\n\
         The wait stopped early because the user sent a message. The task itself was NOT \
         stopped, its result is NOT known yet, and this is not a failure. Answer the user \
         from what is below (and from a real check such as get_task or an HTTP probe), then \
         continue this task. Do not start the same background task again.\n",
        snap.id, snap.duration_ms
    );
    let _ = (until, quiet);
    if dropped_bytes > 0 {
        text.push_str(&format!(
            "[log gap: {dropped_bytes} bytes no longer retained]\n"
        ));
    }
    if delta.is_empty() {
        text.push_str("--- new log ---\n(no new output since the previous read)\n");
    } else {
        text.push_str("--- new log ---\n");
        let mut bounded = delta.to_string();
        cap_tail(&mut bounded, MATCH_WINDOW_BYTES);
        text.push_str(&bounded);
        if !bounded.ends_with('\n') {
            text.push('\n');
        }
    }
    ToolOutput::ok(text).with_metadata(serde_json::json!({
        "status": status,
        "readiness": "none",
        "interrupted": "user_input",
        "elapsed_ms": snap.duration_ms,
        "next_cursor": observation.next_cursor,
        "log_remaining": observation.log_remaining,
    }))
}

/// The model-facing result of a readiness observation. Says what was and was
/// not observed, and never turns either into a completion claim.
fn readiness_report(
    observation: &leveler_execution::BackgroundTaskObservation,
    until: WaitUntil,
    delta: &str,
    dropped_bytes: u64,
    ready: &Option<leveler_execution::TaskReadiness>,
    quiet: Duration,
) -> ToolOutput {
    let snap = &observation.snapshot;
    let matched = ready.is_some();
    let readiness = match (until, matched) {
        (WaitUntil::Match, true) => "matched",
        (WaitUntil::Match, false) => "none",
        (WaitUntil::Output, _) if !delta.is_empty() => "output",
        (WaitUntil::Quiet, _) => "quiet",
        _ => "none",
    };
    let mut text = format!(
        "task_id: {} status: running readiness: {readiness} elapsed_ms: {}\n",
        snap.id, snap.duration_ms
    );
    if let Some(ready) = ready {
        text.push_str(&format!("evidence: {}\n", ready.evidence));
    }
    if dropped_bytes > 0 {
        text.push_str(&format!(
            "[log gap: {dropped_bytes} bytes no longer retained]\n"
        ));
    }
    if !delta.is_empty() {
        text.push_str("--- new log ---\n");
        let mut bounded = delta.to_string();
        cap_tail(&mut bounded, MATCH_WINDOW_BYTES);
        text.push_str(&bounded);
        if !text.ends_with('\n') {
            text.push('\n');
        }
    }
    match (until, matched) {
        (WaitUntil::Match, true) => text.push_str(
            "The process observed its own readiness signal and is STILL RUNNING. Continue with \
             the work this service is a precondition for (health check, HTTP/E2E test); do not \
             wait for it to exit and do not call kill_task unless the user wants it stopped.\n",
        ),
        (WaitUntil::Match, false) => text.push_str(
            "No readiness signal yet; the process is still running. That is neither success nor a \
             hang. Check a real signal (HTTP probe, port, a startup line in get_task) rather than \
             assuming the service is up, and do not wait for its exit.\n",
        ),
        (WaitUntil::Quiet, _) if delta.is_empty() => text.push_str(&format!(
            "No new output for {}s while the process is alive. Silence is not readiness and not \
             proof of a hang.\n",
            quiet.as_secs()
        )),
        _ => text.push_str("New output arrived; the task is still running.\n"),
    }
    ToolOutput::ok(text).with_metadata(serde_json::json!({
        "status": "running",
        "readiness": readiness,
        "elapsed_ms": snap.duration_ms,
        "next_cursor": observation.next_cursor,
        "log_remaining": observation.log_remaining,
    }))
}

fn format_snap(snap: &leveler_execution::BackgroundTaskSnapshot) -> String {
    let status = match snap.status {
        BackgroundTaskStatus::Running => "running",
        BackgroundTaskStatus::Killing => "killing",
        BackgroundTaskStatus::Exited => "exited",
        BackgroundTaskStatus::Killed => "killed",
        BackgroundTaskStatus::Unknown => "unknown",
    };
    let mut text = format!(
        "task_id: {}\nstatus: {status}\nprogram: {}\nargs: {:?}\nexit_code: {:?}\n\
         duration_ms: {}\nsilent_ms: {}\noutput_bytes: {}\n\
         lifetime: {} (stopped when {})\n",
        snap.id,
        snap.program,
        snap.args,
        snap.exit_code,
        snap.duration_ms,
        snap.silent_ms,
        snap.output_bytes,
        snap.lifetime.as_str(),
        snap.lifetime.stop_condition(),
    );
    if snap.status == BackgroundTaskStatus::Running && snap.expired_exit_waits > 0 {
        text.push_str(&format!(
            "expired_exit_waits: {}\n",
            snap.expired_exit_waits
        ));
    }
    text.push_str(&format!("--- log ---\n{}", snap.log));
    text
}

/// The terminal result of a background command, in the machine-readable shape
/// the agent turns into verification evidence.
///
/// Present only when the task's own process produced a result: a natural exit,
/// or a direct child that already exited non-zero while its descendants keep the
/// group alive. A stopped or unobservable task has no result and returns `None`
/// — a cancel is not a pass and not a failure of the command.
fn background_result(
    snap: &leveler_execution::BackgroundTaskSnapshot,
) -> Option<serde_json::Value> {
    let exited = snap.status == BackgroundTaskStatus::Exited;
    let already_failed = snap.command_already_failed();
    if !exited && !already_failed {
        return None;
    }
    Some(serde_json::json!({
        "program": snap.program,
        "args": snap.args,
        "task_id": snap.id,
        "ok": exited && snap.exit_code == Some(0),
        "exit_code": snap.exit_code,
    }))
}

fn format_wait_delta(
    snap: &leveler_execution::BackgroundTaskSnapshot,
    log: &str,
    dropped_bytes: u64,
    next_cursor: u64,
    pending: bool,
) -> String {
    let status = match snap.status {
        BackgroundTaskStatus::Running => "running",
        BackgroundTaskStatus::Killing => "killing",
        BackgroundTaskStatus::Exited => "exited",
        BackgroundTaskStatus::Killed => "killed",
        BackgroundTaskStatus::Unknown => "unknown",
    };
    let mut text = format!(
        "task_id: {} status: {status} next_cursor: {next_cursor}",
        snap.id
    );
    if !matches!(
        snap.status,
        BackgroundTaskStatus::Running | BackgroundTaskStatus::Killing
    ) {
        text.push_str(&format!(
            " exit_code: {:?} duration_ms: {}",
            snap.exit_code, snap.duration_ms
        ));
    }
    if dropped_bytes > 0 {
        text.push_str(&format!(
            "\n[log gap: {dropped_bytes} bytes no longer retained]"
        ));
    }
    if !log.is_empty() {
        text.push_str("\n--- new log ---\n");
        text.push_str(log);
    }
    if pending {
        text.push_str("\n[more log remains; call wait_task again]");
    }
    text
}

async fn task_resource_authority(
    tasks: &BackgroundTaskRegistry,
    id: &str,
    context: &ToolContext,
    capability: leveler_core::Capability,
) -> bool {
    context.policy.unrestricted_execution()
        || tasks
            .resource_identity(id)
            .await
            .ok()
            .flatten()
            .is_some_and(|resource| context.policy.allows_resource(capability, &resource))
}

async fn task_grant_bindings(
    tasks: &BackgroundTaskRegistry,
    input: &serde_json::Value,
    capability: leveler_core::Capability,
) -> Result<Option<Vec<leveler_core::GrantBinding>>, String> {
    let Some(id) = input.get("task_id").and_then(serde_json::Value::as_str) else {
        return Ok(None);
    };
    Ok(tasks.resource_identity(id.trim()).await?.map(|resource| {
        vec![leveler_core::GrantBinding {
            capability,
            resource,
        }]
    }))
}

async fn task_admission_reason(
    tasks: &BackgroundTaskRegistry,
    input: &serde_json::Value,
    context: &ToolContext,
    capability: leveler_core::Capability,
) -> Option<String> {
    if context.policy.unrestricted_execution() {
        return None;
    }
    let id = input.get("task_id")?.as_str()?.trim();
    if task_resource_authority(tasks, id, context, capability).await {
        return None;
    }
    let owner = tasks.peek_task_owner(id).await.ok()?;
    (owner.as_deref() != Some(context.session_scope())).then(||
        format!("task `{id}` belongs to another session or the runtime; explicit approval is required for this exact action"))
}

pub struct GetTaskTool {
    tasks: Arc<BackgroundTaskRegistry>,
}

impl GetTaskTool {
    pub fn new(tasks: Arc<BackgroundTaskRegistry>) -> Self {
        Self { tasks }
    }
}

#[async_trait]
impl Tool for GetTaskTool {
    fn name(&self) -> &'static str {
        "get_task"
    }

    fn description(&self) -> &'static str {
        "Get status and recent log of a background task started with \
         run_command(background=true)."
    }

    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<TaskIdInput>()
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Safe
    }

    /// A crash between "ask for the task" and "record the answer" loses only
    /// the answer: the task registry still holds the task. Skipping it lets the
    /// session resume; the model reads it again on its next round.
    fn recovery_is_skippable(&self) -> bool {
        true
    }

    async fn admission_reason(
        &self,
        input: &serde_json::Value,
        context: &ToolContext,
    ) -> Option<String> {
        task_admission_reason(
            &self.tasks,
            input,
            context,
            leveler_core::Capability::BackgroundTaskObserve,
        )
        .await
    }

    async fn grant_bindings(
        &self,
        input: &serde_json::Value,
        _context: &ToolContext,
    ) -> Result<Option<Vec<leveler_core::GrantBinding>>, String> {
        task_grant_bindings(
            &self.tasks,
            input,
            leveler_core::Capability::BackgroundTaskObserve,
        )
        .await
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        _cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: TaskIdInput = super::parse_input(self.name(), input)?;
        let reg = &self.tasks;
        match reg
            .get_authorized(
                input.task_id.trim(),
                context.session_scope(),
                task_resource_authority(
                    &self.tasks,
                    input.task_id.trim(),
                    &context,
                    leveler_core::Capability::BackgroundTaskObserve,
                )
                .await,
            )
            .await
        {
            Ok(snap) => Ok(
                ToolOutput::ok(format_snap(&snap)).with_metadata(serde_json::json!({
                    "background_result": background_result(&snap),
                    "terminal_task_id": snap.status.is_terminal().then_some(&snap.id),
                })),
            ),
            Err(error) => Ok(ToolOutput::error(error)),
        }
    }
}

pub struct WaitTaskTool {
    tasks: Arc<BackgroundTaskRegistry>,
    /// Silence window for `until: exit`. Production always uses
    /// [`WAIT_IDLE_SECS`]; a test constructs its own tool with a one-second
    /// window rather than spending three minutes proving the same code path.
    idle_ceiling: u64,
}

impl WaitTaskTool {
    pub fn new(tasks: Arc<BackgroundTaskRegistry>) -> Self {
        Self {
            tasks,
            idle_ceiling: WAIT_IDLE_SECS,
        }
    }

    #[cfg(all(test, unix))]
    fn with_idle_ceiling(mut self, secs: u64) -> Self {
        self.idle_ceiling = secs;
        self
    }
}

#[async_trait]
impl Tool for WaitTaskTool {
    fn name(&self) -> &'static str {
        "wait_task"
    }

    fn description(&self) -> &'static str {
        "Wait on a background task started with run_command(background=true). \
         `until` decides what to wait for. `until: \"exit\"` (default) returns when the \
         task reaches a terminal status (exited/killed), when the bounded interval \
         expires, when the run is cancelled, or when the task has produced no output and \
         no status change for 180s — at that point the wait is observing nothing new, \
         and the result states exactly what was seen (silent_ms, output_bytes, and how \
         many earlier exit waits already expired). None of these stops the task; an early \
         return is control, not a verdict. A service that keeps running never produces a \
         terminal status at all, so do not wait for its exit: pass `until: \"match\"` with \
         a `pattern` the program prints when it is ready (for example \"listening on\"), \
         and this returns as soon as that line appears while the process stays up. \
         `until: \"output\"` returns on any new \
         output; `until: \"quiet\"` returns after `quiet_seconds` with no new output. \
         A readiness match is evidence from the task's own output, not a health \
         check, and it never marks the task finished. \
         Returns bounded new log and next_cursor; pass cursor to keep an independent \
         reader position. Without cursor, repeated calls use a shared incremental position. \
         timeout_seconds defaults to 600s (max 600s); expiry returns the current running \
         status without cancelling the task. get_task reads the full retained log."
    }

    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<WaitInput>()
    }

    fn risk(&self) -> RiskLevel {
        // Not Safe, even though the runtime — not this tool — now performs the
        // settlement. Waiting consumes the task's settlement report exactly
        // once, and a crash replay would block recovery for up to two minutes
        // and swallow that report. Neither belongs in an unattended re-run.
        RiskLevel::WorkspaceWrite
    }

    /// Not re-running an interrupted wait IS the safe choice here: a replay is
    /// what consumes the settlement, so skipping is the only option that leaves
    /// the report intact. The task (and any settlement still held for it) is in
    /// the registry, so skipping loses an answer, not a fact. Without this a
    /// session interrupted while polling a long suite could not be resumed at
    /// all: recovery halted and the only advice was a flag the TUI cannot pass.
    fn recovery_is_skippable(&self) -> bool {
        true
    }

    async fn admission_reason(
        &self,
        input: &serde_json::Value,
        context: &ToolContext,
    ) -> Option<String> {
        task_admission_reason(
            &self.tasks,
            input,
            context,
            leveler_core::Capability::BackgroundTaskObserve,
        )
        .await
    }

    async fn grant_bindings(
        &self,
        input: &serde_json::Value,
        _context: &ToolContext,
    ) -> Result<Option<Vec<leveler_core::GrantBinding>>, String> {
        task_grant_bindings(
            &self.tasks,
            input,
            leveler_core::Capability::BackgroundTaskObserve,
        )
        .await
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: WaitInput = super::parse_input(self.name(), input)?;
        let reg = &self.tasks;
        let task_id = input.task_id.trim().to_string();
        let timeout = wait_interval(input.timeout_seconds);
        let until = input.until.unwrap_or(WaitUntil::Exit);
        if until == WaitUntil::Match && input.pattern.as_deref().unwrap_or("").trim().is_empty() {
            return Ok(ToolOutput::error(
                "until \"match\" requires `pattern`: the output line that means the task is ready\n",
            ));
        }
        if until != WaitUntil::Match && input.pattern.is_some() {
            return Ok(ToolOutput::error(
                "`pattern` is only read by until \"match\"\n",
            ));
        }
        // The capability owns observation, waiting and reader positions.
        // Leave room for status and settlement under the model-facing cap.
        let log_budget = context
            .policy
            .tool_output_budget
            .saturating_sub(512)
            .min(16 * 1024);
        // The user's own typing is an execution opportunity: an observation
        // call must hand the round back when input is waiting for it. Set per
        // call by the host; absent for a caller with no user (eval, CLI pipe).
        let yield_now = context.yield_now.clone();
        let authority = task_resource_authority(
            &self.tasks,
            input.task_id.trim(),
            &context,
            leveler_core::Capability::BackgroundTaskObserve,
        )
        .await;
        // Set when this call ended with the task still running, so the shared
        // post-wait path reports the observed facts instead of a bare status.
        let mut idle_exit: Option<(Duration, Option<u64>, u64, u32)> = None;
        let wait_result = if until == WaitUntil::Exit {
            // Wait for a TERMINAL status transition, not for the next log line:
            // one `wait_task` costs one model round, so returning while the
            // task is still running turns a long command into a poll loop.
            // Output restarts the idle clock, so a long command that keeps
            // talking is waited on as before; only silence hands the round
            // back with what was actually observed.
            match wait_for_exit(
                reg,
                &task_id,
                context.session_scope(),
                authority,
                input.cursor,
                log_budget,
                timeout,
                self.idle_ceiling,
                &cancellation,
                yield_now.as_ref(),
            )
            .await
            {
                Ok(ExitWait::Settled(observation, interrupted)) => Ok((observation, interrupted)),
                Ok(ExitWait::Idle {
                    observation,
                    waited,
                    idle_ceiling,
                    bytes_at_start,
                }) => {
                    let expired = reg
                        .note_expired_exit_wait_authorized(
                            &task_id,
                            context.session_scope(),
                            task_resource_authority(
                                &self.tasks,
                                input.task_id.trim(),
                                &context,
                                leveler_core::Capability::BackgroundTaskObserve,
                            )
                            .await,
                        )
                        .await
                        .unwrap_or(1);
                    idle_exit = Some((waited, idle_ceiling, bytes_at_start, expired));
                    Ok((observation, false))
                }
                Err(error) => Err(error),
            }
        } else {
            match observe_until(
                reg,
                &task_id,
                context.session_scope(),
                task_resource_authority(
                    &self.tasks,
                    input.task_id.trim(),
                    &context,
                    leveler_core::Capability::BackgroundTaskObserve,
                )
                .await,
                &input,
                until,
                timeout,
                log_budget,
                &cancellation,
                yield_now.as_ref(),
            )
            .await
            {
                Ok(WaitOutcome::Reached(output)) => return Ok(output),
                Ok(WaitOutcome::Terminal(observation)) => Ok((*observation, false)),
                Err(error) => Err(error),
            }
        };
        match wait_result {
            Ok((observation, interrupted)) => {
                // The wait yielded to the user while the task is still alive:
                // report the observation and let this round end so the message
                // reaches the model at the top of the next one.
                if interrupted
                    && matches!(
                        observation.snapshot.status,
                        BackgroundTaskStatus::Running | BackgroundTaskStatus::Killing
                    )
                    && !observation.snapshot.command_already_failed()
                {
                    return Ok(yielded_report(
                        &observation,
                        WaitUntil::Exit,
                        &observation.snapshot.log,
                        observation.dropped_bytes,
                        std::time::Duration::ZERO,
                    ));
                }
                let gap = observation.dropped_bytes;
                let next_cursor = observation.next_cursor;
                let pending = observation.log_remaining;
                let snap = observation.snapshot;
                let log = &snap.log;
                if matches!(
                    snap.status,
                    BackgroundTaskStatus::Running | BackgroundTaskStatus::Killing
                ) {
                    // The command itself has already failed. Descendants may
                    // still hold the process group; that keeps the task
                    // stoppable, and it must not keep this turn waiting.
                    if snap.command_already_failed() {
                        let code = snap.exit_code.unwrap_or(1);
                        let mut text = format_wait_delta(&snap, log, gap, next_cursor, pending);
                        text.push_str(&format!(
                            "\nThe command already exited with status {code}. Processes it left behind are still in its process group, so this task is still running and still stoppable. Handle this failure; do not wait for the task to end.\n"
                        ));
                        return Ok(ToolOutput::error(text).with_metadata(serde_json::json!({
                            "exit_code": snap.exit_code,
                            "duration_ms": snap.duration_ms,
                            "next_cursor": next_cursor,
                            "log_remaining": pending,
                            "background_result": background_result(&snap),
                        })));
                    }
                    // Interval elapsed (or a kill is in flight). Do not consume the
                    // mutation baseline — that runs once, at true terminal.
                    let mut text = match idle_exit.take() {
                        Some((waited, idle_ceiling, bytes_at_start, expired)) => idle_exit_report(
                            &snap,
                            log,
                            gap,
                            next_cursor,
                            pending,
                            waited,
                            idle_ceiling,
                            bytes_at_start,
                            expired,
                        ),
                        None => format_wait_delta(&snap, log, gap, next_cursor, pending),
                    };
                    if text.is_empty() {
                        text = format_wait_delta(&snap, log, gap, next_cursor, pending);
                    }
                    return Ok(ToolOutput::ok(text).with_metadata(serde_json::json!({
                        "exit_code": snap.exit_code,
                        "duration_ms": snap.duration_ms,
                        "silent_ms": snap.silent_ms,
                        "output_bytes": snap.output_bytes,
                        "lifetime": snap.lifetime.as_str(),
                        "next_cursor": next_cursor,
                        "log_remaining": pending,
                    })));
                }
                // The task is terminal, so the runtime already settled it when
                // entire process group exited and its admitted namespace was diffed.
                // This reads that result — it does not produce it.
                let settlement = observation.settlement;
                let report = match reg
                    .take_settlement_authorized(
                        &task_id,
                        context.session_scope(),
                        task_resource_authority(
                            &self.tasks,
                            input.task_id.trim(),
                            &context,
                            leveler_core::Capability::BackgroundTaskObserve,
                        )
                        .await,
                    )
                    .await
                {
                    Ok(report) => report,
                    Err(error) => return Ok(ToolOutput::error(error)),
                };

                let mut text = format_wait_delta(&snap, log, gap, next_cursor, pending);
                let mut diagnostic = String::new();
                if let Some(note) = settlement.as_ref().and_then(|s| s.note.as_deref()) {
                    diagnostic.push_str("\n[note] ");
                    diagnostic.push_str(note);
                    diagnostic.push('\n');
                }
                let violation = settlement.as_ref().and_then(|s| s.violation.as_deref());
                if let Some(violation) = violation {
                    diagnostic.push_str("\n[mutation rejected] ");
                    diagnostic.push_str(violation);
                    diagnostic.push('\n');
                }
                // Never let verbose diagnostics trigger an outer truncation
                // of bytes the reader cursor has already acknowledged.
                text.push_str(&crate::registry::cap_output_with(
                    &diagnostic,
                    context.policy.tool_output_budget.saturating_sub(text.len()),
                ));

                let failed = snap.status != BackgroundTaskStatus::Exited
                    || snap.exit_code.unwrap_or(1) != 0
                    || violation.is_some();
                let out = if failed {
                    ToolOutput::error(text)
                } else {
                    ToolOutput::ok(text)
                };
                Ok(out.with_metadata(serde_json::json!({
                    "exit_code": snap.exit_code,
                    "modified_files": report
                        .as_ref()
                        .map(|s| s.modified.clone())
                        .unwrap_or_default(),
                    "workspace_snapshot": report.as_ref().and_then(|s| s.snapshot.as_ref()).map(|id| id.0.clone()),
                    "next_cursor": next_cursor,
                    "log_remaining": pending,
                    // The real result of the command this task ran. The agent
                    // records it as verification evidence, so a test that was
                    // moved to the background can still justify completion.
                    "background_result": background_result(&snap),
                    "terminal_task_id": snap.id,
                })))
            }
            Err(e) => Ok(ToolOutput::error(e)),
        }
    }
}

pub struct KillTaskTool {
    tasks: Arc<BackgroundTaskRegistry>,
}

impl KillTaskTool {
    pub fn new(tasks: Arc<BackgroundTaskRegistry>) -> Self {
        Self { tasks }
    }
}

#[async_trait]
impl Tool for KillTaskTool {
    fn name(&self) -> &'static str {
        "kill_task"
    }

    fn description(&self) -> &'static str {
        "Terminate a background task (SIGTERM/kill). Safe if already exited."
    }

    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<TaskIdInput>()
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::WorkspaceWrite
    }

    async fn admission_reason(
        &self,
        input: &serde_json::Value,
        context: &ToolContext,
    ) -> Option<String> {
        task_admission_reason(
            &self.tasks,
            input,
            context,
            leveler_core::Capability::BackgroundTaskControl,
        )
        .await
    }

    async fn grant_bindings(
        &self,
        input: &serde_json::Value,
        _context: &ToolContext,
    ) -> Result<Option<Vec<leveler_core::GrantBinding>>, String> {
        task_grant_bindings(
            &self.tasks,
            input,
            leveler_core::Capability::BackgroundTaskControl,
        )
        .await
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        _cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: TaskIdInput = super::parse_input(self.name(), input)?;
        let reg = &self.tasks;
        match reg
            .kill_authorized(
                input.task_id.trim(),
                context.session_scope(),
                task_resource_authority(
                    &self.tasks,
                    input.task_id.trim(),
                    &context,
                    leveler_core::Capability::BackgroundTaskControl,
                )
                .await,
            )
            .await
        {
            Ok(snap) => Ok(
                ToolOutput::ok(format_snap(&snap)).with_metadata(serde_json::json!({
                    "terminal_task_id": snap.status.is_terminal().then_some(&snap.id),
                })),
            ),
            Err(e) => Ok(ToolOutput::error(e)),
        }
    }
}

// Every test here drives background mutations through a POSIX `sh -c` + git
// fixture; Windows background/rollback behavior is covered by the `windows_`
// canary tests.
#[cfg(all(test, unix))]
mod tests {
    #[tokio::test]
    async fn resource_session_grant_controls_only_the_bound_foreign_task() {
        let dir = scratch_repo();
        let (ctx, registry, commands) = ctx_with_reg(dir.path());
        let owner = ctx.clone().with_session_scope("owner");
        let target = spawn_background(&commands, &owner, "sleep 30").await;
        let other = spawn_background(&commands, &owner, "sleep 30").await;
        let resource = registry.resource_identity(&target).await.unwrap().unwrap();
        let policy = leveler_execution::ResolvedExecutionPolicy::new(
            leveler_execution::WriteScope::None,
            leveler_execution::NetworkScope::None,
            leveler_execution::AuthorizationEvidence::ResourceGrant {
                request: leveler_core::GrantRequest {
                    project_identity: "p".into(),
                    bindings: vec![
                        leveler_core::GrantBinding {
                            capability: leveler_core::Capability::BackgroundTaskControl,
                            resource: resource.clone(),
                        },
                        leveler_core::GrantBinding {
                            capability: leveler_core::Capability::BackgroundTaskObserve,
                            resource,
                        },
                    ],
                },
                scope: leveler_core::GrantScope::Session,
            },
        );
        let approved = ctx
            .with_session_scope("foreign")
            .with_resolved_policy(policy);
        assert!(!approved.policy.unrestricted_execution());
        let (_new_context, new_registry, new_commands) = ctx_with_reg(dir.path());
        let replacement_task = spawn_background(&new_commands, &owner, "sleep 30").await;
        assert_eq!(
            target, replacement_task,
            "fixture exercises reused local task number"
        );
        assert_ne!(
            registry.resource_identity(&target).await.unwrap(),
            new_registry
                .resource_identity(&replacement_task)
                .await
                .unwrap()
        );
        let replay = GetTaskTool::new(new_registry.clone())
            .execute(
                serde_json::json!({"task_id":replacement_task}),
                approved.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let _ = new_registry.kill(&replacement_task).await;
        assert!(
            replay.is_error,
            "same number in a new registry must not replay a grant"
        );
        let killer = KillTaskTool::new(registry.clone());
        let mut observe_request = approved.policy.resource_grant().unwrap().clone();
        observe_request.bindings.retain(|binding| {
            binding.capability == leveler_core::Capability::BackgroundTaskObserve
        });
        let observe_only =
            approved
                .clone()
                .with_resolved_policy(leveler_execution::ResolvedExecutionPolicy::new(
                    leveler_execution::WriteScope::None,
                    leveler_execution::NetworkScope::None,
                    leveler_execution::AuthorizationEvidence::ResourceGrant {
                        request: observe_request,
                        scope: leveler_core::GrantScope::Session,
                    },
                ));
        let cannot_kill = killer
            .execute(
                serde_json::json!({"task_id":target}),
                observe_only,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(cannot_kill.is_error);
        assert_eq!(
            registry.get(&target).await.unwrap().status,
            BackgroundTaskStatus::Running
        );
        let getter = GetTaskTool::new(registry.clone());
        assert_ne!(
            getter
                .grant_bindings(&serde_json::json!({"task_id":target}), &approved)
                .await
                .unwrap(),
            killer
                .grant_bindings(&serde_json::json!({"task_id":target}), &approved)
                .await
                .unwrap()
        );
        let target_result = killer
            .execute(
                serde_json::json!({"task_id":target}),
                approved.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let other_result = killer
            .execute(
                serde_json::json!({"task_id":other}),
                approved.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let other_status = registry.get(&other).await.unwrap().status;
        let target_status = registry.get(&target).await.unwrap().status;
        let owner_after = registry.peek_task_owner(&target).await.unwrap();
        let _ = registry.kill(&other).await;
        assert!(!target_result.is_error, "{}", target_result.content);
        assert!(other_result.is_error);
        assert!(matches!(
            target_status,
            BackgroundTaskStatus::Killed | BackgroundTaskStatus::Killing
        ));
        assert_eq!(other_status, BackgroundTaskStatus::Running);
        assert_eq!(owner_after.as_deref(), Some("owner"));
        let repeated = GetTaskTool::new(registry)
            .execute(
                serde_json::json!({"task_id":target}),
                approved,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!repeated.is_error, "same resource remains authorized");
    }

    #[tokio::test]
    async fn foreign_task_admission_requests_approval_without_consuming_owner_evidence() {
        let dir = scratch_repo();
        let (ctx, registry, commands) = ctx_with_reg(dir.path());
        let owner = ctx.clone().with_session_scope("owner");
        let foreign = ctx.clone().with_session_scope("foreign");
        let id = spawn_background(
            &commands,
            &owner,
            "printf retained-log; printf changed > own.txt",
        )
        .await;
        registry
            .wait(&id, Some(Duration::from_secs(5)), &CancellationToken::new())
            .await
            .unwrap();
        let tools: Vec<Box<dyn Tool>> = vec![
            Box::new(GetTaskTool::new(registry.clone())),
            Box::new(WaitTaskTool::new(registry.clone())),
            Box::new(KillTaskTool::new(registry.clone())),
        ];
        for tool in tools {
            assert!(
                tool.admission_reason(&serde_json::json!({"task_id":id}), &foreign)
                    .await
                    .is_some(),
                "{} must ask for foreign control",
                tool.name()
            );
            assert!(
                tool.admission_reason(&serde_json::json!({"task_id":id}), &owner)
                    .await
                    .is_none()
            );
            assert!(
                tool.admission_reason(&serde_json::json!({"task_id":"missing"}), &foreign)
                    .await
                    .is_none()
            );
        }
        let observation = registry
            .observe_owned(
                &id,
                "owner",
                None,
                1024,
                Duration::ZERO,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(observation.snapshot.log.contains("retained-log"));
        let settlement = registry
            .take_settlement_owned(&id, "owner")
            .await
            .unwrap()
            .unwrap();
        assert!(settlement.modified.iter().any(|path| path == "own.txt"));
    }

    #[tokio::test]
    async fn full_task_tools_control_other_session_and_scoped_tools_still_refuse() {
        let dir = scratch_repo();
        let (ctx, registry, commands) = ctx_with_reg(dir.path());
        let owner = ctx.clone().with_session_scope("owner");
        let id = spawn_background(&commands, &owner, "printf other-session-log; sleep 30").await;
        let scoped = ctx.clone().with_session_scope("foreign");
        let full = ToolContext::with_environment(
            Workspace::new(dir.path()).unwrap(),
            PermissionProfile::FullAccess,
            ctx.execution.environment.clone(),
        )
        .with_session_scope("foreign");
        let getter = GetTaskTool::new(registry.clone());
        assert!(
            getter
                .execute(
                    serde_json::json!({"task_id":id}),
                    scoped.clone(),
                    CancellationToken::new()
                )
                .await
                .unwrap()
                .is_error
        );
        assert!(
            !getter
                .execute(
                    serde_json::json!({"task_id":id}),
                    full.clone(),
                    CancellationToken::new()
                )
                .await
                .unwrap()
                .is_error
        );
        let killer = KillTaskTool::new(registry.clone());
        assert!(
            killer
                .execute(
                    serde_json::json!({"task_id":id}),
                    scoped,
                    CancellationToken::new()
                )
                .await
                .unwrap()
                .is_error
        );
        assert!(
            !killer
                .execute(
                    serde_json::json!({"task_id":id}),
                    full.clone(),
                    CancellationToken::new()
                )
                .await
                .unwrap()
                .is_error
        );
        let output = WaitTaskTool::new(registry)
            .execute(
                serde_json::json!({"task_id":id,"cursor":0,"timeout_seconds":5}),
                full,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            !output.content.contains("another session"),
            "{}",
            output.content
        );
        assert!(
            output.content.contains("status: killed") || output.content.contains("status: exited"),
            "{}",
            output.content
        );
    }

    use super::*;
    use crate::tool::{Tool, ToolContext};
    use crate::tools::RunCommandTool;
    use leveler_execution::{BackgroundTaskRegistry, PermissionProfile, Workspace};
    use leveler_test_support::git::{run, scratch_repo};
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn wait_task_is_workspace_write_risk() {
        // Waiting observes a previously admitted write process and consumes
        // its settlement exactly once; it is not an auto-replayable read.
        let reg = Arc::new(BackgroundTaskRegistry::new());
        assert_eq!(
            WaitTaskTool::new(reg.clone()).risk(),
            RiskLevel::WorkspaceWrite
        );
        // get_task stays a pure status read.
        assert_eq!(GetTaskTool::new(reg).risk(), RiskLevel::Safe);
    }

    #[test]
    fn wait_interval_is_capped() {
        assert_eq!(wait_interval(None).as_secs(), WAIT_DEFAULT_SECS);
        assert_eq!(wait_interval(Some(1)).as_secs(), 1);
        assert_eq!(wait_interval(Some(900)).as_secs(), WAIT_MAX_SECS);
    }

    #[tokio::test]
    async fn wait_interval_returns_running_as_ok_without_killing() {
        let dir = scratch_repo();
        let (ctx, reg, commands) = ctx_with_reg(dir.path());
        let start = RunCommandTool::new(commands.clone())
            .execute(
                serde_json::json!({
                    "program": "sleep",
                    "args": ["30"],
                    "background": true,
                }),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!start.is_error, "spawn: {}", start.content);
        let task_id = start
            .content
            .lines()
            .find_map(|l| l.strip_prefix("task_id: "))
            .expect("task_id")
            .to_string();

        let wait = WaitTaskTool::new(reg.clone())
            .execute(
                serde_json::json!({"task_id": task_id, "timeout_seconds": 1}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            !wait.is_error,
            "still-running wait must not be a tool error: {}",
            wait.content
        );
        assert!(
            wait.content.contains("status: running"),
            "expected running snapshot: {}",
            wait.content
        );
        let snap = reg.get(&task_id).await.expect("retained");
        assert_eq!(snap.status, BackgroundTaskStatus::Running);
        let _ = reg.kill(&task_id).await;
    }

    #[tokio::test]
    async fn explicit_reader_does_not_consume_default_readers_log() {
        let dir = scratch_repo();
        let (ctx, reg, commands) = ctx_with_reg(dir.path());
        let start = RunCommandTool::new(commands)
            .execute(
                serde_json::json!({"program":"sh", "args":["-c", "printf 'independent-reader\\n'"], "background":true}),
                ctx.clone(), CancellationToken::new(),
            ).await.unwrap();
        let id = start
            .content
            .lines()
            .find_map(|l| l.strip_prefix("task_id: "))
            .unwrap();
        reg.wait(id, Some(Duration::from_secs(5)), &CancellationToken::new())
            .await
            .unwrap();
        let wait = WaitTaskTool::new(reg);
        let explicit = wait
            .execute(
                serde_json::json!({"task_id":id,"cursor":0}),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let implicit = wait
            .execute(
                serde_json::json!({"task_id":id}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(explicit.content.contains("independent-reader"));
        assert!(
            implicit.content.contains("independent-reader"),
            "an independent reader stole the default reader's output: {}",
            implicit.content
        );
    }

    /// A command that exits non-zero has already failed, even when a descendant
    /// it started is still in the process group. The wait returns that failure
    /// so the turn can handle it. The task stays running and stoppable: the
    /// group is still this task's workload.
    #[tokio::test]
    async fn wait_task_returns_when_the_command_fails_and_descendants_remain() {
        let dir = scratch_repo();
        let (ctx, reg, commands) = ctx_with_reg(dir.path());
        let id = spawn_background(&commands, &ctx, "sleep 30 & exit 1").await;
        let out = tokio::time::timeout(
            Duration::from_secs(5),
            WaitTaskTool::new(reg.clone()).execute(
                serde_json::json!({"task_id": id}),
                ctx,
                CancellationToken::new(),
            ),
        )
        .await
        .expect("a failed command must not wait out its leftover process group")
        .unwrap();
        assert!(out.is_error, "{}", out.content);
        assert_eq!(out.metadata["exit_code"], 1);
        assert!(
            out.content.contains("exited"),
            "the result names the command's exit: {}",
            out.content
        );
        let snap = reg.get(&id).await.expect("task retained");
        assert_eq!(
            snap.status,
            BackgroundTaskStatus::Running,
            "leftover descendants keep the task stoppable: {:?}",
            snap.status
        );
        reg.kill(&id).await.expect("kill still reaches the group");
    }

    /// A launcher that exits 0 and leaves the real workload behind is still
    /// that workload. `wait_task` does not treat the shell's success as the end.
    #[tokio::test]
    async fn wait_task_keeps_waiting_when_a_successful_launcher_leaves_descendants() {
        let dir = scratch_repo();
        let (ctx, reg, commands) = ctx_with_reg(dir.path());
        let id = spawn_background(&commands, &ctx, "sleep 30 &").await;
        let tool = WaitTaskTool::new(reg.clone());
        let wait = tool.execute(
            serde_json::json!({"task_id": id}),
            ctx,
            CancellationToken::new(),
        );
        tokio::pin!(wait);
        assert!(
            tokio::time::timeout(Duration::from_millis(1000), &mut wait)
                .await
                .is_err(),
            "a successful launcher must not end the wait while its descendant runs"
        );
        let _ = reg.kill(&id).await;
    }

    /// `wait_task` waits for a TERMINAL status transition, not for output. A
    /// chatty long command used to return control on every log line, which cost
    /// one model round per line; the wait must stay pending while the task is
    /// still running and deliver the accumulated output once it exits.
    #[tokio::test]
    async fn wait_does_not_return_on_output_while_the_task_still_runs() {
        let dir = scratch_repo();
        let (ctx, reg, commands) = ctx_with_reg(dir.path());
        let start = RunCommandTool::new(commands).execute(
            serde_json::json!({"program":"sh", "args":["-c", "printf 'early-output\\n'; sleep 2; printf 'final-output\\n'"], "background":true}),
            ctx.clone(), CancellationToken::new(),
        ).await.unwrap();
        let id = start
            .content
            .lines()
            .find_map(|l| l.strip_prefix("task_id: "))
            .unwrap()
            .to_string();
        let waiter = WaitTaskTool::new(reg.clone());
        let wait = waiter.execute(
            serde_json::json!({"task_id":id}),
            ctx,
            CancellationToken::new(),
        );
        tokio::pin!(wait);
        // The task prints immediately and keeps running. One second in, the
        // wait must still be pending: output arrival alone must not satisfy it,
        // or the model is handed a "still running" result to poll again.
        assert!(
            tokio::time::timeout(Duration::from_millis(1000), &mut wait)
                .await
                .is_err(),
            "wait_task returned while the task was still running"
        );
        let output = tokio::time::timeout(Duration::from_secs(10), &mut wait)
            .await
            .expect("terminal status must wake the wait")
            .unwrap();
        assert!(
            output.content.contains("status: exited"),
            "{}",
            output.content
        );
        assert!(
            output.content.contains("early-output"),
            "{}",
            output.content
        );
        assert!(
            output.content.contains("final-output"),
            "{}",
            output.content
        );
        assert_eq!(output.metadata["exit_code"], 0);
    }

    /// The `wait_task` contract, one test per terminal outcome. These are the
    /// cases the model must be able to rely on in ONE call: success and failure
    /// wake the wait from the process's own exit, cancellation wakes it from the
    /// parent's token, and expiry returns the running status without touching
    /// the task.
    /// BG-05/BG-06/BG-14: a resident service must be observable WITHOUT waiting
    /// for its exit. `until: match` returns on the program's own readiness line
    /// and leaves the process running; the registry records the same evidence
    /// every projection reads.
    #[tokio::test]
    async fn wait_until_match_returns_at_readiness_and_keeps_the_process_alive() {
        let dir = scratch_repo();
        let (ctx, reg, commands) = ctx_with_reg(dir.path());
        let id = spawn_background(
            &commands,
            &ctx,
            "printf 'booting\\n'; sleep 0.3; printf 'listening on :8080\\n'; sleep 30",
        )
        .await;
        let started = std::time::Instant::now();
        let out = WaitTaskTool::new(reg.clone())
            .execute(
                serde_json::json!({
                    "task_id": id,
                    "until": "match",
                    "pattern": "listening on",
                    "timeout_seconds": 20,
                }),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(15),
            "readiness must return long before the task's own exit: {:?}",
            started.elapsed()
        );
        assert!(
            out.content.contains("readiness: matched"),
            "{}",
            out.content
        );
        assert!(
            out.content.contains("evidence: listening on :8080"),
            "{}",
            out.content
        );
        assert!(
            out.content.contains("STILL RUNNING"),
            "readiness is not completion: {}",
            out.content
        );
        assert_eq!(out.metadata["readiness"], "matched");
        assert_eq!(out.metadata["status"], "running");
        let snap = reg
            .get(&id)
            .await
            .expect("task survives its readiness wait");
        assert_eq!(snap.status, BackgroundTaskStatus::Running);
        assert_eq!(
            snap.ready.as_ref().map(|ready| ready.evidence.as_str()),
            Some("listening on :8080")
        );
    }

    /// The user's message is a reason to stop observing, never a reason to
    /// stop the process. A wait that yields says what it saw, keeps the task
    /// running, and claims neither success nor failure.
    #[tokio::test]
    async fn a_yield_ends_the_wait_and_leaves_the_process_running() {
        let dir = scratch_repo();
        let (ctx, reg, commands) = ctx_with_reg(dir.path());
        let id = spawn_background(&commands, &ctx, "printf 'up\n'; sleep 30").await;
        // Let the reader take the process's first line: what the model answers
        // the user with must be the output it actually saw.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let yield_now = CancellationToken::new();
        yield_now.cancel();
        let started = std::time::Instant::now();
        let out = WaitTaskTool::new(reg.clone())
            .execute(
                serde_json::json!({ "task_id": id, "cursor": 0, "timeout_seconds": 600 }),
                ctx.clone().with_yield_now(yield_now),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "the bound was 600s; yielding must not wait for it"
        );
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("status: running"), "{}", out.content);
        assert!(out.content.contains("up"), "{}", out.content);
        assert_eq!(out.metadata["interrupted"], "user_input");
        let snap = reg.get(&id).await.expect("task");
        assert_eq!(snap.status, BackgroundTaskStatus::Running);
        reg.kill_owned(&id, "default").await.ok();
    }

    /// Terminal truth outranks the yield: a process that ends at the same
    /// instant the user types still reports its result.
    #[tokio::test]
    async fn a_terminal_result_is_not_lost_to_a_yield() {
        let dir = scratch_repo();
        let (ctx, reg, commands) = ctx_with_reg(dir.path());
        let id = spawn_background(&commands, &ctx, "printf 'done\n'; exit 0").await;
        // Wait for the process to actually end, then yield "at the same time".
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !reg.get(&id).await.unwrap().status.is_terminal() {
            assert!(std::time::Instant::now() < deadline, "task never exited");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let yield_now = CancellationToken::new();
        yield_now.cancel();
        let out = WaitTaskTool::new(reg.clone())
            .execute(
                serde_json::json!({ "task_id": id }),
                ctx.with_yield_now(yield_now),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(out.content.contains("exited"), "{}", out.content);
        assert!(out.content.contains("done"), "{}", out.content);
        assert_ne!(out.metadata["interrupted"], "user_input");
    }

    /// A readiness wait yields the same way, without inventing readiness.
    #[tokio::test]
    async fn a_yield_ends_a_match_wait_without_readiness() {
        let dir = scratch_repo();
        let (ctx, reg, commands) = ctx_with_reg(dir.path());
        let id = spawn_background(&commands, &ctx, "sleep 30").await;
        let yield_now = CancellationToken::new();
        yield_now.cancel();
        let started = std::time::Instant::now();
        let out = WaitTaskTool::new(reg.clone())
            .execute(
                serde_json::json!({
                    "task_id": id,
                    "until": "match",
                    "pattern": "listening on",
                    "timeout_seconds": 600,
                }),
                ctx.with_yield_now(yield_now),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(out.metadata["readiness"], "none");
        assert_eq!(out.metadata["status"], "running");
        assert_eq!(out.metadata["interrupted"], "user_input");
        assert!(
            reg.get(&id).await.unwrap().ready.is_none(),
            "a yielded wait must not record readiness evidence"
        );
        reg.kill_owned(&id, "default").await.ok();
    }

    /// BG-06: no signal is no signal. The wait reports the running process and
    /// records NO readiness, so no projection can render a service as ready.
    #[tokio::test]
    async fn wait_until_match_without_a_signal_reports_no_readiness() {
        let dir = scratch_repo();
        let (ctx, reg, commands) = ctx_with_reg(dir.path());
        let id = spawn_background(&commands, &ctx, "sleep 30").await;
        let out = WaitTaskTool::new(reg.clone())
            .execute(
                serde_json::json!({
                    "task_id": id,
                    "until": "match",
                    "pattern": "listening on",
                    "timeout_seconds": 1,
                }),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("readiness: none"), "{}", out.content);
        assert_eq!(out.metadata["status"], "running");
        let snap = reg.get(&id).await.expect("task");
        assert_eq!(snap.status, BackgroundTaskStatus::Running);
        assert!(
            snap.ready.is_none(),
            "silence must not become readiness evidence"
        );
    }

    /// BG-04: a long command that prints nothing is neither hung, ready, nor
    /// finished. `until: quiet` reports the silence and leaves it running.
    #[tokio::test]
    async fn wait_until_quiet_reports_silence_without_calling_it_ready() {
        let dir = scratch_repo();
        let (ctx, reg, commands) = ctx_with_reg(dir.path());
        let id = spawn_background(&commands, &ctx, "printf 'starting\\n'; sleep 30").await;
        let started = std::time::Instant::now();
        let out = WaitTaskTool::new(reg.clone())
            .execute(
                serde_json::json!({
                    "task_id": id,
                    "until": "quiet",
                    "quiet_seconds": 1,
                    "timeout_seconds": 10,
                }),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(started.elapsed() < std::time::Duration::from_secs(8));
        assert!(out.content.contains("readiness: quiet"), "{}", out.content);
        assert_eq!(out.metadata["status"], "running");
        assert_eq!(
            reg.get(&id).await.expect("task").status,
            BackgroundTaskStatus::Running
        );
    }

    /// BG-05 vs BG-01: a task that exits while printing the ready line is a
    /// FINISHED task, not a ready service. Terminal truth wins.
    #[tokio::test]
    async fn wait_until_match_reports_the_exit_when_the_task_ends() {
        let dir = scratch_repo();
        let (ctx, reg, commands) = ctx_with_reg(dir.path());
        let id = spawn_background(&commands, &ctx, "printf 'listening on :1\\n'").await;
        // The reaper publishes the exit asynchronously; wait for the terminal
        // fact instead of racing it, so this test locks precedence rather than
        // scheduling luck.
        for _ in 0..200 {
            if reg
                .get(&id)
                .await
                .is_some_and(|snap| snap.status.is_terminal())
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(
            reg.get(&id)
                .await
                .is_some_and(|snap| snap.status.is_terminal()),
            "the task must have exited before the wait starts"
        );
        let out = WaitTaskTool::new(reg.clone())
            .execute(
                serde_json::json!({
                    "task_id": id,
                    "until": "match",
                    "pattern": "listening on",
                    "timeout_seconds": 10,
                }),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("status: exited"), "{}", out.content);
        assert_eq!(out.metadata["exit_code"], 0);
        assert!(reg.get(&id).await.expect("task").ready.is_none());
    }

    /// `until: output` is for a process whose next line is the interesting
    /// fact, not for a terminal status.
    #[tokio::test]
    async fn wait_until_output_returns_on_the_next_line() {
        let dir = scratch_repo();
        let (ctx, reg, commands) = ctx_with_reg(dir.path());
        let id = spawn_background(
            &commands,
            &ctx,
            "sleep 0.3; printf 'ready-ish\\n'; sleep 30",
        )
        .await;
        let out = WaitTaskTool::new(reg.clone())
            .execute(
                serde_json::json!({
                    "task_id": id,
                    "until": "output",
                    "timeout_seconds": 10,
                }),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("ready-ish"), "{}", out.content);
        assert_eq!(out.metadata["status"], "running");
    }

    #[tokio::test]
    async fn wait_task_success_returns_the_terminal_status() {
        let dir = scratch_repo();
        let (ctx, reg, commands) = ctx_with_reg(dir.path());
        let id = spawn_background(&commands, &ctx, "printf 'ok\\n'").await;
        let out = WaitTaskTool::new(reg)
            .execute(
                serde_json::json!({"task_id": id}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("status: exited"), "{}", out.content);
        assert_eq!(out.metadata["exit_code"], 0);
    }

    #[tokio::test]
    async fn wait_task_failure_returns_the_terminal_error() {
        let dir = scratch_repo();
        let (ctx, reg, commands) = ctx_with_reg(dir.path());
        let id = spawn_background(&commands, &ctx, "exit 7").await;
        let out = WaitTaskTool::new(reg)
            .execute(
                serde_json::json!({"task_id": id}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            out.is_error,
            "a non-zero exit must be an error: {}",
            out.content
        );
        assert_eq!(out.metadata["exit_code"], 7);
    }

    #[tokio::test]
    async fn wait_task_cancel_interrupts_the_wait_without_killing_the_task() {
        let dir = scratch_repo();
        let (ctx, reg, commands) = ctx_with_reg(dir.path());
        let id = spawn_background(&commands, &ctx, "sleep 30").await;
        let cancel = CancellationToken::new();
        let waiter = WaitTaskTool::new(reg.clone());
        let wait = waiter.execute(
            serde_json::json!({"task_id": id.clone()}),
            ctx,
            cancel.clone(),
        );
        tokio::pin!(wait);
        tokio::time::sleep(Duration::from_millis(200)).await;
        cancel.cancel();
        let out = tokio::time::timeout(Duration::from_secs(5), &mut wait)
            .await
            .expect("parent cancellation must interrupt wait_task")
            .unwrap();
        assert!(out.is_error, "{}", out.content);
        assert!(out.content.contains("cancelled"), "{}", out.content);
        let snap = reg.get(&id).await.expect("task retained");
        assert_eq!(
            snap.status,
            BackgroundTaskStatus::Running,
            "wait_task cancellation must not kill the task"
        );
        let _ = reg.kill(&id).await;
    }

    #[tokio::test]
    async fn wait_task_timeout_returns_running_without_killing() {
        let dir = scratch_repo();
        let (ctx, reg, commands) = ctx_with_reg(dir.path());
        let id = spawn_background(&commands, &ctx, "sleep 30").await;
        let out = WaitTaskTool::new(reg.clone())
            .execute(
                serde_json::json!({"task_id": id.clone(), "timeout_seconds": 1}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("status: running"), "{}", out.content);
        let snap = reg.get(&id).await.expect("task retained");
        assert_eq!(snap.status, BackgroundTaskStatus::Running);
        let _ = reg.kill(&id).await;
    }

    /// AO-02/AO-04: a task that runs and says nothing hands the round back with
    /// the facts it observed, instead of sitting out the whole bound.
    ///
    /// This is the incident: `make up | tail -60` never produced a byte, the
    /// wait returned "status: running" twice, and nothing in the result said ten
    /// minutes had passed without a single new observation.
    #[tokio::test]
    async fn a_silent_task_returns_when_waiting_stops_observing_anything() {
        let dir = scratch_repo();
        let (ctx, reg, commands) = ctx_with_reg(dir.path());
        let id = spawn_background(&commands, &ctx, "sleep 60").await;
        let started = std::time::Instant::now();
        let out = WaitTaskTool::new(reg.clone())
            .with_idle_ceiling(1)
            .execute(
                serde_json::json!({"task_id": id.clone(), "timeout_seconds": 600}),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let elapsed = started.elapsed();
        assert!(!out.is_error, "{}", out.content);
        assert!(
            elapsed < std::time::Duration::from_secs(20),
            "silence must hand the round back, not spend the bound: {elapsed:?}"
        );
        for fact in [
            "status: running",
            "waited_ms:",
            "silent_ms:",
            "output_bytes: 0",
            "Output during this wait: 0 bytes",
            "NOT stopped",
        ] {
            assert!(
                out.content.contains(fact),
                "missing `{fact}` in: {}",
                out.content
            );
        }
        // A wait that ended on silence is not a failure and not a completion.
        let snap = reg.get(&id).await.expect("task retained");
        assert_eq!(snap.status, BackgroundTaskStatus::Running);
        assert_eq!(snap.expired_exit_waits, 1);
        let _ = reg.kill(&id).await;
    }

    /// The repeat is what the incident consisted of: the same call, the same
    /// result, no way to tell it apart from a first attempt except the clock.
    /// The count is a fact the second call has to be able to read.
    #[tokio::test]
    async fn a_repeated_exit_wait_reports_how_many_already_expired() {
        let dir = scratch_repo();
        let (ctx, reg, commands) = ctx_with_reg(dir.path());
        let id = spawn_background(&commands, &ctx, "sleep 60").await;
        let wait = WaitTaskTool::new(reg.clone()).with_idle_ceiling(1);
        for _ in 0..2 {
            let out = wait
                .execute(
                    serde_json::json!({"task_id": id.clone(), "timeout_seconds": 600}),
                    ctx.clone(),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            assert!(!out.is_error, "{}", out.content);
        }
        let out = wait
            .execute(
                serde_json::json!({"task_id": id.clone(), "timeout_seconds": 600}),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            out.content
                .contains("Exit waits on this task that have already expired without a result: 3"),
            "{}",
            out.content
        );
        let _ = reg.kill(&id).await;
    }

    /// The dogfood shape: the command prints its startup log and then goes
    /// quiet while it keeps running. The wait must hand the round back once the
    /// silence has lasted the idle window, not at the caller's bound.
    #[tokio::test]
    async fn output_then_silence_ends_the_wait_on_the_idle_window() {
        let dir = scratch_repo();
        let (ctx, reg, commands) = ctx_with_reg(dir.path());
        let id = spawn_background(&commands, &ctx, "echo starting; sleep 60").await;
        let started = std::time::Instant::now();
        let out = WaitTaskTool::new(reg.clone())
            .with_idle_ceiling(1)
            .execute(
                serde_json::json!({"task_id": id.clone(), "timeout_seconds": 60}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let elapsed = started.elapsed();
        assert!(!out.is_error, "{}", out.content);
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "silence must end the wait long before the bound: {elapsed:?}"
        );
        assert!(
            out.content.contains("nothing observable changed"),
            "{}",
            out.content
        );
        assert!(
            out.content.contains("starting"),
            "the output it did see is delivered: {}",
            out.content
        );
        let _ = reg.kill(&id).await;
    }

    /// The launcher exits 0 while its children keep the group alive — exactly
    /// what `make up` does. The task is still running, so the wait must still
    /// end on silence rather than the bound.
    #[tokio::test]
    async fn silence_ends_the_wait_after_the_launcher_exits_successfully() {
        let dir = scratch_repo();
        let (ctx, reg, commands) = ctx_with_reg(dir.path());
        let id = spawn_background(&commands, &ctx, "echo starting; sleep 60 & exit 0").await;
        let started = std::time::Instant::now();
        let out = WaitTaskTool::new(reg.clone())
            .with_idle_ceiling(1)
            .execute(
                serde_json::json!({"task_id": id.clone(), "timeout_seconds": 60}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "silence must end the wait long before the bound: {elapsed:?}\n{}",
            out.content
        );
        assert!(
            out.content.contains("status: running"),
            "the descendants keep the task running: {}",
            out.content
        );
        let _ = reg.kill(&id).await;
    }

    /// Continuous output followed by silence: every chunk restarts the idle
    /// clock, and the wait must still end one idle window after the last one.
    #[tokio::test]
    async fn a_burst_of_output_then_silence_still_ends_on_the_idle_window() {
        let dir = scratch_repo();
        let (ctx, reg, commands) = ctx_with_reg(dir.path());
        let id = spawn_background(
            &commands,
            &ctx,
            "for i in $(seq 1 20); do echo line-$i; sleep 0.2; done; sleep 60",
        )
        .await;
        let started = std::time::Instant::now();
        let out = WaitTaskTool::new(reg.clone())
            .with_idle_ceiling(2)
            .execute(
                serde_json::json!({"task_id": id.clone(), "timeout_seconds": 120}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(15),
            "silence after a burst must end the wait, not the bound: {elapsed:?}\n{}",
            out.content
        );
        assert!(
            out.content.contains("nothing observable changed"),
            "{}",
            out.content
        );
        let _ = reg.kill(&id).await;
    }

    /// Output received during an exit wait must remain counted after each
    /// chunk resets the silence clock.
    #[tokio::test]
    async fn idle_exit_report_counts_output_across_multiple_chunks() {
        let dir = scratch_repo();
        let (ctx, reg, commands) = ctx_with_reg(dir.path());
        let id = spawn_background(
            &commands,
            &ctx,
            "sleep 0.2; printf first; sleep 0.2; printf second; sleep 60",
        )
        .await;
        assert_eq!(reg.get(&id).await.unwrap().output_bytes, 0);
        let out = WaitTaskTool::new(reg.clone())
            .with_idle_ceiling(1)
            .execute(
                serde_json::json!({"task_id": id, "timeout_seconds": 10}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let snapshot = reg.get(&id).await.unwrap();
        let _ = reg.kill(&id).await;
        assert_eq!(snapshot.status, BackgroundTaskStatus::Running);
        assert!(out.content.contains("first"), "{}", out.content);
        assert!(out.content.contains("second"), "{}", out.content);
        assert!(
            out.content.contains(&format!(
                "Output during this wait: {} bytes",
                snapshot.output_bytes
            )),
            "{}",
            out.content
        );
    }

    /// The direct child can exit while descendants retain the process group.
    /// Both wait expiry paths must expose those two facts in model-facing text.
    #[tokio::test]
    async fn idle_exit_report_exposes_child_exit_without_settling_process_group() {
        let dir = scratch_repo();
        let (ctx, reg, commands) = ctx_with_reg(dir.path());
        let mut results = Vec::new();
        for (idle_ceiling, timeout_seconds) in [(1, 10), (10, 1)] {
            let id = spawn_background(&commands, &ctx, "sleep 60 & exit 0").await;
            let out = WaitTaskTool::new(reg.clone())
                .with_idle_ceiling(idle_ceiling)
                .execute(
                    serde_json::json!({"task_id": id, "timeout_seconds": timeout_seconds}),
                    ctx.clone(),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            let snapshot = reg.get(&id).await.unwrap();
            let _ = reg.kill(&id).await;
            assert_eq!(snapshot.status, BackgroundTaskStatus::Running);
            assert_eq!(snapshot.exit_code, Some(0));
            assert_eq!(out.metadata["exit_code"], 0);
            results.push(out);
        }
        for out in results {
            assert!(!out.is_error, "{}", out.content);
            assert!(out.content.contains("status: running"), "{}", out.content);
            assert!(
                out.content.contains("exit_code: Some(0)"),
                "the child's known result must reach the model: {}",
                out.content
            );
            assert!(out.content.contains("process group"), "{}", out.content);
            assert!(
                !out.content.contains("there is no exit code"),
                "{}",
                out.content
            );
        }
    }

    /// The production ceiling with the production bound, one time: the idle
    /// window is what ends a silent wait, never the caller's longer bound.
    /// Ignored by default because it spends three real minutes proving the
    /// constant; run it explicitly with `--ignored`.
    #[tokio::test]
    #[ignore = "spends three real minutes proving the production constant"]
    async fn the_production_idle_ceiling_ends_a_silent_exit_wait() {
        let dir = scratch_repo();
        let (ctx, reg, commands) = ctx_with_reg(dir.path());
        let id = spawn_background(
            &commands,
            &ctx,
            "for i in $(seq 1 7); do echo line-$i; sleep 2; done; sleep 300",
        )
        .await;
        let started = std::time::Instant::now();
        let out = WaitTaskTool::new(reg.clone())
            .execute(
                serde_json::json!({"task_id": id.clone(), "timeout_seconds": 300}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(WAIT_IDLE_SECS + 60),
            "the idle ceiling must end the wait, not the 300s bound: {elapsed:?}\n{}",
            out.content
        );
        assert!(
            out.content.contains("nothing observable changed"),
            "{}",
            out.content
        );
        let _ = reg.kill(&id).await;
    }

    /// Same shape as the production constants, scaled down: a 3s idle window
    /// inside a 300s bound, with output for the first 14s.
    #[tokio::test]
    async fn a_short_idle_window_ends_a_wait_with_a_long_bound() {
        let dir = scratch_repo();
        let (ctx, reg, commands) = ctx_with_reg(dir.path());
        let id = spawn_background(
            &commands,
            &ctx,
            "for i in $(seq 1 7); do echo line-$i; sleep 2; done; sleep 300",
        )
        .await;
        let started = std::time::Instant::now();
        let out = WaitTaskTool::new(reg.clone())
            .with_idle_ceiling(3)
            .execute(
                serde_json::json!({"task_id": id.clone(), "timeout_seconds": 300}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(40),
            "the idle window must end the wait, not the 300s bound: {elapsed:?}\n{}",
            out.content
        );
        assert!(
            out.content.contains("nothing observable changed"),
            "{}",
            out.content
        );
        let _ = reg.kill(&id).await;
    }

    #[test]
    fn the_production_tool_uses_the_documented_idle_window() {
        let dir = scratch_repo();
        let (_ctx, reg, _commands) = ctx_with_reg(dir.path());
        let tool = WaitTaskTool::new(reg);
        assert_eq!(tool.idle_ceiling, WAIT_IDLE_SECS);
        assert_eq!(WAIT_IDLE_SECS, 180);
    }

    /// AO-07: silence ends a wait, output does not. A command that keeps talking
    /// is still waited on for its whole bound in one call.
    #[tokio::test]
    async fn output_restarts_the_idle_window_so_a_talkative_command_is_still_waited_out() {
        let dir = scratch_repo();
        let (ctx, reg, commands) = ctx_with_reg(dir.path());
        let id = spawn_background(
            &commands,
            &ctx,
            "for i in 1 2 3 4 5 6 7 8 9 10; do echo tick-$i; sleep 0.4; done; exit 7",
        )
        .await;
        let out = WaitTaskTool::new(reg.clone())
            .with_idle_ceiling(1)
            .execute(
                serde_json::json!({"task_id": id.clone(), "timeout_seconds": 60}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        // The command ran ~4s with a 1s idle window and still reached its own
        // exit: output is progress, so the wait kept observing.
        assert!(
            out.content.contains("status: exited"),
            "output must keep the wait alive: {}",
            out.content
        );
        assert!(
            out.content.contains("exit_code: Some(7)"),
            "{}",
            out.content
        );
        assert!(out.content.contains("tick-10"), "{}", out.content);
    }

    /// A caller that asked for less than the idle ceiling keeps exactly the
    /// contract it asked for, and is told about its own bound.
    #[tokio::test]
    async fn a_short_bound_still_ends_the_wait_and_is_named_as_the_reason() {
        let dir = scratch_repo();
        let (ctx, reg, commands) = ctx_with_reg(dir.path());
        let id = spawn_background(&commands, &ctx, "sleep 60").await;
        let out = WaitTaskTool::new(reg.clone())
            .execute(
                serde_json::json!({"task_id": id.clone(), "timeout_seconds": 1}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.contains("wait bound (1s) expired"),
            "{}",
            out.content
        );
        assert!(
            !out.content.contains("nothing observable changed"),
            "the caller's own bound is the reason: {}",
            out.content
        );
        let _ = reg.kill(&id).await;
    }

    /// The lifetime and its consequence are facts an observer needs: whether
    /// completing the goal will stop this process.
    #[tokio::test]
    async fn get_task_names_the_lifetime_and_what_stops_it() {
        let dir = scratch_repo();
        let (ctx, reg, commands) = ctx_with_reg(dir.path());
        let id = spawn_background(&commands, &ctx, "sleep 30").await;
        let out = GetTaskTool::new(reg.clone())
            .execute(
                serde_json::json!({"task_id": id.clone()}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content
                .contains("lifetime: goal (stopped when the current goal finishes)"),
            "{}",
            out.content
        );
        assert!(out.content.contains("silent_ms:"), "{}", out.content);
        assert!(out.content.contains("output_bytes:"), "{}", out.content);
        let _ = reg.kill(&id).await;
    }

    /// Spawn one background shell command into the shared registry and return
    /// its task id; the wait contract tests all start the same way.
    async fn spawn_background(
        commands: &Arc<crate::tools::CommandExecution>,
        ctx: &ToolContext,
        script: &str,
    ) -> String {
        let start = RunCommandTool::new(commands.clone())
            .execute(
                serde_json::json!({"program": "sh", "args": ["-c", script], "background": true}),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!start.is_error, "spawn: {}", start.content);
        start
            .content
            .lines()
            .find_map(|line| line.strip_prefix("task_id: "))
            .expect("task id")
            .to_string()
    }

    #[tokio::test]
    async fn repeated_wait_only_returns_new_log() {
        let dir = scratch_repo();
        let (ctx, reg, commands) = ctx_with_reg(dir.path());
        let start = RunCommandTool::new(commands)
            .execute(
                serde_json::json!({
                    "program": "sh",
                    "args": ["-c", "printf 'once-only-marker\\n'; sleep 10"],
                    "background": true,
                }),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!start.is_error, "spawn: {}", start.content);
        let task_id = start
            .content
            .lines()
            .find_map(|line| line.strip_prefix("task_id: "))
            .expect("task id");
        let wait_tool = WaitTaskTool::new(reg.clone());
        let first = wait_tool
            .execute(
                serde_json::json!({"task_id": task_id, "timeout_seconds": 1}),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            first.content.contains("once-only-marker"),
            "{}",
            first.content
        );

        let second = wait_tool
            .execute(
                serde_json::json!({"task_id": task_id, "timeout_seconds": 1}),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            !second.content.contains("once-only-marker"),
            "{}",
            second.content
        );
        // A wait that saw nothing new still has to report what it observed
        // (status, silence, bytes), so it is not a single line any more — but it
        // must not replay output the caller already read.
        assert!(
            second.content.contains("status: running"),
            "{}",
            second.content
        );

        // Two independent readers may replay from their own explicit cursor
        // without consuming the shared convenience cursor.
        for _ in 0..2 {
            let independent = wait_tool
                .execute(
                    serde_json::json!({"task_id": task_id, "cursor": 0, "timeout_seconds": 1}),
                    ctx.clone(),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            assert!(
                independent.content.contains("once-only-marker"),
                "{}",
                independent.content
            );
            assert!(independent.metadata["next_cursor"].as_u64().unwrap() > 0);
        }
        let invalid = wait_tool
            .execute(
                serde_json::json!({"task_id": task_id, "cursor": u64::MAX}),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(invalid.is_error);
        assert!(invalid.content.contains("beyond"), "{}", invalid.content);

        let full = GetTaskTool::new(reg.clone())
            .execute(
                serde_json::json!({"task_id": task_id}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            full.content.contains("once-only-marker"),
            "{}",
            full.content
        );
        let _ = reg.kill(task_id).await;
    }

    #[tokio::test]
    async fn wait_keeps_large_log_for_later_calls_under_small_output_budget() {
        let dir = scratch_repo();
        let (mut ctx, reg, commands) = ctx_with_reg(dir.path());
        ctx.policy.tool_output_budget = crate::registry::MIN_TOOL_OUTPUT;
        let start = RunCommandTool::new(commands)
            .execute(
                serde_json::json!({
                    "program": "sh",
                    "args": ["-c", "printf '%020000dZ' 0"],
                    "background": true,
                }),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!start.is_error, "spawn: {}", start.content);
        let task_id = start
            .content
            .lines()
            .find_map(|line| line.strip_prefix("task_id: "))
            .expect("task id");
        let waiter = WaitTaskTool::new(reg);
        let mut zeroes = 0;
        let mut saw_end = false;
        for _ in 0..50 {
            let result = waiter
                .execute(
                    serde_json::json!({"task_id": task_id, "timeout_seconds": 1}),
                    ctx.clone(),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            assert!(!result.is_error, "{}", result.content);
            assert!(result.content.len() <= ctx.policy.tool_output_budget);
            let log = result
                .content
                .split("--- new log ---\n")
                .nth(1)
                .unwrap_or("")
                .split("\n[more log remains")
                .next()
                .unwrap();
            zeroes += log.bytes().filter(|byte| *byte == b'0').count();
            saw_end |= log.contains('Z');
            if result.metadata["log_remaining"] == false {
                break;
            }
        }
        assert_eq!(zeroes, 20_000);
        assert!(saw_end, "final byte was lost");
    }

    #[tokio::test]
    async fn failed_scoped_commands_do_not_displace_delivered_log() {
        let dir = scratch_repo();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        let (mut ctx, reg, commands) = ctx_with_reg(dir.path());
        ctx.policy.tool_output_budget = crate::registry::MIN_TOOL_OUTPUT;
        let ctx = ctx.with_command_write_constraints(Some(vec!["src".into()]), None, Vec::new());
        let start = RunCommandTool::new(commands)
            .execute(
                serde_json::json!({"program":"sh", "args":["-c", "for i in 1 2 3 4 5 6; do touch outside-$i-$(printf '%0190d' 0) 2>/dev/null; done; printf '%02000dZ' 0; exit 1"], "background":true}),
                ctx.clone(), CancellationToken::new(),
            ).await.unwrap();
        assert!(!start.is_error, "{}", start.content);
        let id = start
            .content
            .lines()
            .find_map(|l| l.strip_prefix("task_id: "))
            .unwrap();
        reg.wait(id, Some(Duration::from_secs(10)), &CancellationToken::new())
            .await
            .unwrap();
        let waiter = WaitTaskTool::new(reg);
        let mut delivered = String::new();
        for _ in 0..10 {
            let out = waiter
                .execute(
                    serde_json::json!({"task_id":id}),
                    ctx.clone(),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            assert!(out.is_error);
            assert!(
                out.content.len() <= ctx.policy.tool_output_budget,
                "settlement must not make the outer result cap discard consumed log: {}",
                out.content.len()
            );
            let log = out
                .content
                .split("--- new log ---\n")
                .nth(1)
                .unwrap_or("")
                .split("\n[")
                .next()
                .unwrap();
            delivered.push_str(log);
            if out.metadata["log_remaining"] == false {
                break;
            }
        }
        // Pipe reads split wherever the kernel gives them; each chunk carries
        // its own `[stdout] ` tag. The contract is the payload and its order,
        // not the chunk framing.
        let payload = delivered.replace("[stdout] ", "");
        assert_eq!(payload, format!("{}Z", "0".repeat(2000)));
    }

    /// The context, the ONE background registry, and the command runtime built
    /// over it. `run_command(background)` and the lifecycle tools share the
    /// registry by construction — a task started into one registry and waited
    /// on through another is exactly the orphan this ownership makes
    /// impossible.
    fn ctx_with_reg(
        dir: &std::path::Path,
    ) -> (
        ToolContext,
        Arc<BackgroundTaskRegistry>,
        Arc<crate::tools::CommandExecution>,
    ) {
        let ws = Workspace::new(dir).unwrap();
        // Library tests do not install the application's global environment
        // capability. Give both the tool context and its background registry
        // the same explicit snapshot used by the composition root; otherwise
        // confined background commands fail before spawning.
        let environment = Arc::new(leveler_core::EnvSnapshot::new(
            std::env::vars_os(),
            std::env::current_dir().unwrap_or_default(),
            std::env::temp_dir(),
        ));
        let reg = Arc::new(BackgroundTaskRegistry::with_environment(
            environment.clone(),
        ));
        let commands = Arc::new(crate::tools::CommandExecution::new(reg.clone(), None));
        let ctx = ToolContext::with_environment(ws, PermissionProfile::Assisted, environment);
        (ctx, reg, commands)
    }

    /// A background command that exits non-zero must surface as a tool ERROR
    /// carrying its exit code — never as a quiet success.
    ///
    /// Spawn Reliability Gate, Experiment 3c. The Multi-Agent shape this
    /// guards is a parent that fans out children *and* leaves a verification
    /// command running in the background: if that command's failure arrives as
    /// `ok`, the parent synthesises a conclusion on top of a check that did not
    /// pass, and reports success it has not earned.
    #[tokio::test]
    async fn a_failing_background_command_is_reported_as_an_error_with_its_exit_code() {
        let dir = scratch_repo();
        let (ctx, reg, commands) = ctx_with_reg(dir.path());

        let start = RunCommandTool::new(commands.clone())
            .execute(
                serde_json::json!({
                    "program": "sh",
                    "args": ["-c", "echo failing >&2; exit 3"],
                    "background": true,
                }),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!start.is_error, "spawn should succeed: {}", start.content);
        let task_id = start
            .content
            .lines()
            .find_map(|l| l.strip_prefix("task_id: "))
            .expect("task_id in spawn output")
            .to_string();

        // Log observation can return before exit. This test asserts the
        // terminal result, so synchronize on completion, not output arrival.
        reg.wait(
            &task_id,
            Some(Duration::from_secs(10)),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let wait = WaitTaskTool::new(reg.clone())
            .execute(
                serde_json::json!({"task_id": task_id, "timeout_seconds": 10}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert!(
            wait.is_error,
            "a non-zero background exit must not read as success: {}",
            wait.content
        );
        assert_eq!(
            wait.metadata.get("exit_code").and_then(|v| v.as_i64()),
            Some(3),
            "the exit code has to reach the caller, not just the word 'failed': {:?}",
            wait.metadata
        );
    }

    #[tokio::test]
    async fn wait_accounts_modified_files_without_restoring_when_no_allowlist() {
        // Default Goal background (dev server / watcher): account diffs, never
        // auto-restore intentional long-lived mutations (K17 / PR-3b).
        let dir = scratch_repo();
        std::fs::write(dir.path().join("keep.txt"), "original\n").unwrap();
        run(dir.path(), &["add", "-A"]);
        run(dir.path(), &["commit", "-qm", "init"]);

        let (ctx, reg, commands) = ctx_with_reg(dir.path());
        let start = RunCommandTool::new(commands.clone())
            .execute(
                serde_json::json!({
                    "program": "sh",
                    "args": ["-c", "echo new > created.txt && echo changed > keep.txt"],
                    "background": true,
                }),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!start.is_error, "spawn should succeed: {}", start.content);
        let task_id = start
            .content
            .lines()
            .find_map(|l| l.strip_prefix("task_id: "))
            .expect("task_id in spawn output")
            .to_string();

        let wait = WaitTaskTool::new(reg.clone())
            .execute(
                serde_json::json!({"task_id": task_id, "timeout_seconds": 10}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!wait.is_error, "wait should succeed: {}", wait.content);

        let modified: Vec<String> = wait
            .metadata
            .get("modified_files")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            modified.contains(&"created.txt".to_string())
                && modified.contains(&"keep.txt".to_string()),
            "wait must account background mutations: {modified:?}"
        );
        // No allowlist → files must remain (do not destroy intentional mutations).
        assert!(
            dir.path().join("created.txt").exists(),
            "without allowlist, wait must not restore away created files"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("keep.txt"))
                .unwrap()
                .trim(),
            "changed"
        );
    }

    #[tokio::test]
    async fn wait_preserves_owned_edits_and_reports_denied_foreign_write() {
        let dir = scratch_repo();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "original\n").unwrap();
        run(dir.path(), &["add", "-A"]);
        run(dir.path(), &["commit", "-qm", "init"]);

        let (ctx, reg, commands) = ctx_with_reg(dir.path());
        let constrained =
            ctx.with_command_write_constraints(Some(vec!["src".to_string()]), None, Vec::new());

        let start = RunCommandTool::new(commands.clone())
            .execute(
                serde_json::json!({
                    "program": "sh",
                    "args": ["-c", "echo ok > src/lib.rs; echo bad > outside.txt"],
                    "background": true,
                }),
                constrained.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!start.is_error, "spawn should succeed: {}", start.content);
        let task_id = start
            .content
            .lines()
            .find_map(|l| l.strip_prefix("task_id: "))
            .expect("task_id")
            .to_string();

        let wait = WaitTaskTool::new(reg.clone())
            .execute(
                serde_json::json!({"task_id": task_id, "timeout_seconds": 10}),
                constrained.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert!(wait.is_error, "allowlist violation must fail wait");
        assert!(
            wait.content.contains("Operation not permitted")
                || wait.content.contains("Permission denied")
                || wait.content.contains("Read-only file system"),
            "expected scope message: {}",
            wait.content
        );
        assert!(
            !dir.path().join("outside.txt").exists(),
            "out-of-scope create must never occur"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("src/lib.rs"))
                .unwrap()
                .trim(),
            "ok",
            "owned edits must survive a later denied write"
        );
        let modified = wait
            .metadata
            .get("modified_files")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        assert!(
            modified == vec![serde_json::json!("src/lib.rs")],
            "settlement contains only owned writes: {modified:?}"
        );
        let reread = WaitTaskTool::new(reg)
            .execute(
                serde_json::json!({"task_id": task_id, "cursor": 0}),
                constrained,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(reread.is_error, "reading must not consume terminal failure");
        assert_ne!(reread.metadata["exit_code"], serde_json::json!(0));
        assert!(reread.metadata["workspace_snapshot"].is_null());
    }

    #[tokio::test]
    async fn wait_keeps_in_allowlist_mutations() {
        let dir = scratch_repo();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "original\n").unwrap();
        run(dir.path(), &["add", "-A"]);
        run(dir.path(), &["commit", "-qm", "init"]);

        let (ctx, reg, commands) = ctx_with_reg(dir.path());
        let constrained =
            ctx.with_command_write_constraints(Some(vec!["src".to_string()]), None, Vec::new());

        let start = RunCommandTool::new(commands.clone())
            .execute(
                serde_json::json!({
                    "program": "sh",
                    "args": ["-c", "echo patched > src/lib.rs"],
                    "background": true,
                }),
                constrained.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let task_id = start
            .content
            .lines()
            .find_map(|l| l.strip_prefix("task_id: "))
            .expect("task_id")
            .to_string();

        let wait = WaitTaskTool::new(reg.clone())
            .execute(
                serde_json::json!({"task_id": task_id, "timeout_seconds": 10}),
                constrained,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            !wait.is_error,
            "in-scope edit should pass: {}",
            wait.content
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("src/lib.rs"))
                .unwrap()
                .trim(),
            "patched"
        );
        let modified: Vec<String> = wait
            .metadata
            .get("modified_files")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(modified, vec!["src/lib.rs".to_string()]);
    }

    #[tokio::test]
    async fn execution_boundary_refuses_foreign_owned_file_before_mutation() {
        let dir = scratch_repo();
        std::fs::create_dir_all(dir.path().join("owned")).unwrap();
        std::fs::write(dir.path().join("foreign.txt"), "unchanged").unwrap();
        let (ctx, _, commands) = ctx_with_reg(dir.path());
        let ctx = ctx
            .with_command_write_constraints(Some(vec!["owned".into()]), None, Vec::new())
            .with_foreign_owned_paths(vec!["foreign.txt".into()]);
        let out = commands
            .run_foreground(
                "sh",
                vec!["-c".into(), "printf corrupted > foreign.txt".into()],
                None,
                Some(5),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            out.is_error,
            "unauthorized write must fail: {}",
            out.content
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("foreign.txt")).unwrap(),
            "unchanged"
        );
    }

    #[tokio::test]
    async fn task_tools_reject_another_session_and_allow_the_creator() {
        let dir = scratch_repo();
        let (ctx, reg, commands) = ctx_with_reg(dir.path());
        let owner = ctx.clone().with_session_scope("owner");
        let stranger = ctx.with_session_scope("stranger");
        let id = spawn_background(&commands, &owner, "sleep 30").await;
        let input = serde_json::json!({"task_id": id});
        let get = GetTaskTool::new(reg.clone())
            .execute(input.clone(), stranger.clone(), CancellationToken::new())
            .await
            .unwrap();
        let kill = KillTaskTool::new(reg.clone())
            .execute(input.clone(), stranger, CancellationToken::new())
            .await
            .unwrap();
        let still_running = reg.get(&id).await.unwrap().status;
        let _ = reg.kill(&id).await;
        assert!(get.is_error, "foreign task log must not be disclosed");
        assert!(kill.is_error, "foreign task must not be signalled");
        assert_eq!(still_running, BackgroundTaskStatus::Running);
        let own = GetTaskTool::new(reg)
            .execute(input, owner, CancellationToken::new())
            .await
            .unwrap();
        assert!(!own.is_error, "the same session may operate across turns");
    }

    #[tokio::test]
    async fn parent_excludes_foreign_paths_and_symlink_aliases_before_write() {
        let dir = scratch_repo();
        std::fs::create_dir_all(dir.path().join("owned")).unwrap();
        std::fs::create_dir_all(dir.path().join("foreign")).unwrap();
        std::fs::write(dir.path().join("foreign/keep"), "original").unwrap();
        std::os::unix::fs::symlink("../foreign", dir.path().join("owned/alias")).unwrap();
        let (ctx, _, commands) = ctx_with_reg(dir.path());
        let ctx = ctx.with_foreign_owned_paths(vec!["foreign".into()]);
        for path in ["foreign/keep", "owned/alias/keep"] {
            let out = commands
                .run_foreground(
                    "sh",
                    vec!["-c".into(), format!("printf changed > {path}")],
                    None,
                    Some(5),
                    ctx.clone(),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            assert!(out.is_error, "{path}: {}", out.content);
            assert_eq!(
                std::fs::read_to_string(dir.path().join("foreign/keep")).unwrap(),
                "original"
            );
        }
        let out = commands
            .run_foreground(
                "sh",
                vec!["-c".into(), "printf own > owned/new".into()],
                None,
                Some(5),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            out.metadata["modified_files"],
            serde_json::json!(["owned/new"])
        );
        assert!(
            out.metadata["workspace_snapshot"].is_null(),
            "a scoped accounting tree must never be exposed as a full rollback snapshot"
        );
    }

    #[tokio::test]
    async fn zero_write_scope_keeps_background_read_capability() {
        let dir = scratch_repo();
        let (ctx, reg, commands) = ctx_with_reg(dir.path());
        let ctx = ctx.with_command_write_constraints(Some(Vec::new()), None, Vec::new());
        let id = spawn_background(&commands, &ctx, "printf observed").await;
        let snap = reg
            .wait(&id, Some(Duration::from_secs(5)), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(snap.exit_code, Some(0));
        assert!(snap.log.contains("observed"));
    }

    #[tokio::test]
    async fn scoped_execution_rejects_hardlinks_to_other_owners() {
        let dir = scratch_repo();
        std::fs::create_dir(dir.path().join("owned")).unwrap();
        std::fs::write(dir.path().join("foreign"), "original").unwrap();
        std::fs::hard_link(dir.path().join("foreign"), dir.path().join("owned/alias")).unwrap();
        let (ctx, _, commands) = ctx_with_reg(dir.path());
        let ctx = ctx.with_command_write_constraints(Some(vec!["owned".into()]), None, Vec::new());
        let out = commands
            .run_foreground(
                "sh",
                vec!["-c".into(), "printf changed > owned/alias".into()],
                None,
                Some(5),
                ctx,
                CancellationToken::new(),
            )
            .await;
        assert_eq!(
            std::fs::read_to_string(dir.path().join("foreign")).unwrap(),
            "original",
            "an allowed path must not alias a foreign inode: {out:?}"
        );
        assert!(out.is_err() || out.unwrap().is_error);
    }

    #[tokio::test]
    async fn scoped_command_cannot_create_a_foreign_hardlink_then_write_it() {
        let dir = scratch_repo();
        std::fs::create_dir(dir.path().join("owned")).unwrap();
        std::fs::write(dir.path().join("foreign"), "original").unwrap();
        let (ctx, _, commands) = ctx_with_reg(dir.path());
        let ctx = ctx.with_command_write_constraints(Some(vec!["owned".into()]), None, Vec::new());
        let out = commands
            .run_foreground(
                "sh",
                vec![
                    "-c".into(),
                    "ln foreign owned/alias && printf changed > owned/alias".into(),
                ],
                None,
                Some(5),
                ctx,
                CancellationToken::new(),
            )
            .await
            .expect("the process must actually run under the sandbox");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("foreign")).unwrap(),
            "original",
            "runtime link creation must not acquire foreign write authority: {out:?}"
        );
        assert!(out.is_error, "the OS must reject the hardlink: {out:?}");
        assert_eq!(
            out.metadata["execution_status"], "completed",
            "spawn failure is not evidence of a denied link"
        );
        assert_ne!(out.metadata["exit_code"], 0);
        assert!(
            !dir.path().join("owned/alias").exists(),
            "the alias must never be created"
        );
        #[cfg(target_os = "macos")]
        assert!(
            out.content.contains("ln:") && out.content.contains("Operation not permitted"),
            "expected Seatbelt to deny link creation: {out:?}"
        );
    }

    #[tokio::test]
    async fn hardlinks_entirely_within_owned_scope_remain_writable() {
        let dir = scratch_repo();
        std::fs::create_dir(dir.path().join("owned")).unwrap();
        std::fs::write(dir.path().join("owned/source"), "original").unwrap();
        std::fs::hard_link(
            dir.path().join("owned/source"),
            dir.path().join("owned/alias"),
        )
        .unwrap();
        let (ctx, _, commands) = ctx_with_reg(dir.path());
        let ctx = ctx.with_command_write_constraints(Some(vec!["owned".into()]), None, Vec::new());
        let out = commands
            .run_foreground(
                "sh",
                vec!["-c".into(), "printf changed > owned/alias".into()],
                None,
                Some(5),
                ctx,
                CancellationToken::new(),
            )
            .await;
        assert!(out.is_ok(), "all inode names are owned: {out:?}");
        assert!(!out.unwrap().is_error);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("owned/source")).unwrap(),
            "changed"
        );
    }
    #[tokio::test]
    async fn foreign_wait_cannot_consume_the_owners_log_or_settlement() {
        let dir = scratch_repo();
        let (ctx, reg, commands) = ctx_with_reg(dir.path());
        let owner = ctx.clone().with_session_scope("owner");
        let id = spawn_background(
            &commands,
            &owner,
            "printf private-log; printf changed > own.txt",
        )
        .await;
        reg.wait(&id, Some(Duration::from_secs(5)), &CancellationToken::new())
            .await
            .unwrap();
        let waiter = WaitTaskTool::new(reg);
        let args = serde_json::json!({"task_id":id});
        let foreign = waiter
            .execute(
                args.clone(),
                ctx.with_session_scope("foreign"),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(foreign.is_error);
        assert!(!foreign.content.contains("private-log"));
        let own = waiter
            .execute(args, owner, CancellationToken::new())
            .await
            .unwrap();
        assert!(own.content.contains("private-log"));
        assert_eq!(
            own.metadata["modified_files"],
            serde_json::json!(["own.txt"])
        );
    }
}
