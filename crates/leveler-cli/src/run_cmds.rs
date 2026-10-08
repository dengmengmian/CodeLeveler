//! Run-style subcommands: run, parallel run,
//! tui, and resume, plus their shared finish/ship helpers.

use std::net::SocketAddr;

// Both are used only by the Unix-gated test helper below, so they are not
// imported on Windows, where `-D unused-imports` would reject them.
#[cfg(all(test, unix))]
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
#[cfg(all(test, unix))]
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use leveler_agent::StopReason;
use leveler_app::{Application, CollaborationExecution, InProcessRuntimeClient};
use leveler_client_protocol::InteractiveRuntimeClient;
use leveler_local_transport::{CreateSessionRequest, LocalSocketRuntimeClient};
use leveler_project::Layout;
#[cfg(test)]
use leveler_runtime_host::{
    generate_daemon_token, probe_default_runtime as connect_default_runtime,
};
// Every user of the rest is a Unix-gated test, so they are not imported on
// Windows, where `-D unused-imports` would reject them.
#[cfg(all(test, unix))]
use leveler_runtime_host::{
    RuntimeConsistency, bind_daemon_transports, classify_runtime, classify_runtime_generation,
    force_handover_allowed, handoff_key, stalled_turn_sessions, verify_replacement,
};

use crate::cli::{OutputFormat, RunMode};
use crate::common::{
    build_approver, map_mode, project_default_mode, resolve_mode, resolve_model,
    spawn_interrupt_handler, wire_mode,
};
use crate::output::Line;
use crate::render::{emit_jsonl, render_event};

#[allow(clippy::too_many_arguments)]
pub(crate) async fn cmd_run(
    layout: Layout,
    task: String,
    model: Option<String>,
    mode: Option<RunMode>,
    auto_approve: bool,
    output: OutputFormat,
    ship: leveler_app::ShipOptions,
    sandbox: bool,
    collaboration: leveler_lifecycle::CollaborationMode,
    max_model_steps: Option<u32>,
) -> anyhow::Result<std::process::ExitCode> {
    let mut app = Application::assemble(layout)?
        .with_collaboration(collaboration)
        .with_model_step_ceiling(max_model_steps);
    let execution = CollaborationExecution::of(collaboration);
    if let Some(overrides) = eval_env_overrides()? {
        app = app.with_execution_overrides(overrides);
    }
    let model_ref = resolve_model(&app, model)?;
    let execution_mode = resolve_mode(mode, project_default_mode(&app.layout));

    let session_id = app
        .create_session_with_mode(&model_ref, &task, execution_mode)
        .await?;

    if output == OutputFormat::Text {
        println!(
            "{}",
            Line::heading(&format!("Running task with {model_ref}"))
        );
        println!("  session: {session_id}");
        println!("  mode: {execution_mode:?}");
        println!("  task: {task}\n");
    } else {
        emit_jsonl(serde_json::json!({
            "type": "session_started",
            "session_id": session_id.to_string(),
            "model": model_ref.to_string(),
        }));
    }

    let approver = build_approver(auto_approve);
    let cancellation = CancellationToken::new();
    spawn_interrupt_handler(cancellation.clone());

    let result = app
        .run_in_session(
            &session_id,
            &model_ref,
            execution_mode,
            &task,
            approver,
            sandbox,
            &mut |e| render_event(e, output),
            cancellation,
        )
        .await;

    // Shipping eligibility is not evidence that formatting, builds or tests passed.
    if ship.any()
        && output == OutputFormat::Text
        && let Ok(outcome) = &result
        && outcome.stop_reason == StopReason::Completed
        && !outcome.modified_files.is_empty()
    {
        ship_changes_and_print(&app, &model_ref, &task, &outcome.modified_files, &ship).await;
    }

    finish(result, &session_id.to_string(), output, execution)
}
pub(crate) async fn cmd_run_parallel(
    layout: Layout,
    task: String,
    model: Option<String>,
    mode: Option<RunMode>,
    parallel: usize,
    collaboration: leveler_lifecycle::CollaborationMode,
) -> anyhow::Result<std::process::ExitCode> {
    // The explicit CLI axis wins over the product default here exactly as it
    // does on the single-agent path: `parallel_edit` reads this Application's
    // resolved collaboration for both the parent session and every child.
    let app = Application::assemble(layout)?.with_collaboration(collaboration);
    let model_ref = resolve_model(&app, model)?;
    let execution_mode = resolve_mode(mode, project_default_mode(&app.layout));

    println!(
        "{}",
        Line::heading(&format!(
            "Parallel edit: {parallel} agents with {model_ref}"
        ))
    );
    println!("  mode: {execution_mode:?}");
    println!("  task: {task}\n");
    println!(
        "{}",
        Line::warn("Running agents concurrently in isolated worktrees…")
    );

    let cancellation = CancellationToken::new();
    spawn_interrupt_handler(cancellation.clone());

    let outcome = app
        .parallel_edit(&model_ref, execution_mode, &task, parallel, cancellation)
        .await?;

    println!();
    println!("{}", Line::heading("Parallel result"));
    println!(
        "  {} candidate(s), {} verified",
        outcome.candidates, outcome.verified
    );
    println!("  session: {}", outcome.session);
    if !outcome.integrated.is_empty() {
        println!(
            "  {} integrated: {}",
            console::style("✓").green(),
            outcome.integrated.join(", ")
        );
    }
    if !outcome.conflicted.is_empty() {
        println!(
            "  {} skipped (conflicted with integrated edits): {}",
            console::style("!").yellow(),
            outcome.conflicted.join(", ")
        );
    }
    if outcome.integrated.is_empty() {
        println!(
            "{}",
            Line::warn("No candidate produced integrable changes.")
        );
        Ok(std::process::ExitCode::FAILURE)
    } else {
        println!("{}", Line::ok("Integrated into the current branch."));
        Ok(std::process::ExitCode::SUCCESS)
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SocketIntent {
    Embedded,
    ProbeDefault,
    RequireExplicit,
}

fn socket_intent(
    in_process: bool,
    // R1: `--auto-approve` no longer forces an embedded runtime. It is carried as
    // a per-session approval policy on the (trusted-local) CreateSessionRequest,
    // so an unattended goal runs in the daemon and survives client disconnect.
    _auto_approve: bool,
    explicit_socket: bool,
    config_overridden: bool,
) -> SocketIntent {
    if in_process {
        SocketIntent::Embedded
    } else if explicit_socket {
        SocketIntent::RequireExplicit
    } else if config_overridden {
        // A running daemon cannot inherit this invocation-scoped config.
        SocketIntent::Embedded
    } else {
        SocketIntent::ProbeDefault
    }
}

/// The lifecycle line a retiring runtime reports: exactly the two things the
/// drain waits for, so a user can see WHY it has not exited yet.
///
/// When the drain is blocked by live background work, the real tasks are named
/// (id, command, age) instead of a bare count — a bare count tells the user
/// nothing about what to do next. The list comes from the runtime's own
/// `BackgroundTaskRegistry` via `RuntimeInfo.health.blockers`, so it is the
/// same set `alive_count` counts, never a `ps` guess.
#[cfg(any(unix, windows))]
#[derive(Clone, Copy, PartialEq, Eq)]
enum HandoffLang {
    Zh,
    En,
}

#[cfg(any(unix, windows))]
impl HandoffLang {
    /// The UI language, resolved the same way every other surface resolves it.
    fn current() -> Self {
        match leveler_tui::Locale::resolve(None) {
            leveler_tui::Locale::En => Self::En,
            leveler_tui::Locale::Zh => Self::Zh,
        }
    }

    fn pick(self, zh: &'static str, en: &'static str) -> &'static str {
        match self {
            Self::Zh => zh,
            Self::En => en,
        }
    }
}

/// The lifecycle line a retiring runtime reports, in the user's language.
///
/// The user thinks in *versions*, never in "runtimes": this text says "the
/// previous CodeLeveler version" and never exposes `quiescent`, a fingerprint
/// or a pgid. The counts are the runtime's own.
#[cfg(any(unix, windows))]
fn retiring_status_lang(
    health: &leveler_client_protocol::RuntimeHealth,
    lang: HandoffLang,
) -> String {
    let phase = if health.quiescent() {
        lang.pick("正在退出", "exiting")
    } else {
        lang.pick("正在完成现有工作", "finishing existing work")
    };
    let mut text = format!(
        "{} {} · {} {} · {phase}",
        lang.pick("当前轮次", "turns"),
        health.active_turns,
        lang.pick("后台任务", "background tasks"),
        health.active_background_tasks,
    );
    if health.quiescent() || (health.turn_blockers.is_empty() && health.blockers.is_empty()) {
        return text;
    }
    // Main turns first: they are what a stale handover is usually stuck on.
    if !health.turn_blockers.is_empty() {
        text.push_str(&format!(
            "\n\n  {}",
            lang.pick(
                "旧版本 CodeLeveler 仍有任务正在执行：",
                "The previous CodeLeveler version still has running tasks:",
            )
        ));
        for blocker in &health.turn_blockers {
            text.push_str(&format!(
                "\n    {} {}",
                lang.pick("会话", "session"),
                blocker.session_id.as_str(),
            ));
            text.push_str(&format!(
                "\n      {} {} · {} {}{} · {}",
                lang.pick("已运行", "elapsed"),
                format_task_age(blocker.elapsed_ms),
                lang.pick("最后活动", "last activity"),
                format_task_age(blocker.idle_ms),
                lang.pick("前", " ago"),
                turn_status_label(blocker, lang),
            ));
        }
    }
    if !health.blockers.is_empty() {
        text.push_str(&format!(
            "\n\n  {}",
            lang.pick(
                "旧版本 CodeLeveler 仍有后台任务在运行：",
                "The previous CodeLeveler version still has background tasks:",
            )
        ));
        for blocker in &health.blockers {
            text.push_str(&format!(
                "\n    {}  {}  {}",
                blocker.task_id,
                blocker_command_line(&blocker.program, &blocker.args),
                format_task_age(blocker.elapsed_ms),
            ));
        }
        text.push_str(&format!(
            "\n\n  {} leveler background logs <task_id>",
            lang.pick("查看：", "Inspect:")
        ));
        text.push_str(&format!(
            "\n  {}    leveler background stop <task_id>",
            lang.pick("停止：", "Stop:   ")
        ));
    }
    text
}

/// Describe a turn's progress for the handover line. "no observable progress"
/// is a HINT computed from idle time alone — never "dead", never a verdict, and
/// it never triggers an action by itself.
#[cfg(any(unix, windows))]
fn turn_status_label(
    blocker: &leveler_client_protocol::UiTurnBlocker,
    lang: HandoffLang,
) -> &'static str {
    if blocker.suspected_stalled_after(leveler_client_protocol::STALE_TURN_WARN_AFTER) {
        lang.pick("长时间没有可观测进展", "no observable progress")
    } else {
        lang.pick("正在执行", "running")
    }
}

/// `program args…` as one line, so a blocker reads as the command it is.
#[cfg(any(unix, windows))]
pub(crate) fn blocker_command_line(program: &str, args: &[String]) -> String {
    let mut line = program.to_string();
    for arg in args {
        line.push(' ');
        line.push_str(arg);
    }
    line
}

/// A compact age for the blocker line (`1d 01h`, `18m`, `4s`).
#[cfg(any(unix, windows))]
pub(crate) fn format_task_age(elapsed_ms: u64) -> String {
    let secs = elapsed_ms / 1000;
    let (days, hours, minutes, seconds) = (
        secs / 86_400,
        (secs % 86_400) / 3_600,
        (secs % 3_600) / 60,
        secs % 60,
    );
    if days > 0 {
        format!("{days}d {hours:02}h")
    } else if hours > 0 {
        format!("{hours}h {minutes:02}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

/// What the user asked the handover to do, read from the terminal.
#[cfg(any(unix, windows))]
type HandoverInput = leveler_runtime_host::HandoffAction;

/// Decide what a raw terminal line means. Anything unrecognized is ignored,
/// never guessed: a stray keystroke must not interrupt or force anything.
#[cfg(any(unix, windows))]
fn parse_handover_input(line: &str) -> Option<HandoverInput> {
    match line.trim().to_ascii_lowercase().as_str() {
        "i" | "interrupt" => Some(HandoverInput::Interrupt),
        "f" | "force" => Some(HandoverInput::Force),
        _ => None,
    }
}

/// Reads handover commands from the terminal, when there is one. In a
/// non-interactive process (piped stdin, CI, a test) this returns `None`, so
/// the wait never blocks on input and never forces.
#[cfg(any(unix, windows))]
fn spawn_handover_input() -> Option<tokio::sync::mpsc::UnboundedReceiver<HandoverInput>> {
    use std::io::IsTerminal;

    if !std::io::stdin().is_terminal() {
        return None;
    }
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        use std::io::BufRead;
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            if let Some(action) = parse_handover_input(&line)
                && tx.send(action).is_err()
            {
                break;
            }
        }
    });
    Some(rx)
}

/// Render lifecycle observations in the current terminal language. The host
/// owns the handoff state machine; this shell owns user-visible text and input.
#[cfg(any(unix, windows))]
struct TuiHandoffUi;

#[cfg(any(unix, windows))]
impl leveler_runtime_host::HandoffUi for TuiHandoffUi {
    fn emit(&self, event: leveler_runtime_host::HandoffEvent) {
        use leveler_runtime_host::HandoffEvent;
        let lang = HandoffLang::current();
        match event {
            HandoffEvent::Status(health) => {
                let status = retiring_status_lang(&health, lang);
                let stalled = leveler_runtime_host::stalled_turn_sessions(&health).len();
                let mut message = format!(
                    "{}\n  {status}\n  {}",
                    lang.pick("版本更新等待中", "Version update pending"),
                    lang.pick(
                        "现有任务不会被自动中断，完成后将自动切换到当前版本。",
                        "Existing work will not be interrupted; the current version will take over automatically.",
                    ),
                );
                if stalled > 0 {
                    message.push_str(&format!(
                        "\n  {}",
                        lang.pick(
                            "检测到长时间无进展的任务：输入 i 尝试中断，或继续等待。",
                            "A task shows no progress: type i to interrupt it, or keep waiting.",
                        )
                    ));
                }
                eprintln!("{message}");
            }
            HandoffEvent::HandshakeUnavailable => {
                let status = lang.pick(
                    "旧版本 CodeLeveler 未响应握手，暂时无法读取其状态。",
                    "The previous CodeLeveler version does not answer the handshake; its state cannot be read yet.",
                );
                eprintln!(
                    "{}\n  {status}\n  {}",
                    lang.pick("版本更新等待中", "Version update pending"),
                    lang.pick(
                        "现有任务不会被自动中断，完成后将自动切换到当前版本。",
                        "Existing work will not be interrupted; the current version will take over automatically.",
                    )
                );
            }
            HandoffEvent::ForceAvailable => eprintln!(
                "  {}",
                lang.pick(
                    "中断未生效：输入 f 强制结束旧版本并继续切换。",
                    "The interrupt did not settle: type f to force-end the previous version and continue.",
                )
            ),
            HandoffEvent::StateUnavailable => eprintln!(
                "  {}",
                lang.pick(
                    "暂时读不到旧版本状态，无法执行该操作。",
                    "The previous version's state cannot be read yet; that action is unavailable.",
                )
            ),
            HandoffEvent::NoStalledTurns => eprintln!(
                "  {}",
                lang.pick(
                    "当前没有检测到长时间无进展的任务。",
                    "No task is currently showing a lack of progress.",
                )
            ),
            HandoffEvent::InterruptRequested => eprintln!(
                "  {}",
                lang.pick(
                    "已请求中断，等待旧版本正常收尾…",
                    "Interrupt requested; waiting for the previous version to settle…",
                )
            ),
            HandoffEvent::InterruptDeliveryFailed => eprintln!(
                "  {}",
                lang.pick(
                    "中断请求未送达旧版本。",
                    "The interrupt request could not reach the previous version.",
                )
            ),
            HandoffEvent::StillWaiting {
                waited_secs,
                active_turns,
                active_background_tasks,
                can_force,
            } => {
                let hint = if can_force {
                    lang.pick(
                        "输入 i 尝试中断，或输入 f 强制切换。",
                        "Type i to interrupt it, or f to force the switch.",
                    )
                } else {
                    lang.pick(
                        "后台任务不能被强制切换终止；请按上面的提示停止它们，或继续等待。",
                        "Background tasks cannot be force-ended; stop them as shown above, or keep waiting.",
                    )
                };
                eprintln!(
                    "{}\n  {}\n  {hint}",
                    lang.pick("版本更新等待中", "Version update pending"),
                    lang.pick(
                        "旧版本仍在执行任务，本次等待已持续",
                        "The previous version is still working; this wait has lasted",
                    )
                    .to_string()
                        + &format!(
                            " {} ({} {} · {} {})",
                            format_task_age(waited_secs * 1_000),
                            lang.pick("当前轮次", "turns"),
                            active_turns,
                            lang.pick("后台任务", "background tasks"),
                            active_background_tasks,
                        ),
                );
            }
            HandoffEvent::UpgradeDeferred {
                active_turns,
                active_background_tasks,
            } => eprintln!(
                "{}\n  {}\n  {} ({} {} · {} {})",
                lang.pick("版本更新推迟", "Version update deferred"),
                lang.pick(
                    "旧版本仍在执行任务，因此本次升级没有让它停止接受新工作；旧运行时继续正常服务。",
                    "The previous version is still working, so this upgrade did not stop it from accepting new work; it keeps serving normally.",
                ),
                lang.pick("当前欠工作：", "work still owed:"),
                lang.pick("当前轮次", "turns"),
                active_turns,
                lang.pick("后台任务", "background tasks"),
                active_background_tasks,
            ),
            HandoffEvent::ForceBlocked => eprintln!(
                "  {}",
                lang.pick(
                    "仍有后台任务在运行，不能强制切换；请先按上面的提示停止它们。",
                    "Background tasks are still running; force handover is unavailable. Stop them first, as shown above.",
                )
            ),
            HandoffEvent::ForceRequested => eprintln!(
                "  {}",
                lang.pick(
                    "已强制结束旧版本，正在切换…",
                    "Force-ending the previous version; switching…",
                )
            ),
            HandoffEvent::ForceFailed(error) => eprintln!(
                "  {}{error}",
                lang.pick("强制切换失败：", "Force handover failed: ")
            ),
            HandoffEvent::MigrationStarted { pid, version } => eprintln!(
                "{}\n  {} (pid {pid}, {version})",
                lang.pick("正在替换无原子退役能力的旧版本", "Replacing a previous version with no atomic retirement"),
                lang.pick(
                    "已核实该进程就是当前端点的运行时，将先请求其正常退出。",
                    "The process was verified as this endpoint's runtime; it will be asked to exit first.",
                ),
            ),
            HandoffEvent::MigrationTerminating { pid, force } => {
                let detail = if force {
                    lang.pick(
                        "正常退出请求未被响应，已发送强制结束信号。",
                        "The graceful exit request went unanswered; the forced signal was sent.",
                    )
                } else {
                    lang.pick("已发送正常退出请求。", "Sent the graceful exit request.")
                };
                eprintln!("  {detail} (pid {pid})");
            }
            HandoffEvent::MigrationTerminated { pid } => eprintln!(
                "  {} (pid {pid})",
                lang.pick(
                    "旧版本已退出并释放端点，开始启动当前版本。",
                    "The previous version exited and released the endpoint; starting the current version.",
                )
            ),
            HandoffEvent::MigrationFailed { reason } => eprintln!(
                "{}\n  {reason}\n  {}",
                lang.pick(
                    "无法安全替换旧版本，已放弃自动升级（未终止任何进程）",
                    "The previous version cannot be safely replaced; automatic upgrade abandoned (nothing was terminated)",
                ),
                lang.pick(
                    "请手动退出旧版本 CodeLeveler 后重新启动；不会盲目终止进程。",
                    "Exit the previous CodeLeveler version manually and start again; no process is terminated on a guess.",
                ),
            ),
        }
    }

    fn input(
        &self,
    ) -> Option<tokio::sync::mpsc::UnboundedReceiver<leveler_runtime_host::HandoffAction>> {
        spawn_handover_input()
    }

    /// A human at this terminal can answer `i` / `f`. Reports the same fact
    /// `input` would act on, without acquiring stdin: the reader thread must
    /// only start when the host is actually waiting on a stalled turn.
    fn interactive(&self) -> bool {
        use std::io::IsTerminal;
        std::io::stdin().is_terminal()
    }

    fn render_ensure_error(&self, error: &leveler_runtime_host::EnsureError) -> String {
        format_ensure_error(error)
    }
}

#[cfg(any(unix, windows))]
fn daemon_launch() -> anyhow::Result<leveler_runtime_host::DetachedRuntimeLaunch> {
    Ok(leveler_runtime_host::DetachedRuntimeLaunch {
        executable: std::env::current_exe()?,
        ready_prefix: "leveler-tui-ready".to_string(),
    })
}

#[cfg(any(unix, windows))]
fn format_ensure_error(host: &leveler_runtime_host::EnsureError) -> String {
    use leveler_runtime_host::EnsureError;
    match host {
        EnsureError::UnknownGeneration => {
            let lang = HandoffLang::current();
            format!(
                    "{}\n  {}\n{}",
                    lang.pick("发现旧版本 CodeLeveler", "Previous CodeLeveler version detected"),
                    lang.pick(
                        "当前项目仍连接到不支持自动版本切换的早期版本。",
                        "This project is still connected to an earlier version that cannot complete an automatic version switch.",
                    ),
                    lang.pick(
                        "该版本无法自动完成版本切换；请退出后重新启动 CodeLeveler 以切换到当前版本。",
                        "This version cannot complete an automatic version switch. Exit and start CodeLeveler again to switch to the current version.",
                    ),
                )
        }
        EnsureError::RetireBlocked {
            waited_secs,
            active_turns,
            active_background_tasks,
        } => {
            let lang = HandoffLang::current();
            let waited = format_task_age(waited_secs.saturating_mul(1_000));
            format!(
                "{}\n  {} {waited} ({} {active_turns} · {} {active_background_tasks})\n  {}\n  {}",
                lang.pick(
                    "旧版本 CodeLeveler 仍在执行任务，本次启动无法自动接管运行时。",
                    "The previous CodeLeveler version is still working, so this launch cannot take over the runtime.",
                ),
                lang.pick("已等待", "waited"),
                lang.pick("当前轮次", "turns"),
                lang.pick("后台任务", "background tasks"),
                lang.pick(
                    "旧版本未被打断、未被终止，任务不会丢失；上面的状态里列了每个阻塞任务和停止命令。",
                    "The previous version was neither interrupted nor terminated, so no work is lost; the status above names each blocking task and its stop command.",
                ),
                lang.pick(
                    "处理完后重新启动 CodeLeveler 即可切换到当前版本；如需一直等待，设置 LEVELER_HANDOVER_DRAIN_TIMEOUT_SECS=0。",
                    "Start CodeLeveler again afterwards to switch to this version; set LEVELER_HANDOVER_DRAIN_TIMEOUT_SECS=0 to wait forever instead.",
                ),
            )
        }
        EnsureError::LegacyMigrationRefused { reason, .. } => {
            let lang = HandoffLang::current();
            format!(
                "{}\n  {reason}\n  {}",
                lang.pick(
                    "旧版本无法自动替换，且不能安全强制迁移。",
                    "The previous version cannot be replaced automatically, and cannot be force-migrated safely.",
                ),
                lang.pick(
                    "未终止任何进程，也未取消任何任务；请手动退出旧版本后重新启动。",
                    "No process was terminated and no work was cancelled; exit the previous version manually and start again.",
                ),
            )
        }
        EnsureError::ReadyTimeout {
            seconds,
            log_path,
            observation,
        } => {
            let lang = HandoffLang::current();
            let state = match observation {
                leveler_runtime_host::StartupObservation::Starting => lang.pick(
                    "旧进程还没完成启动（未公布就绪，进程仍在运行，本客户端不会把它杀掉）。",
                    "The previous process has not finished starting (no readiness published; it is still running and this client will not kill it).",
                ),
                leveler_runtime_host::StartupObservation::Unresponsive => lang.pick(
                    "旧进程已公布就绪但对连接无响应；这不能证明它已经退出。",
                    "The previous process published readiness but does not answer; that is not proof it has exited.",
                ),
                leveler_runtime_host::StartupObservation::ChildExited => lang.pick(
                    "启动的进程已退出且未公布就绪。",
                    "The process that was launched exited without publishing readiness.",
                ),
            };
            format!(
                "the local runtime did not become ready within {seconds}s\n  {state}\n  log: {}\n  retry, read the log, or run `leveler tui --in-process` as a fallback",
                log_path.display()
            )
        }
        EnsureError::StartupFailed { log_path, tail } => format!(
            "the local runtime failed to start (log: {}):\n{}\nrun `leveler tui --in-process` as a fallback",
            log_path.display(),
            tail
        ),
    }
}

#[cfg(any(unix, windows))]
fn map_ensure_error(error: anyhow::Error) -> anyhow::Error {
    if let Some(host) = error.downcast_ref::<leveler_runtime_host::EnsureError>() {
        return anyhow::anyhow!(format_ensure_error(host));
    }
    error
}

#[cfg(any(unix, windows))]
async fn ensure_tui_runtime(layout: &Layout) -> anyhow::Result<LocalSocketRuntimeClient> {
    let client = leveler_runtime_host::ensure_default_runtime(
        layout,
        &daemon_launch()?,
        Arc::new(TuiHandoffUi),
    )
    .await
    .map_err(map_ensure_error)?;
    if let Some(host) = leveler_execution::execution_host::ExecutionHostClient::probe(
        &leveler_app::execution_host_config(layout)?,
    )
    .await
    .map_err(anyhow::Error::msg)?
    {
        let count = host
            .list()
            .await
            .map_err(anyhow::Error::msg)?
            .into_iter()
            .filter(|task| {
                matches!(
                    task.snapshot.status,
                    leveler_execution::BackgroundTaskStatus::Running
                        | leveler_execution::BackgroundTaskStatus::Killing
                )
            })
            .count();
        if count > 0 {
            eprintln!("{} {count}", HandoffLang::current().pick(
                "后台服务继续由独立执行宿主持有，不阻塞版本更新：",
                "Background services remain owned by the execution host and do not block version updates:",
            ));
        }
    }
    Ok(client)
}

#[cfg(test)]
#[cfg(unix)]
async fn observe_retiring_runtime(
    client: &LocalSocketRuntimeClient,
    socket_path: &Path,
    _reason: leveler_client_protocol::RestartReason,
    observed_pid: Option<u32>,
    interval: Duration,
) -> leveler_runtime_host::DrainOutcome {
    leveler_runtime_host::observe_retiring_runtime(
        client,
        socket_path,
        observed_pid,
        interval,
        &TuiHandoffUi,
    )
    .await
}

/// Bind the TUI-embedded Web UI against an existing local runtime service.
///
/// `/web` is a TUI capability, not an in-process-runtime capability: HTTP
/// lives in this process and routes through whatever [`LocalRuntimeService`]
/// the TUI already has. It does not own that runtime and does not open
/// daemon TCP.
async fn bind_tui_web_ui(
    service: Arc<dyn leveler_local_transport::LocalRuntimeService>,
    repo_root: PathBuf,
    shutdown: CancellationToken,
) -> Result<String, String> {
    let token = leveler_runtime_host::generate_daemon_token();
    let addr: SocketAddr = "127.0.0.1:0".parse().expect("valid loopback addr");
    let router = leveler_web::RouterService::new(service, repo_root);
    let manager = leveler_web::ProjectManager::new(
        router.clone(),
        leveler_core::LevelerHome::resolve(leveler_core::environment()),
        std::env::current_exe().ok(),
    );
    let background = manager.clone();
    tokio::spawn(async move {
        background.clone().load_registry().await;
        background
            .discover_historical_projects(&std::env::temp_dir())
            .await;
    });
    let server = leveler_web::bind_multi(router, manager, addr, token.clone())
        .await
        .map_err(|e| e.to_string())?;
    let local = server.local_addr();
    if !local.ip().is_loopback() {
        return Err(format!("refusing to serve Web UI on non-loopback {local}"));
    }
    let url = format!("http://{local}/?token={token}");
    tokio::spawn(async move {
        let _ = server.serve(shutdown).await;
    });
    Ok(url)
}

/// Shared `/web` launcher for in-process and daemon-connected TUIs.
fn make_web_launcher(
    service: Arc<dyn leveler_local_transport::LocalRuntimeService>,
    repo_root: PathBuf,
    shutdown: CancellationToken,
) -> leveler_tui::WebLauncher {
    Arc::new(move || {
        let service = service.clone();
        let repo_root = repo_root.clone();
        let shutdown = shutdown.clone();
        Box::pin(async move { bind_tui_web_ui(service, repo_root, shutdown).await })
    })
}

fn make_url_opener() -> leveler_tui::UrlOpener {
    Arc::new(move |url| {
        Box::pin(async move {
            tokio::task::spawn_blocking(move || leveler_execution::open_url(&url))
                .await
                .map_err(|error| format!("URL 打开任务异常结束：{error}"))?
                .map_err(|error| error.to_string())
        })
    })
}

/// The collaboration axis every NEW interactive TUI session opens on.
///
/// An interactive terminal session is a conversation, and `/goal <task>` is the
/// durable way to ask for the goal lifecycle. The statement lives ONCE, on the
/// axis vocabulary itself
/// ([`leveler_lifecycle::CollaborationMode::interactive_session`]), because
/// every interactive host states it — the terminal, the Web host and the
/// Desktop bridge. Reading `CollaborationMode::default()` instead would hand the
/// axis to the product's coding-session default, and interactive clients would
/// silently disagree the moment that default moved. Resuming a session takes its
/// persisted axis and never passes through here.
fn interactive_session_collaboration() -> leveler_local_transport::CollaborationMode {
    leveler_local_transport::CollaborationMode::interactive_session()
}

/// The session-open request a bare `leveler` / `leveler tui` issues.
///
/// The axis is stated by [`interactive_session_collaboration`] and is `Chat`.
/// `leveler run` and a wire request that omits the field keep resolving to the
/// product default (Goal), and resume takes the session's persisted axis.
fn interactive_session_request(
    model: Option<leveler_client_protocol::ModelRef>,
    mode: leveler_client_protocol::PermissionProfile,
    auto_approve: bool,
) -> CreateSessionRequest {
    CreateSessionRequest {
        collaboration: interactive_session_collaboration(),
        workspace: leveler_local_transport::CreateWorkspaceSelection::RuntimeDefault,
        goal: "interactive session".to_string(),
        model,
        mode,
        // `--auto-approve` becomes this session's policy; the daemon runs the
        // turn and it survives this client disconnecting.
        approval_policy: if auto_approve {
            leveler_client_protocol::ApprovalPolicy::AutoApprove
        } else {
            leveler_client_protocol::ApprovalPolicy::Interactive
        },
    }
}

/// Open the interactive terminal UI. Reuses a healthy per-repository daemon
/// when possible and otherwise starts the runtime inside the TUI process.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn cmd_tui(
    layout: Layout,
    model: Option<String>,
    mode: Option<RunMode>,
    auto_approve: bool,
    in_process: bool,
    socket: Option<PathBuf>,
    session: Option<String>,
    config_overridden: bool,
) -> anyhow::Result<std::process::ExitCode> {
    // Start-up self-update runs before the terminal is taken over: a
    // successful install replaces this process in place, which is only safe
    // outside the alternate screen. It is on by default; package-manager users
    // can opt out with `[update].auto_update = false`.
    crate::upgrade_cmd::run_startup_update().await;
    if in_process && socket.is_some() {
        anyhow::bail!("--socket cannot be combined with --in-process");
    }
    let explicit_socket = socket.is_some();
    let intent = socket_intent(in_process, auto_approve, explicit_socket, config_overridden);
    if auto_approve && intent == SocketIntent::RequireExplicit {
        anyhow::bail!(
            "socket clients cannot elevate daemon permissions; start `leveler serve \
             --auto-approve` instead"
        );
    }
    let socket_path = socket.unwrap_or_else(|| layout.socket_path());
    let socket_client = match intent {
        SocketIntent::Embedded => None,
        // The normal product path: discover the repository's runtime, start
        // one when none is running, and connect. The TUI does not own task
        // lifetime here — closing it leaves the daemon (and its tasks)
        // running. Platforms without local IPC keep the embedded runtime.
        #[cfg(any(unix, windows))]
        SocketIntent::ProbeDefault => {
            let client = ensure_tui_runtime(&layout).await?;
            // Supervisor semantics for a daemon that dies mid-session: the
            // client's reconnect/request paths call back into the same
            // ensure-daemon flow (idempotent; a concurrent revival race
            // elects one winner), the restarted daemon keeps its durable
            // RuntimeId, recovery reacquires a fresh OwnerEpoch inside the
            // daemon, and session subscriptions resync from a fresh snapshot.
            client.set_reviver(Arc::new(leveler_runtime_host::DaemonReviver::new(
                layout.clone(),
                daemon_launch()?,
                Arc::new(TuiHandoffUi),
            )));
            Some(client)
        }
        #[cfg(not(any(unix, windows)))]
        SocketIntent::ProbeDefault => leveler_runtime_host::probe_default_runtime(&socket_path)
            .await
            .map_err(|error| {
                anyhow::anyhow!(
                    "the local runtime at {} answered the probe but rejected the client: {error}",
                    socket_path.display()
                )
            })?,
        SocketIntent::RequireExplicit => Some(
            leveler_runtime_host::connect_existing_runtime(&socket_path)
                .await
                .map_err(|error| {
                    anyhow::anyhow!(
                        "cannot connect to requested local runtime at {}: {error}; start \
                         `leveler serve --socket {}` for this repository",
                        socket_path.display(),
                        socket_path.display()
                    )
                })?,
        ),
    };
    if let Some(client) = socket_client {
        let model = model
            .as_deref()
            .map(crate::common::parse_model_ref)
            .transpose()?;
        let client = Arc::new(client);
        let (session_id, context_window) = if let Some(id) = session.as_deref() {
            let session_id = leveler_core::SessionId::new(id);
            let snap = client.snapshot(&session_id).await.map_err(|e| {
                anyhow::anyhow!(
                    "cannot open session {id}: {e}\n\
                     list sessions: leveler resume"
                )
            })?;
            // R006 R6-P2: --auto-approve was a silent no-op on `--session`
            // resume — the policy lives only in the daemon's session map, and
            // reconnecting never re-asserted it, so approvals fell back to a
            // 5-minute human timeout → Deny → guard kill. Re-assert the
            // session's effective policy through the same trust gate as
            // creation. Without the flag nothing changes (no blanket upgrade).
            if auto_approve {
                use leveler_local_transport::LocalRuntimeService as _;
                client
                    .attach_session_policy(
                        &session_id,
                        leveler_client_protocol::ApprovalPolicy::AutoApprove,
                    )
                    .await
                    .map_err(|e| {
                        anyhow::anyhow!("cannot re-assert auto-approve on session {id}: {e}")
                    })?;
            }
            // An explicit `--permission` overrides the persisted mode; absence
            // keeps it. The command persists AND moves the live cell, so the
            // UI, the row and the running turn cannot disagree.
            if let Some(explicit) = mode {
                let requested = map_mode(explicit);
                if requested.as_str() != snap.mode.as_str() {
                    client
                        .send(
                            leveler_client_protocol::ClientCommand::SetPermissionProfile {
                                session_id: session_id.clone(),
                                mode: wire_mode(requested),
                            },
                        )
                        .await
                        .map_err(|e| {
                            anyhow::anyhow!("cannot apply --permission to session {id}: {e}")
                        })?;
                }
            }
            // The daemon path has no model registry of its own and the
            // snapshot carries the model's name but not its limits, so the
            // context gauge had nothing to divide by — and, contrary to the
            // note that used to sit here, no later turn ever filled it in.
            // The window the user configured for that model is right here.
            let window = snap
                .model
                .as_ref()
                .and_then(|m| {
                    leveler_app::GlobalConfig::load()
                        .ok()
                        .and_then(|g| g.context_window_for(&m.to_string()))
                })
                .unwrap_or(0);
            (session_id, window)
        } else {
            // R006 R6-P5: bare `leveler tui` silently starts a NEW session, so
            // an interrupted long-running goal is easy to abandon by accident —
            // every resume in Batch #1 needed a session UUID the user has no
            // way to know. Surface resumable work before creating another one.
            if let Some(hint) = resumable_session_hint(&layout).await {
                eprintln!("{hint}");
            }
            let bootstrap = client
                .create_session(interactive_session_request(
                    model,
                    wire_mode(resolve_mode(mode, project_default_mode(&layout))),
                    auto_approve,
                ))
                .await?;
            (bootstrap.session.id, bootstrap.context_window)
        };
        let global = leveler_app::GlobalConfig::load()?;
        let boot = leveler_tui::Boot {
            session_id,
            user: std::env::var("USER").unwrap_or_else(|_| "there".to_string()),
            version: env!("CARGO_PKG_VERSION").to_string(),
            // Workbench Header already shows project context; no welcome card.
            show_welcome: false,
            draft_path: Some(layout.state_dir.join("draft.txt")),
            history_path: Some(layout.state_dir.join("input_history.json")),
            context_window,
            locale: leveler_tui::Locale::resolve(global.lang.as_deref()),
            untrusted_config: crate::trust_cmds::untrusted_config_display(
                layout.require_workspace()?,
            ),
            // The runtime reports the session's Thinking Level in the user's
            // own vocabulary, and the TUI adopts it from the first snapshot; a
            // boot value here would be a guess about capability.
            thinking: None,
        };
        // A phone can be served over this connection.
        //
        // `/remote` used to refuse here on the grounds that the TUI does not own
        // a runtime. It does not need to own one: the daemon on the other end of
        // this socket *is* a `LocalRuntimeService`, and it is on this machine —
        // the socket is a file in this repository's state directory. Refusing
        // meant that opening a TUI in a repository where `leveler serve` was
        // already running made the whole feature unreachable, with a message
        // about a "remote daemon" that named the one case this is not.
        let runtime_service: Arc<dyn leveler_local_transport::LocalRuntimeService> = client.clone();
        let remote_launcher = crate::remote_invite::launcher(
            runtime_service.clone(),
            layout.require_workspace()?.to_path_buf(),
            leveler_remote_agent::RemoteHome::new(
                leveler_core::LevelerHome::resolve(leveler_core::environment()).remote_state_dir(),
            ),
        );
        let web_shutdown = CancellationToken::new();
        let web_launcher = make_web_launcher(
            runtime_service,
            layout.require_workspace()?.to_path_buf(),
            web_shutdown.clone(),
        );
        let client: Arc<dyn InteractiveRuntimeClient> = client;
        let exit = leveler_tui::run(
            client,
            Some(web_launcher),
            Some(make_url_opener()),
            Some(remote_launcher),
            Some(crate::clean_cmd::clean_host()),
            boot,
        )
        .await?;
        // Stop the TUI-owned HTTP server; the local daemon keeps running.
        web_shutdown.cancel();
        // `/update` installed a new binary: replace this process, preserving
        // the original invocation. The daemon keeps serving either way.
        if exit == leveler_tui::TuiExit::Restart {
            crate::upgrade_cmd::restart_after_update();
        }
        return Ok(std::process::ExitCode::SUCCESS);
    }

    let app = Arc::new(Application::assemble(layout)?);
    app.reconcile_execution_services().await?;
    let model_ref = resolve_model(app.as_ref(), model)?;

    // One resolution per launch. A resume falls back to the PERSISTED mode
    // (never a default), a new session falls back to the project default; an
    // explicit `--permission` replaces either.
    let (session_id, mode, persisted_mode) = if let Some(id) = session.as_deref() {
        let session_id = leveler_core::SessionId::new(id);
        // Fail early with a clear message if the id is unknown for this repo.
        let db = app.open_database().await?;
        leveler_storage::SessionRepository::new(&db)
            .get(&session_id)
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "session `{id}` not found in this repository.\n\
                     list sessions: leveler resume"
                )
            })?;
        // A starting process reaps the zombie turns its own runtime left
        // behind. Creating a session did that; reopening one did not — so a
        // session resumed after the previous process was killed opened with the
        // killed turn still marked `running`, and the interface painted a live
        // "waiting for the model" clock over work that had been dead since the
        // kill. Scoped to this session: reopening one must not disturb another.
        app.reap_zombie_turns(&db, Some(&session_id)).await?;
        let persisted = app
            .persisted_permission_profile(&session_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("session `{id}` has no execution row"))?;
        let mode = resolve_mode(mode, persisted);
        (session_id, mode, Some(persisted))
    } else {
        let mode = resolve_mode(mode, project_default_mode(&app.layout));
        let id = app
            .create_session_with_collaboration(
                &model_ref,
                "interactive session",
                mode,
                interactive_session_collaboration(),
            )
            .await?;
        (id, mode, None)
    };

    let in_process_client = Arc::new(InProcessRuntimeClient::new_with_options(
        app.clone(),
        model_ref.clone(),
        mode,
        false,
        auto_approve,
    ));
    // The created row already carries `mode`; on a resume an explicit flag has
    // to move the persisted row + live cell like any other profile change.
    if let Some(persisted) = persisted_mode
        && persisted != mode
    {
        in_process_client
            .send(
                leveler_client_protocol::ClientCommand::SetPermissionProfile {
                    session_id: session_id.clone(),
                    mode: wire_mode(mode),
                },
            )
            .await?;
    }
    // `/web` inside the TUI binds the browser Web UI over this same in-process
    // runtime. The service is the `InProcessRuntimeClient` itself (it implements
    // `LocalRuntimeService`); the launcher mints a fresh token and serves on an
    // ephemeral loopback port, returning the token-carrying URL. Aggregation
    // mode, same as `leveler web`: without it every `/api/projects` endpoint
    // 404s ("打开项目失败: Not Found") and the sidebar can only ever show the
    // current repository.
    let web_service: Arc<dyn leveler_local_transport::LocalRuntimeService> =
        in_process_client.clone();
    let remote_service: Arc<dyn leveler_local_transport::LocalRuntimeService> =
        in_process_client.clone();
    let quit_client = in_process_client.clone();
    let remote_repo_root = app.layout.require_workspace()?.to_path_buf();
    let web_shutdown = CancellationToken::new();
    let web_launcher = make_web_launcher(
        web_service,
        app.layout.require_workspace()?.to_path_buf(),
        web_shutdown.clone(),
    );

    let client: Arc<dyn InteractiveRuntimeClient> = in_process_client;

    let draft_path = app.layout.state_dir.join("draft.txt");
    let history_path = app.layout.state_dir.join("input_history.json");
    // The active model's context window feeds the TUI context gauge.
    let context_window = app
        .config
        .models
        .iter()
        .find(|m| m.profile.id == model_ref.model && m.profile.provider == model_ref.provider)
        .map(|m| m.profile.limits.context_window)
        .unwrap_or(0);
    let boot = leveler_tui::Boot {
        session_id,
        user: std::env::var("USER").unwrap_or_else(|_| "there".to_string()),
        version: env!("CARGO_PKG_VERSION").to_string(),
        // Workbench Header already shows project context; no welcome card.
        show_welcome: false,
        draft_path: Some(draft_path),
        history_path: Some(history_path),
        context_window,
        // LEVELER_LANG → ~/.leveler/config.toml lang → system → zh.
        locale: leveler_tui::Locale::resolve(app.config.lang.as_deref()),
        untrusted_config: crate::trust_cmds::untrusted_config_display(
            app.layout.require_workspace()?,
        ),
        thinking: app
            .config
            .models
            .iter()
            .find(|m| m.profile.id == model_ref.model && m.profile.provider == model_ref.provider)
            .and_then(|m| leveler_app::ui_thinking_state(Some(&m.profile), None)),
    };

    // `/remote`: the agent serves paired phones over this same in-process
    // runtime, so remote access lives exactly as long as this session.
    let remote_launcher = crate::remote_invite::launcher(
        remote_service,
        remote_repo_root,
        leveler_remote_agent::RemoteHome::new(
            leveler_core::LevelerHome::resolve(leveler_core::environment()).remote_state_dir(),
        ),
    );

    let exit = leveler_tui::run(
        client,
        Some(web_launcher),
        Some(make_url_opener()),
        Some(remote_launcher),
        Some(crate::clean_cmd::clean_host()),
        boot,
    )
    .await?;
    web_shutdown.cancel();
    if exit == leveler_tui::TuiExit::Restart {
        // The idle `/update` path: shut the in-process runtime down so its
        // background work is not abandoned mid-flight, then replace the
        // process. The new binary starts a fresh runtime.
        let _ = quit_client
            .send(leveler_client_protocol::ClientCommand::Quit)
            .await;
        crate::upgrade_cmd::restart_after_update();
    }
    // Drop-based reapers never run past `std::process::exit`; shut the
    // runtime down explicitly (background tasks + browser tree, R004 F7).
    let _ = quit_client
        .send(leveler_client_protocol::ClientCommand::Quit)
        .await;
    // The TUI owns the only runtime, and an in-process `/web` server is spawned
    // detached on a token it never cancels (it serves "until the process
    // exits"). Falling through to a normal return would drop the runtime with
    // that task still live, which hangs the process — the terminal is restored
    // and the composer draft/history are already persisted inside `run`, but the
    // shell never comes back. Exit the process directly so a running Web UI can
    // never keep the CLI alive after the user quits the TUI.
    std::process::exit(0);
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn cmd_serve(
    layout: Layout,
    model: Option<String>,
    mode: Option<RunMode>,
    auto_approve: bool,
    sandbox: bool,
    socket: Option<PathBuf>,
    tcp: Option<SocketAddr>,
    ready_json: Option<PathBuf>,
) -> anyhow::Result<std::process::ExitCode> {
    let socket_path = socket.unwrap_or_else(|| layout.socket_path());
    let app = Arc::new(Application::assemble(layout)?);
    let model_ref = resolve_model(app.as_ref(), model)?;
    let default_mode = resolve_mode(mode, project_default_mode(&app.layout));

    // Minted here so the runtime can retire this process itself once work
    // drains (ShutdownWhenIdle); the signal handlers below cancel the same
    // token, so there is one shutdown path rather than two.
    let shutdown = CancellationToken::new();
    // One client-presence counter, shared by both transports and the runtime:
    // the daemon's idle eviction reads exactly what the transport counts.
    let local_waiters = leveler_local_transport::LocalWaiters::new();
    let runtime = Arc::new(
        InProcessRuntimeClient::new_with_options(
            app.clone(),
            model_ref.clone(),
            default_mode,
            sandbox,
            auto_approve,
        )
        .with_process_shutdown(shutdown.clone())
        .with_client_presence(local_waiters.clone()),
    );
    // The host establishes exclusive ownership before recovery and publishes
    // readiness only after the application finishes its existing recovery.
    let prepared = leveler_runtime_host::prepare_daemon(
        &app,
        &runtime,
        &socket_path,
        tcp,
        ready_json.as_deref(),
        local_waiters,
    )
    .await?;
    let bound = prepared.bound;
    let runtime_id = prepared.runtime_id;

    println!("{}", Line::heading("Local runtime ready"));
    if let Some(server) = &bound.unix {
        println!("  socket: {}", server.path().display());
    }
    if let Some((server, token)) = &bound.tcp {
        println!("  tcp: {}", server.local_addr()?);
        // Printed once to the operator's own terminal: this is how a WebUI /
        // external client authenticates. Not logged elsewhere.
        println!("  token: {token}");
    }
    println!("  model: {model_ref}");
    println!("  runtime: {runtime_id}");
    println!("  press Ctrl+C to stop the daemon");

    let signal_shutdown = shutdown.clone();
    tokio::spawn(async move {
        // Ctrl-C and SIGTERM both reach the graceful path: a plain `kill`
        // must not orphan background tasks / the browser tree (R004 F7).
        #[cfg(unix)]
        {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("install SIGTERM handler");
            // SIGHUP: a closing spawn terminal must trigger the same graceful
            // path, never a default hard kill that orphans background tasks
            // and wipes session policy (R006 R6-P2).
            let mut hup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
                .expect("install SIGHUP handler");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
                _ = hup.recv() => {}
            }
            signal_shutdown.cancel();
        }
        #[cfg(not(unix))]
        if tokio::signal::ctrl_c().await.is_ok() {
            signal_shutdown.cancel();
        }
    });
    let unix_serve = async {
        match bound.unix {
            Some(server) => server.serve(shutdown.clone()).await,
            None => Ok(()),
        }
    };
    let tcp_serve = async {
        match bound.tcp {
            Some((server, _)) => server.serve(shutdown.clone()).await,
            None => Ok(()),
        }
    };
    let result = tokio::try_join!(unix_serve, tcp_serve);
    // Stopping the daemon is an explicit runtime shutdown, unlike closing a
    // TUI client. Cancel and reap any remaining turns before process exit.
    let _ = runtime
        .send(leveler_client_protocol::ClientCommand::Quit)
        .await;
    result?;
    Ok(std::process::ExitCode::SUCCESS)
}

/// Start the browser WebUI server. Default: assemble an in-process runtime
/// (like `serve`); with `--connect`, bridge an existing TCP daemon instead.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn cmd_web(
    layout: Layout,
    addr: SocketAddr,
    connect: Option<SocketAddr>,
    token: Option<String>,
    model: Option<String>,
    mode: Option<RunMode>,
    auto_approve: bool,
    sandbox: bool,
) -> anyhow::Result<std::process::ExitCode> {
    // The owned runtime is kept only so an in-process server can shut it down
    // explicitly on exit; a connected daemon belongs to another process. The
    // in-process path runs in aggregation mode: the primary repository behind
    // a RouterService, plus registered project daemons (probe-or-spawn).
    let (server, runtime, token) = match connect {
        Some(daemon_addr) => {
            let token =
                token.ok_or_else(|| anyhow::anyhow!("--token is required with --connect"))?;
            let client = LocalSocketRuntimeClient::connect_tcp(daemon_addr, token.clone()).await?;
            let service: Arc<dyn leveler_local_transport::LocalRuntimeService> =
                Arc::new(DaemonService(client));
            // Aggregation is available in --connect mode too, so the WebUI's
            // "open project" flow works (POST /api/projects) instead of 404ing.
            // The daemon is the primary behind the RouterService; added projects
            // get their own probe-or-spawn daemons like the in-process path.
            let router =
                leveler_web::RouterService::new(service, layout.require_workspace()?.to_path_buf());
            let manager = leveler_web::ProjectManager::new(
                router.clone(),
                leveler_core::LevelerHome::resolve(leveler_core::environment()),
                std::env::current_exe().ok(),
            );
            let background = manager.clone();
            tokio::spawn(async move {
                background.clone().load_registry().await;
                background
                    .discover_historical_projects(&std::env::temp_dir())
                    .await;
            });
            let server = leveler_web::bind_multi(router, manager, addr, token.clone()).await?;
            (server, None, token)
        }
        None => {
            if token.is_some() {
                return Err(anyhow::anyhow!(
                    "--token is only meaningful with --connect (without it, a token is generated)"
                ));
            }
            let app = Arc::new(Application::assemble(layout)?);
            app.reconcile_execution_services().await?;
            let model_ref = resolve_model(app.as_ref(), model)?;
            let runtime = Arc::new(InProcessRuntimeClient::new_with_options(
                app.clone(),
                model_ref,
                resolve_mode(mode, project_default_mode(&app.layout)),
                sandbox,
                auto_approve,
            ));
            let service: Arc<dyn leveler_local_transport::LocalRuntimeService> = runtime.clone();
            let db = app.open_database().await?;
            let engine = app.task_engine(&db)?;
            let reap = leveler_engine::reap_after_restart(
                &engine,
                None,
                leveler_engine::ReapScope::EndedBoots,
            )
            .await?;
            app.finish_reaped_sessions(&engine, &reap.reaped_sessions)
                .await?;
            if !reap.events.is_empty() {
                tracing::warn!(
                    reaped = reap.events.len(),
                    "reaped zombie turns before WebUI startup"
                );
            }
            app.start_memory_consolidator().await?;
            let router = leveler_web::RouterService::new(
                service,
                app.layout.require_workspace()?.to_path_buf(),
            );
            let manager = leveler_web::ProjectManager::new(
                router.clone(),
                leveler_core::LevelerHome::resolve(leveler_core::environment()),
                std::env::current_exe().ok(),
            );
            // Bring persisted projects online in the background — the server
            // must not wait on daemons that need spawning. Then register
            // every repository that has Leveler state (TUI-only projects), so
            // the sidebar lists all previously used projects.
            let background = manager.clone();
            tokio::spawn(async move {
                background.clone().load_registry().await;
                background
                    .discover_historical_projects(&std::env::temp_dir())
                    .await;
            });
            let token = leveler_runtime_host::generate_daemon_token();
            let server = leveler_web::bind_multi(router, manager, addr, token.clone()).await?;
            (server, Some(runtime), token)
        }
    };

    println!("{}", Line::heading("Web UI ready"));
    // Printed once to the operator's own terminal: the URL carries the bearer
    // token the browser needs. Not logged elsewhere.
    println!("  url: http://{}/?token={token}", server.local_addr());
    println!("  press Ctrl+C to stop the server");

    let shutdown = CancellationToken::new();
    let signal_shutdown = shutdown.clone();
    tokio::spawn(async move {
        // Ctrl-C and SIGTERM both reach the graceful path: a plain `kill`
        // must not orphan background tasks / the browser tree (R004 F7).
        #[cfg(unix)]
        {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("install SIGTERM handler");
            // SIGHUP: a closing spawn terminal must trigger the same graceful
            // path, never a default hard kill that orphans background tasks
            // and wipes session policy (R006 R6-P2).
            let mut hup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
                .expect("install SIGHUP handler");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
                _ = hup.recv() => {}
            }
            signal_shutdown.cancel();
        }
        #[cfg(not(unix))]
        if tokio::signal::ctrl_c().await.is_ok() {
            signal_shutdown.cancel();
        }
    });
    let result = server.serve(shutdown).await;
    if let Some(runtime) = runtime {
        // Stopping an in-process server is an explicit runtime shutdown,
        // unlike closing a browser tab. Cancel and reap remaining turns.
        let _ = runtime
            .send(leveler_client_protocol::ClientCommand::Quit)
            .await;
    }
    result?;
    Ok(std::process::ExitCode::SUCCESS)
}

/// A `LocalRuntimeService` facade over a TCP-connected daemon client: the
/// transport client has the right methods but cannot implement the trait in
/// its own crate without a dependency cycle, so the impl lives here.
struct DaemonService(LocalSocketRuntimeClient);

#[async_trait::async_trait]
impl InteractiveRuntimeClient for DaemonService {
    async fn send(
        &self,
        command: leveler_client_protocol::ClientCommand,
    ) -> Result<(), leveler_client_protocol::ClientError> {
        self.0.send(command).await
    }

    async fn deliver(
        &self,
        envelope: leveler_client_protocol::CommandEnvelope,
    ) -> Result<(), leveler_client_protocol::ClientError> {
        self.0.deliver(envelope).await
    }

    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<leveler_client_protocol::RuntimeEvent> {
        self.0.subscribe()
    }

    fn subscribe_session(
        &self,
        session_id: &leveler_core::SessionId,
    ) -> tokio::sync::broadcast::Receiver<leveler_client_protocol::RuntimeEvent> {
        self.0.subscribe_session(session_id)
    }

    async fn snapshot(
        &self,
        session_id: &leveler_core::SessionId,
    ) -> Result<leveler_client_protocol::UiSessionSnapshot, leveler_client_protocol::ClientError>
    {
        self.0.snapshot(session_id).await
    }
}

#[async_trait::async_trait]
impl leveler_local_transport::LocalRuntimeService for DaemonService {
    async fn create_session(
        &self,
        request: CreateSessionRequest,
    ) -> Result<leveler_local_transport::SessionBootstrap, leveler_client_protocol::ClientError>
    {
        self.0.create_session(request).await
    }

    async fn local_waiter_count(&self) -> Result<usize, leveler_client_protocol::ClientError> {
        self.0.local_waiter_count().await
    }

    async fn runtime_info(
        &self,
    ) -> Result<leveler_client_protocol::RuntimeInfo, leveler_client_protocol::ClientError> {
        leveler_local_transport::LocalRuntimeService::runtime_info(&self.0).await
    }
}

/// Interactive resume: reopen a session in the TUI (the mainstream `resume`).
/// With no id, list recent sessions so the user can pick one to reopen.
pub(crate) async fn cmd_resume(
    layout: Layout,
    id: Option<String>,
    config_overridden: bool,
) -> anyhow::Result<std::process::ExitCode> {
    let Some(id) = id else {
        return list_sessions_for_resume(layout).await;
    };
    // Reopen reuses the TUI session path; persisted model/mode are restored.
    // No explicit `--permission`: absence keeps the persisted mode.
    cmd_tui(
        layout,
        None,
        None,
        false,
        false,
        None,
        Some(id),
        config_overridden,
    )
    .await
}

/// Print recent sessions with a copy-paste `leveler resume <id>` hint.
async fn list_sessions_for_resume(layout: Layout) -> anyhow::Result<std::process::ExitCode> {
    let app = Application::assemble(layout)?;
    let db = app.open_database().await?;
    let sessions = leveler_storage::SessionRepository::new(&db).list().await?;
    if sessions.is_empty() {
        println!(
            "{}",
            Line::warn("No sessions yet. Start one with `leveler`.")
        );
        return Ok(std::process::ExitCode::SUCCESS);
    }
    println!(
        "{}",
        Line::heading("Recent sessions — reopen with `leveler resume <id>`")
    );
    for s in sessions.iter().take(20) {
        println!("  {}  [{}]  {}", s.id, s.status.as_str(), s.goal);
    }
    Ok(std::process::ExitCode::SUCCESS)
}

/// A one-line notice when this repository has work that could be resumed.
///
/// R6-P5: the affordance gap is not that resume is missing — it is that
/// starting fresh is the silent default, so an interrupted long-running goal
/// is easy to abandon by accident. Returns `None` when there is nothing to
/// resume, and never fails the launch: a hint that cannot be produced is not
/// a reason to refuse to start.
async fn resumable_session_hint(layout: &Layout) -> Option<String> {
    let app = Application::assemble(layout.clone()).ok()?;
    let db = app.open_database().await.ok()?;
    let sessions = leveler_storage::SessionRepository::new(&db)
        .list()
        .await
        .ok()?;
    let rows: Vec<(String, String, String)> = sessions
        .iter()
        .map(|s| (s.id.clone(), s.status.as_str().to_string(), s.goal.clone()))
        .collect();
    format_resumable_hint(&rows).map(|body| Line::warn(&body).to_string())
}

/// Pure formatting half of [`resumable_session_hint`], split out so the rule
/// (which statuses count, what the user is told) is testable without a daemon.
fn format_resumable_hint(sessions: &[(String, String, String)]) -> Option<String> {
    let resumable: Vec<_> = sessions
        .iter()
        .filter(|(_, status, _)| matches!(status.as_str(), "incomplete" | "blocked"))
        .collect();
    let (id, status, goal) = resumable.first()?;
    let first_line = goal.lines().next().unwrap_or("").trim();
    let goal = if first_line.chars().count() > 60 {
        let cut: String = first_line.chars().take(60).collect();
        format!("{cut}…")
    } else {
        first_line.to_string()
    };
    let more = match resumable.len() {
        1 => String::new(),
        n => format!(" (+{} more — `leveler resume`)", n - 1),
    };
    Some(format!(
        "Starting a NEW session. Unfinished work here: {id} [{status}] {goal}{more}\n  \
         resume it with: leveler --session {id}"
    ))
}

/// Headless recovery of an interrupted non-interactive run (`run --resume`).
pub(crate) async fn cmd_run_resume(
    layout: Layout,
    id: String,
    mode: Option<RunMode>,
    auto_approve: bool,
    confirm_recovery: bool,
    output: OutputFormat,
) -> anyhow::Result<std::process::ExitCode> {
    // Resume reloads collaboration and active capabilities from durable state.
    let app = Application::assemble(layout)?;
    let session_id = leveler_core::SessionId::new(id.clone());

    // The row is the axis SoT; an unreadable row falls back to the product
    // default (Goal), never to a silent Chat. The same value drives the exit
    // code, so an answer is a success exactly when the axis says so.
    let collaboration = app.session_product_axes(&session_id).await.ok();
    let execution = CollaborationExecution::of(collaboration.unwrap_or_default());

    // An explicit `--permission` overrides the persisted mode. This is the one
    // durable write the resume then reads back, so the resumed turn, the row
    // and any later snapshot all agree. Absence keeps the persisted mode.
    if let Some(explicit) = mode {
        app.set_persisted_permission_profile(&session_id, map_mode(explicit))
            .await?;
    }

    // The explicit answer to a RecoveryConfirmationRequired stop: the user
    // inspected the workspace, so close the interrupted call(s) first.
    if confirm_recovery {
        let closed = app.acknowledge_crash_window(&session_id).await?;
        if output == OutputFormat::Text {
            println!(
                "{}",
                Line::warn(&format!(
                    "Acknowledged {closed} interrupted tool call(s); they were NOT replayed."
                ))
            );
        }
    }

    if output == OutputFormat::Text {
        println!("{}", Line::heading(&format!("Resuming session {id}")));
        if let Some(collaboration) = collaboration {
            println!("  collab: {}", collaboration.as_str());
        }
    }

    let approver = build_approver(auto_approve);
    let cancellation = CancellationToken::new();
    spawn_interrupt_handler(cancellation.clone());

    let result = app
        .resume_session(
            &session_id,
            approver,
            &mut |e| render_event(e, output),
            cancellation,
        )
        .await;

    finish(result, &id, output, execution)
}

/// Run the git/GitHub workflow for the produced changes and print the result.
async fn ship_changes_and_print(
    app: &Application,
    model: &leveler_model::ModelRef,
    goal: &str,
    modified: &[String],
    ship: &leveler_app::ShipOptions,
) {
    println!("{}", Line::heading("Shipping changes"));
    match app
        .ship_changes(goal, modified, model, ship, CancellationToken::new())
        .await
    {
        Ok(out) => {
            if out.committed {
                let sha = out
                    .commit_sha
                    .as_deref()
                    .map(|s| format!(" ({})", &s[..s.len().min(8)]))
                    .unwrap_or_default();
                println!("{}", Line::ok(&format!("committed to {}{sha}", out.branch)));
            }
            if out.pushed {
                println!("{}", Line::ok(&format!("pushed {}", out.branch)));
            }
            if let Some(url) = &out.pr_url {
                println!("{}", Line::ok(&format!("pull request: {url}")));
            }
            for note in &out.notes {
                println!("{}", Line::warn(note));
            }
        }
        Err(e) => println!("{}", Line::fail(&format!("ship failed: {e}"))),
    }
    println!();
}

/// Exit code for a run that ended honestly without an independently verified
/// completion: `Blocked`, `Stalled`, `Incomplete`, `BudgetExhausted` or
/// `TurnLimitReached`. `Answered` joins them only for a Goal axis; for chat and
/// plan the answer IS the terminal the axis exists to reach.
///
/// It is deliberately distinct from [`std::process::ExitCode::FAILURE`] (1),
/// which is reserved for an execution/runtime/provider error. An honest
/// refusal must not look like a crash to a shell, CI job or evaluator. The
/// exact cause stays available machine-readably in the JSONL
/// `session_completed` event's `stop_reason`.
pub(crate) const NOT_COMPLETED_EXIT_CODE: u8 = 3;

/// The headless `leveler run` / `leveler run --resume` exit-code contract.
///
/// `StopReason` is the runtime's authoritative outcome fact and is also
/// emitted machine-readably. The process exit code is its coarse scriptable
/// projection, read through the collaboration dispatch so an axis's own
/// terminal counts as success:
///
/// ```text
/// 0    completed / answered   goal completion, or the answer a chat/plan run exists for
/// 1    failed                  execution / runtime / provider error (crash, transport)
/// 2    usage error             CLI argument error (clap)
/// 3    not completed           agent ended honestly without reaching the axis's terminal
/// 130  interrupted             cancelled; the session is resumable
/// ```
///
/// A run can therefore never report an honest non-completion with the same
/// code as a crash.
fn outcome_exit_code(
    stop_reason: StopReason,
    execution: CollaborationExecution,
) -> std::process::ExitCode {
    match stop_reason {
        StopReason::Completed => std::process::ExitCode::SUCCESS,
        // `Answered` is the Goal axis's unresolved state; Chat and Plan end on
        // it by design.
        StopReason::Answered if execution.answer_is_the_terminal() => {
            std::process::ExitCode::SUCCESS
        }
        StopReason::Answered
        | StopReason::Incomplete
        | StopReason::BudgetExhausted
        | StopReason::TurnLimitReached
        | StopReason::Blocked
        | StopReason::Stalled => std::process::ExitCode::from(NOT_COMPLETED_EXIT_CODE),
    }
}

/// Render the final summary and pick an exit code, handling cancellation
/// gracefully (a cancelled run is resumable, not a hard error).
fn finish(
    result: Result<leveler_agent::AgentOutcome, leveler_app::AppError>,
    session_id: &str,
    output: OutputFormat,
    execution: CollaborationExecution,
) -> anyhow::Result<std::process::ExitCode> {
    match result {
        Ok(outcome) => {
            if output == OutputFormat::Text {
                println!();
                if !outcome.modified_files.is_empty() {
                    println!("{}", Line::heading("Modified files"));
                    for f in &outcome.modified_files {
                        println!("  {f}");
                    }
                    println!();
                }
                match outcome.stop_reason {
                    StopReason::Completed => println!(
                        "{}",
                        Line::ok(&format!(
                            "Completed in {} model step(s).",
                            outcome.model_steps
                        ))
                    ),
                    StopReason::Answered => {
                        if execution.answer_is_the_terminal() {
                            println!(
                                "{}",
                                Line::ok(&format!(
                                    "Answered in {} model step(s).",
                                    outcome.model_steps
                                ))
                            )
                        } else {
                            println!(
                                "{}",
                                Line::warn(&format!(
                                    "Answer ended after {} model step(s); task completion was not independently verified.",
                                    outcome.model_steps
                                ))
                            )
                        }
                    }
                    StopReason::Incomplete => println!(
                        "{}",
                        Line::warn(&format!(
                            "Stopped after {} model step(s): completeness could not be established.",
                            outcome.model_steps
                        ))
                    ),
                    StopReason::BudgetExhausted => println!(
                        "{}",
                        Line::warn(&format!(
                            "Stopped after {} model step(s): {} Resume with: leveler resume {session_id}",
                            outcome.model_steps, outcome.final_text
                        ))
                    ),
                    StopReason::TurnLimitReached => println!(
                        "{}",
                        Line::warn(&format!(
                            "Hit the model-step safety ceiling after {} model step(s). \
                             The turn was force-stopped to guarantee termination; \
                             check if the model was looping.",
                            outcome.model_steps
                        ))
                    ),
                    StopReason::Blocked => println!(
                        "{}",
                        Line::warn(&format!(
                            "Stopped: the model reported the goal blocked after {} model step(s).",
                            outcome.model_steps
                        ))
                    ),
                    StopReason::Stalled => println!(
                        "{}",
                        Line::warn(&format!(
                            "Stopped: the model went quiet without resolving the goal \
                             after {} model step(s) (not verified).",
                            outcome.model_steps
                        ))
                    ),
                }
            } else {
                emit_jsonl(serde_json::json!({
                    "type": "session_completed",
                    "session_id": session_id,
                    "stop_reason": format!("{:?}", outcome.stop_reason),
                    // Wire/CLI key kept as `rounds` for existing consumers; the
                    // value is the model-step count (a mechanical iteration
                    // count, never a task-progress measure).
                    "rounds": outcome.model_steps,
                    "modified_files": outcome.modified_files,
                }));
            }
            Ok(outcome_exit_code(outcome.stop_reason, execution))
        }
        Err(leveler_app::AppError::Agent(leveler_agent::AgentError::Cancelled)) => {
            if output == OutputFormat::Text {
                println!(
                    "\n{}",
                    Line::warn(&format!(
                        "Interrupted. Resume with: leveler resume {session_id}"
                    ))
                );
            } else {
                emit_jsonl(serde_json::json!({
                    "type": "session_interrupted",
                    "session_id": session_id,
                }));
            }
            Ok(std::process::ExitCode::from(130))
        }
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tui_runtime_selection_tests {
    use super::*;
    use leveler_local_transport::LocalRuntimeService;

    #[test]
    fn daemon_token_is_256_bits_of_hex_and_not_constant() {
        let a = generate_daemon_token();
        let b = generate_daemon_token();
        assert_eq!(a.len(), 64, "256-bit token → 64 hex chars: {a}");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()), "{a}");
        assert_ne!(a, b, "a CSPRNG token must not repeat");
    }

    #[test]
    fn default_launch_probes_an_existing_daemon_without_requiring_it() {
        assert_eq!(
            socket_intent(
                /*in_process*/ false, /*auto_approve*/ false,
                /*explicit_socket*/ false, /*config_overridden*/ false,
            ),
            SocketIntent::ProbeDefault,
        );
    }

    #[tokio::test]
    async fn missing_default_daemon_is_an_embedded_fallback_not_an_error() {
        let socket = std::env::temp_dir().join(format!(
            "leveler-missing-daemon-{}-{}.sock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));

        assert!(connect_default_runtime(&socket).await.unwrap().is_none());
    }

    /// A runtime that is still finishing work is NOT a startup failure.
    ///
    /// The defect this pins: the handover used to bail after 10s with
    /// "did not finish its work", so a busy old generation dead-ended the new
    /// TUI. It must instead wait for the runtime's own drain — here a
    /// background task outlives several observation intervals — and return
    /// only once the socket is released.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_busy_retiring_runtime_is_waited_on_not_failed() {
        use leveler_app::{Application, InProcessRuntimeClient};
        use leveler_local_transport::{LocalSocketRuntimeClient, LocalSocketServer};
        use leveler_model::ModelRef;
        use leveler_project::Layout;
        use std::sync::Arc;
        use std::time::Duration;

        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let repo = tmp.path().join("repo");
        let config = tmp.path().join("configs");
        std::fs::create_dir_all(home.join("state")).unwrap();
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(config.join("providers")).unwrap();
        std::fs::create_dir_all(config.join("models")).unwrap();
        std::fs::write(
            config.join("providers/mock.yaml"),
            "id: mock\nprotocol: openai_chat\nbase_url: http://127.0.0.1:9\n",
        )
        .unwrap();
        std::fs::write(
            config.join("models/m.yaml"),
            r#"
id: m
provider: mock
model_id: mock-model
protocol: openai_chat
capabilities:
  streaming: true
  tool_calling: true
  parallel_tool_calls: false
  structured_output: true
  reasoning: false
  vision: false
limits:
  context_window: 8192
  reliable_context: 4096
  max_output_tokens: 1024
  max_tool_schema_bytes: 8192
  max_parallel_tool_calls: 1
compatibility:
  synthesize_tool_call_ids: true
  drop_unsupported_fields: true
"#,
        )
        .unwrap();
        let layout = Layout::from_parts(repo.clone(), config, home.join("state"));
        let app = Arc::new(Application::assemble(layout).unwrap());
        let shutdown = tokio_util::sync::CancellationToken::new();
        let runtime: Arc<dyn leveler_local_transport::LocalRuntimeService> = Arc::new(
            InProcessRuntimeClient::new(
                app.clone(),
                ModelRef::new("mock", "m"),
                leveler_execution::PermissionProfile::Assisted,
                false,
            )
            .with_process_shutdown(shutdown.clone()),
        );
        let socket = tmp.path().join("handover.sock");
        let server = LocalSocketServer::bind(&socket, runtime).await.unwrap();
        let serve_shutdown = shutdown.clone();
        let serve = tokio::spawn(async move { server.serve(serve_shutdown).await });
        let client = LocalSocketRuntimeClient::connect(&socket).await.unwrap();

        // A background task that outlives several observation intervals, so the
        // wait genuinely sees a non-quiescent runtime before it drains.
        let task = app
            .background_tasks()
            .spawn(
                leveler_execution::ProcessRequest::new(
                    "sleep",
                    vec!["1".to_string()],
                    repo.clone(),
                ),
                None,
            )
            .await
            .expect("background task starts");

        client
            .send(leveler_client_protocol::ClientCommand::ShutdownWhenIdle {
                reason: leveler_client_protocol::RestartReason::BuildMismatch,
            })
            .await
            .unwrap();
        let health = leveler_local_transport::LocalRuntimeService::runtime_info(&client)
            .await
            .unwrap()
            .health;
        assert_eq!(health.active_turns, 0);
        assert_eq!(health.active_background_tasks, 1);
        assert!(!health.quiescent());
        assert!(health.shutting_down);
        let observed_pid = leveler_local_transport::LocalRuntimeService::runtime_info(&client)
            .await
            .unwrap()
            .pid;

        tokio::time::timeout(
            Duration::from_secs(15),
            observe_retiring_runtime(
                &client,
                &socket,
                leveler_client_protocol::RestartReason::BuildMismatch,
                Some(observed_pid),
                Duration::from_millis(25),
            ),
        )
        .await
        .expect("the handover completes once the work drains, without a startup failure");

        // The drain's own token fired, so the process would exit and the
        // socket is gone; the replacement could now bind.
        assert!(shutdown.is_cancelled());
        serve.await.unwrap().unwrap();
        drop(task);
    }

    #[test]
    fn explicit_socket_is_required_and_never_silently_downgraded() {
        assert_eq!(
            socket_intent(false, false, true, false),
            SocketIntent::RequireExplicit,
        );
    }

    #[test]
    fn non_replayable_launch_options_force_the_embedded_runtime() {
        // `config_overridden` still forces embedded this round (invocation-scoped
        // config a running daemon cannot inherit), as does `--in-process`.
        assert_eq!(
            socket_intent(false, false, false, true),
            SocketIntent::Embedded,
        );
        assert_eq!(
            socket_intent(true, false, false, false),
            SocketIntent::Embedded,
        );
    }

    // R1: `--auto-approve` is no longer an invocation-scoped reason to embed the
    // runtime in the TUI. It becomes a per-session approval policy carried on the
    // CreateSessionRequest, so an unattended goal runs in the daemon and survives
    // client disconnect. (§6.A)
    #[test]
    fn auto_approve_attaches_to_the_daemon_via_session_scoped_policy() {
        assert_eq!(
            socket_intent(
                /*in_process*/ false, /*auto_approve*/ true,
                /*explicit_socket*/ false, /*config_overridden*/ false,
            ),
            SocketIntent::ProbeDefault,
        );
    }

    // §6.C — an explicit `--in-process` must still win over `--auto-approve`.
    #[test]
    fn in_process_wins_over_auto_approve() {
        assert_eq!(
            socket_intent(true, true, false, false),
            SocketIntent::Embedded,
        );
    }

    #[test]
    fn explicit_socket_wins_over_implicit_reuse_restrictions() {
        assert_eq!(
            socket_intent(false, false, true, true),
            SocketIntent::RequireExplicit,
        );
    }

    #[test]
    fn in_process_and_socket_clients_are_the_same_web_capability() {
        fn check<T: LocalRuntimeService + InteractiveRuntimeClient + Send + Sync + 'static>() {}
        check::<InProcessRuntimeClient>();
        check::<LocalSocketRuntimeClient>();
    }

    #[test]
    fn tui_web_does_not_shell_out_to_leveler_web() {
        let src = include_str!("run_cmds.rs");
        assert!(
            !src.contains("arg(\"web\")"),
            "/web must call leveler-web, not spawn `leveler web`"
        );
    }
}

#[cfg(test)]
mod web_launcher_tests {
    use super::*;
    use leveler_client_protocol::{
        ClientCommand, ClientError, InteractiveRuntimeClient, RuntimeEvent, SessionId,
        UiSessionSnapshot,
    };
    use leveler_local_transport::{CreateSessionRequest, LocalRuntimeService, SessionBootstrap};
    use tokio::sync::broadcast;

    struct StubService {
        events: broadcast::Sender<RuntimeEvent>,
    }

    impl StubService {
        fn new() -> Self {
            Self {
                events: broadcast::channel(8).0,
            }
        }
    }

    #[async_trait::async_trait]
    impl InteractiveRuntimeClient for StubService {
        async fn send(&self, _command: ClientCommand) -> Result<(), ClientError> {
            Ok(())
        }
        async fn deliver(
            &self,
            _envelope: leveler_client_protocol::CommandEnvelope,
        ) -> Result<(), ClientError> {
            Ok(())
        }
        fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
            self.events.subscribe()
        }
        fn subscribe_session(&self, _session_id: &SessionId) -> broadcast::Receiver<RuntimeEvent> {
            self.events.subscribe()
        }
        async fn snapshot(
            &self,
            _session_id: &SessionId,
        ) -> Result<UiSessionSnapshot, ClientError> {
            Err(ClientError::Runtime("not exercised".into()))
        }
    }

    #[async_trait::async_trait]
    impl LocalRuntimeService for StubService {
        async fn create_session(
            &self,
            _request: CreateSessionRequest,
        ) -> Result<SessionBootstrap, ClientError> {
            Err(ClientError::Runtime("not exercised".into()))
        }
    }

    #[tokio::test]
    async fn make_web_launcher_binds_loopback_with_a_bearer_token() {
        let service: Arc<dyn LocalRuntimeService> = Arc::new(StubService::new());
        let shutdown = CancellationToken::new();
        let url = bind_tui_web_ui(
            service,
            PathBuf::from("/tmp/web-cap-repo"),
            shutdown.clone(),
        )
        .await
        .expect("bind Web UI");
        assert!(
            url.starts_with("http://127.0.0.1:"),
            "must be loopback, got {url}"
        );
        assert!(!url.contains("0.0.0.0"), "{url}");
        let token = url.split("token=").nth(1).expect("token-bearing URL");
        assert_eq!(token.len(), 64, "{url}");
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()), "{url}");
        let status = get_projects_status(&url).await;
        assert_eq!(
            status, 200,
            "/api/projects must stay on the multi-project surface, got {status}"
        );
        shutdown.cancel();
    }

    async fn get_projects_status(url: &str) -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let host = url
            .trim_start_matches("http://")
            .split('/')
            .next()
            .expect("host");
        let mut stream = tokio::net::TcpStream::connect(host)
            .await
            .expect("connect web");
        let req = format!(
            "GET /api/projects?token={token} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n",
            token = url.split("token=").nth(1).unwrap_or("")
        );
        stream.write_all(req.as_bytes()).await.unwrap();
        let mut buf = vec![0u8; 1024];
        let n = stream.read(&mut buf).await.unwrap();
        let resp = String::from_utf8_lossy(&buf[..n]);
        resp.split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    }

    #[tokio::test]
    async fn make_web_launcher_returns_the_bound_url() {
        let service: Arc<dyn LocalRuntimeService> = Arc::new(StubService::new());
        let shutdown = CancellationToken::new();
        let launcher = make_web_launcher(
            service,
            PathBuf::from("/tmp/web-cap-repo"),
            shutdown.clone(),
        );
        let url = launcher().await.expect("bind Web UI");
        assert!(url.starts_with("http://127.0.0.1:"), "{url}");
        shutdown.cancel();
    }
}

// Unix sockets + the loopback TCP daemon are unix-only; on Windows the
// transport returns Unavailable by design, so these binding tests are gated.
#[cfg(all(test, unix))]
mod daemon_bind_tests {
    use super::*;
    use leveler_client_protocol::{
        ClientCommand, ClientError, NotificationLevel, RuntimeEvent, SessionId, UiSessionSnapshot,
        mock::MockRuntimeClient,
    };
    use leveler_local_transport::{CreateSessionRequest, LocalRuntimeService, SessionBootstrap};
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::broadcast;

    /// Minimal LocalRuntimeService for bind tests: command surface is never
    /// exercised, only the transports are bound.
    struct TestService {
        mock: MockRuntimeClient,
        session_events: Mutex<HashMap<SessionId, broadcast::Sender<RuntimeEvent>>>,
        raw_sends: AtomicUsize,
        deliveries: AtomicUsize,
    }

    impl TestService {
        fn session_sender(&self, session_id: &SessionId) -> broadcast::Sender<RuntimeEvent> {
            self.session_events
                .lock()
                .unwrap()
                .entry(session_id.clone())
                .or_insert_with(|| broadcast::channel(64).0)
                .clone()
        }

        fn emit_for(&self, session_id: &SessionId, event: RuntimeEvent) {
            let _ = self.session_sender(session_id).send(event);
        }
    }

    #[async_trait::async_trait]
    impl InteractiveRuntimeClient for TestService {
        async fn send(&self, command: ClientCommand) -> Result<(), ClientError> {
            self.raw_sends.fetch_add(1, Ordering::SeqCst);
            self.mock.send(command).await
        }
        async fn deliver(
            &self,
            envelope: leveler_client_protocol::CommandEnvelope,
        ) -> Result<(), ClientError> {
            self.deliveries.fetch_add(1, Ordering::SeqCst);
            self.mock.deliver(envelope).await
        }
        fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
            self.mock.subscribe()
        }
        fn subscribe_session(&self, session_id: &SessionId) -> broadcast::Receiver<RuntimeEvent> {
            self.session_sender(session_id).subscribe()
        }
        async fn snapshot(&self, session_id: &SessionId) -> Result<UiSessionSnapshot, ClientError> {
            self.mock.snapshot(session_id).await
        }
    }

    #[async_trait::async_trait]
    impl LocalRuntimeService for TestService {
        async fn create_session(
            &self,
            _request: CreateSessionRequest,
        ) -> Result<SessionBootstrap, ClientError> {
            Err(ClientError::Runtime("not exercised".to_string()))
        }
    }

    fn test_service() -> Arc<TestService> {
        Arc::new(TestService {
            mock: MockRuntimeClient::new(SessionId::new("s-test")),
            session_events: Mutex::new(HashMap::new()),
            raw_sends: AtomicUsize::new(0),
            deliveries: AtomicUsize::new(0),
        })
    }

    #[tokio::test]
    async fn connected_web_bridge_preserves_the_daemon_s_runtime_contract() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("daemon.sock");
        let upstream = test_service();
        let mut bound = bind_daemon_transports(
            &sock,
            Some("127.0.0.1:0".parse().unwrap()),
            Some("bridge-token".to_string()),
            upstream.clone(),
            leveler_local_transport::LocalWaiters::new(),
        )
        .await
        .expect("daemon transports bind");
        let (server, token) = bound.tcp.take().expect("TCP transport");
        let addr = server.local_addr().unwrap();
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(server.serve(shutdown.clone()));

        let client = LocalSocketRuntimeClient::connect_tcp(addr, token)
            .await
            .expect("web bridge connects to daemon");
        let session_id = SessionId::new("s1");
        client
            .send(ClientCommand::OpenSession {
                session_id: session_id.clone(),
            })
            .await
            .expect("opens the daemon's per-session subscription");
        let bridge = DaemonService(client);
        let mut events = bridge.subscribe_session(&session_id);

        upstream.emit_for(
            &session_id,
            RuntimeEvent::Notification {
                level: NotificationLevel::Info,
                message: "session-only".to_string(),
            },
        );
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
            .await
            .expect("the web bridge lost the daemon's per-session stream")
            .unwrap();
        assert!(matches!(
            event,
            RuntimeEvent::Notification { message, .. } if message == "session-only"
        ));

        let sends_before = upstream.raw_sends.load(Ordering::SeqCst);
        bridge
            .issue(session_id, ClientCommand::RequestSessionList)
            .await
            .expect("the web bridge delivers an enveloped command");
        assert_eq!(
            upstream.deliveries.load(Ordering::SeqCst),
            1,
            "the web bridge must preserve the daemon's idempotent delivery boundary"
        );
        assert_eq!(
            upstream.raw_sends.load(Ordering::SeqCst),
            sends_before,
            "an enveloped web command must not be downgraded to raw send"
        );
        assert_eq!(
            bridge.local_waiter_count().await.unwrap(),
            0,
            "the count must come from the daemon, not the facade default of one; \
             a TCP peer is never a trusted local waiter, whatever ClientKind it \
             declares (see `tcp_forged_local_kind_cannot_disarm_remote_approval_timeout`)"
        );

        shutdown.cancel();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn tcp_mode_binds_the_unix_ownership_socket_too() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("daemon.sock");
        let bound = bind_daemon_transports(
            &sock,
            Some("127.0.0.1:0".parse().unwrap()),
            Some("test-token".to_string()),
            test_service(),
            leveler_local_transport::LocalWaiters::new(),
        )
        .await
        .expect("binds");
        assert!(bound.unix.is_some(), "TCP 模式也必须占住 Unix 锁 socket");
        assert!(sock.exists(), "锁 socket 文件必须真的落盘");
        let (tcp_server, token) = bound.tcp.as_ref().expect("tcp bound");
        assert_eq!(
            token, "test-token",
            "env 提供的 token 必须被沿用而不是重新生成"
        );
        assert!(tcp_server.local_addr().unwrap().port() > 0);
    }

    #[tokio::test]
    async fn local_socket_client_feeds_the_shared_web_binder() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("daemon.sock");
        let mut bound = bind_daemon_transports(
            &sock,
            None,
            None,
            test_service(),
            leveler_local_transport::LocalWaiters::new(),
        )
        .await
        .expect("unix daemon");
        let unix = bound.unix.take().expect("unix listener");
        let daemon_shutdown = CancellationToken::new();
        let daemon_task = tokio::spawn(unix.serve(daemon_shutdown.clone()));
        let client = LocalSocketRuntimeClient::connect(&sock)
            .await
            .expect("tui-style socket client");
        let service: Arc<dyn LocalRuntimeService> = Arc::new(client);
        let shutdown = CancellationToken::new();
        let url = bind_tui_web_ui(service, dir.path().to_path_buf(), shutdown.clone())
            .await
            .expect("socket-backed /web");
        assert!(
            url.starts_with("http://127.0.0.1:"),
            "loopback only, got {url}"
        );
        assert!(url.contains("?token="), "{url}");
        shutdown.cancel();
        daemon_shutdown.cancel();
        let _ = daemon_task.await;
    }

    /// Healthy attach must be a local operation with no noticeable latency:
    /// probe the socket, connect, and read one `RuntimeInfo`. No scan, no GC,
    /// no network. Twenty warm samples so the numbers mean something.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn healthy_attach_probe_is_local_and_fast() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("daemon.sock");
        let mut bound = bind_daemon_transports(
            &sock,
            None,
            None,
            test_service(),
            leveler_local_transport::LocalWaiters::new(),
        )
        .await
        .expect("server binds");
        let unix = bound.unix.take().expect("unix listener");
        let server_shutdown = tokio_util::sync::CancellationToken::new();
        let server_task = tokio::spawn(unix.serve(server_shutdown.clone()));

        // Warm up: the first connect creates the transport.
        let warm = connect_default_runtime(&sock).await.unwrap();
        assert!(warm.is_some(), "the bound socket must answer");
        drop(warm);

        let mut samples = Vec::new();
        for _ in 0..20 {
            let start = std::time::Instant::now();
            let client = connect_default_runtime(&sock)
                .await
                .expect("probe")
                .expect("connected");
            // The real healthy path then reads one RuntimeInfo. Bounded so a
            // non-answering test service can never hang this measurement.
            let _ = tokio::time::timeout(
                Duration::from_secs(2),
                leveler_local_transport::LocalRuntimeService::runtime_info(&client),
            )
            .await;
            samples.push(start.elapsed());
            drop(client);
        }
        samples.sort();
        let median = samples[samples.len() / 2];
        let p95 = samples[(samples.len() * 95 / 100).min(samples.len() - 1)];
        let max = *samples.last().unwrap();
        eprintln!(
            "healthy attach probe: median={median:?} p95={p95:?} max={max:?} (20 warm samples, local socket only)"
        );
        assert!(
            median < Duration::from_millis(100),
            "healthy attach median {median:?} exceeds 100ms"
        );
        assert!(
            p95 < Duration::from_millis(200),
            "healthy attach p95 {p95:?} exceeds 200ms"
        );

        server_shutdown.cancel();
        let _ = server_task.await;
    }

    #[tokio::test]
    async fn second_daemon_on_the_same_socket_fails_fast() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("daemon.sock");
        let _first = bind_daemon_transports(
            &sock,
            None,
            None,
            test_service(),
            leveler_local_transport::LocalWaiters::new(),
        )
        .await
        .expect("first daemon binds");
        let second = bind_daemon_transports(
            &sock,
            Some("127.0.0.1:0".parse().unwrap()),
            None,
            test_service(),
            leveler_local_transport::LocalWaiters::new(),
        )
        .await;
        assert!(
            second.is_err(),
            "同一仓库上的第二个 daemon 必须 bind 失败（否则会把第一个的活跃 turn 当僵尸 reap）"
        );
    }
}

#[cfg(test)]
mod resume_hint_tests {
    use super::format_resumable_hint;

    fn row(id: &str, status: &str, goal: &str) -> (String, String, String) {
        (id.to_string(), status.to_string(), goal.to_string())
    }

    /// R6-P5: an interrupted goal must be visible BEFORE another session is
    /// silently created, and the notice must carry the id the user needs.
    #[test]
    fn unfinished_work_is_announced_with_the_command_to_resume() {
        let hint = format_resumable_hint(&[row("sess-1", "incomplete", "fix inherited auth")])
            .expect("an incomplete session must produce a hint");
        assert!(hint.contains("sess-1"), "{hint}");
        assert!(hint.contains("leveler --session sess-1"), "{hint}");
        assert!(hint.contains("fix inherited auth"), "{hint}");
    }

    /// Finished work is not unfinished work — no nagging.
    #[test]
    fn completed_sessions_produce_no_hint() {
        assert!(format_resumable_hint(&[row("s", "completed", "done")]).is_none());
        assert!(format_resumable_hint(&[]).is_none());
    }

    /// A blocked session needs attention and is resumable, so it counts.
    #[test]
    fn blocked_sessions_count_as_resumable() {
        assert!(format_resumable_hint(&[row("s", "blocked", "needs input")]).is_some());
    }

    /// Several resumable sessions: name the newest, point at the list for the
    /// rest rather than printing a wall of ids at launch.
    #[test]
    fn extra_sessions_are_summarised_not_listed() {
        let hint = format_resumable_hint(&[
            row("newest", "incomplete", "goal a"),
            row("older", "incomplete", "goal b"),
            row("oldest", "blocked", "goal c"),
        ])
        .unwrap();
        assert!(hint.contains("newest"), "{hint}");
        assert!(hint.contains("+2 more"), "{hint}");
        assert!(!hint.contains("oldest"), "must not list every id: {hint}");
    }

    /// A long multi-line goal must not flood the launch line.
    #[test]
    fn long_goals_are_truncated_to_one_line() {
        let goal = "x".repeat(200) + "\nsecond line";
        let hint = format_resumable_hint(&[row("s", "incomplete", &goal)]).unwrap();
        assert!(hint.contains('…'), "{hint}");
        assert!(!hint.contains("second line"), "{hint}");
        assert!(hint.lines().count() <= 2, "{hint}");
    }
}

#[cfg(all(test, unix))]
mod runtime_consistency_tests {
    use super::{RuntimeConsistency, classify_runtime, classify_runtime_generation};
    use leveler_client_protocol::{RuntimeHealth, RuntimeInfo};
    use leveler_core::{BuildIdentity, RuntimeId};

    fn build(version: &str, revision: &str, dirty: bool) -> BuildIdentity {
        BuildIdentity {
            version: version.into(),
            revision: revision.into(),
            dirty,
            fingerprint: String::new(),
        }
    }

    fn with_fingerprint(revision: &str, dirty: bool, fingerprint: &str) -> BuildIdentity {
        BuildIdentity {
            version: "0.2.0-beta.1".into(),
            revision: revision.into(),
            dirty,
            fingerprint: fingerprint.into(),
        }
    }

    fn clean(revision: &str) -> BuildIdentity {
        build("0.2.0-beta.1", revision, false)
    }

    fn runtime(build: BuildIdentity, fingerprint: Option<&str>) -> RuntimeInfo {
        RuntimeInfo {
            runtime_id: RuntimeId::new("runtime"),
            version: build.version.clone(),
            build,
            config_fingerprint: fingerprint.map(str::to_string),
            pid: 1,
            health: RuntimeHealth::default(),
        }
    }

    #[test]
    fn the_same_build_is_reused() {
        let me = clean("abc123");
        assert!(matches!(
            classify_runtime(Some(&me.clone()), &me),
            RuntimeConsistency::Current
        ));
    }

    #[test]
    fn the_same_build_with_a_different_config_is_replaced() {
        let me = clean("abc123");
        let reported = runtime(me.clone(), Some("sha256:old"));
        assert!(matches!(
            classify_runtime_generation(Some(&reported), &me, "sha256:new"),
            RuntimeConsistency::ConfigChanged
        ));
    }

    #[test]
    fn a_runtime_without_a_config_generation_is_unknown() {
        let me = clean("abc123");
        let reported = runtime(me.clone(), None);
        assert!(matches!(
            classify_runtime_generation(Some(&reported), &me, "sha256:new"),
            RuntimeConsistency::Unknown
        ));
    }

    /// THE incident, as a decision: two builds calling themselves
    /// `0.2.0-beta.1` are not thereby the same build, and the one that has
    /// been running since yesterday is the one that must go.
    #[test]
    fn same_version_different_revision_is_outdated() {
        let expected = clean("new111");
        match classify_runtime(Some(&clean("old999")), &expected) {
            RuntimeConsistency::Outdated { runtime, .. } => {
                assert_eq!(runtime.revision, "old999");
            }
            other => panic!("a different build must be Outdated, got {other:?}"),
        }
    }

    /// A daemon old enough to predate the handshake reports nothing. Nothing
    /// is Unknown — and Unknown is never replaced on a guess, because a
    /// runtime we cannot reason about may be holding live work.
    #[test]
    fn a_runtime_that_reports_nothing_is_unknown() {
        assert!(matches!(
            classify_runtime(None, &clean("abc123")),
            RuntimeConsistency::Unknown
        ));
        assert!(matches!(
            classify_runtime(Some(&BuildIdentity::default()), &clean("abc123")),
            RuntimeConsistency::Unknown
        ));
        assert!(matches!(
            classify_runtime(
                Some(&build("0.2.0-beta.1", "unknown", false)),
                &clean("abc")
            ),
            RuntimeConsistency::Unknown
        ));
    }

    /// The dirty matrix for a LEGACY runtime with no fingerprint: a modified
    /// tree is not identified by the commit it was modified from, so it
    /// matches nothing.
    #[test]
    fn dirty_builds_are_outdated_in_every_direction() {
        let dirty = build("0.2.0-beta.1", "abc123", true);
        let clean_same = clean("abc123");
        for (reported, expected) in [
            (&dirty, &clean_same),
            (&clean_same, &dirty),
            (&dirty, &dirty),
        ] {
            assert!(
                matches!(
                    classify_runtime(Some(reported), expected),
                    RuntimeConsistency::Outdated { .. }
                ),
                "a fingerprint-less dirty build must never be reused: {reported:?} vs {expected:?}"
            );
        }
    }

    /// A dirty development build recognizes its OWN artifact: the same
    /// executable launched twice is one generation, whatever uncommitted work
    /// it was built from.
    #[test]
    fn a_dirty_build_with_its_own_fingerprint_is_current() {
        let me = with_fingerprint("abc123", true, "abc123-deadbeef");
        let same_artifact = with_fingerprint("abc123", true, "abc123-deadbeef");
        assert!(matches!(
            classify_runtime(Some(&same_artifact), &me),
            RuntimeConsistency::Current
        ));
    }

    /// A rebuild from different source is a new generation and still triggers
    /// the handover.
    #[test]
    fn a_rebuilt_dirty_build_is_outdated() {
        let me = with_fingerprint("abc123", true, "abc123-deadbeef");
        let rebuilt = with_fingerprint("abc123", true, "abc123-0badcafe");
        assert!(matches!(
            classify_runtime(Some(&rebuilt), &me),
            RuntimeConsistency::Outdated { .. }
        ));
    }
}

#[cfg(all(test, unix))]
mod replacement_verification_tests {
    use super::verify_replacement;
    use leveler_core::BuildIdentity;

    fn build(revision: &str, dirty: bool) -> BuildIdentity {
        BuildIdentity {
            version: "0.2.0-beta.1".into(),
            revision: revision.into(),
            dirty,
            fingerprint: String::new(),
        }
    }

    #[test]
    fn a_replacement_that_is_the_expected_build_is_accepted() {
        let expected = build("newbuild", false);
        assert!(verify_replacement(Some(&expected.clone()), &expected).is_ok());
    }

    /// Starting a process is not replacing a runtime. If what came up is some
    /// third build, the bootstrap fails — and it fails once, since relaunching
    /// a wrong build repeatedly only burns the machine.
    #[test]
    fn a_replacement_of_the_wrong_build_is_rejected_once() {
        let err = verify_replacement(
            Some(&build("someothersha", false)),
            &build("expected", false),
        )
        .expect_err("a different build is not a replacement");
        let message = err.to_string();
        assert!(message.contains("not replaced correctly"), "{message}");
        assert!(
            message.contains("someothersha") && message.contains("expected"),
            "the failure names both builds so it can be acted on: {message}"
        );
    }

    /// A replacement that came up but says nothing about itself cannot be
    /// verified, so it is not accepted either — silence is not proof.
    #[test]
    fn a_replacement_that_reports_no_identity_is_rejected() {
        assert!(verify_replacement(None, &build("expected", false)).is_err());
        assert!(
            verify_replacement(Some(&BuildIdentity::default()), &build("expected", false)).is_err()
        );
    }

    /// A dirty replacement is not the clean build that was expected.
    #[test]
    fn a_dirty_replacement_is_rejected() {
        assert!(verify_replacement(Some(&build("same", true)), &build("same", false)).is_err());
    }

    /// A developer's build is dirty, and the replacement was spawned from this
    /// process's own executable. Refusing it because "two dirty trees may
    /// differ" refuses the binary we just launched ourselves: the TUI then
    /// never starts on any modified tree, and says so with a message whose two
    /// build names are the same string.
    #[test]
    fn a_dirty_replacement_of_this_very_build_is_accepted() {
        let me = build("same", true);
        assert!(
            verify_replacement(Some(&me.clone()), &me).is_ok(),
            "the runtime spawned from our own exe reports our own identity"
        );
    }
}

/// MA4-C ablation seam, EVAL ONLY: `LEVELER_EVAL_PARENT_REASONING_EFFORT`
/// lowers the reasoning effort of the top-level `leveler run` seat while
/// delegated children keep the model default. Unset leaves the run untouched;
/// an unknown level is refused rather than silently ignored.
const PARENT_REASONING_ENV: &str = "LEVELER_EVAL_PARENT_REASONING_EFFORT";

fn parent_reasoning_override(
    raw: Option<String>,
) -> anyhow::Result<Option<leveler_agent::coding::ExecutionOverrides>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let effort = leveler_model::ReasoningEffort::parse(&raw).ok_or_else(|| {
        anyhow::anyhow!("{PARENT_REASONING_ENV}: unknown reasoning effort {raw:?}")
    })?;
    Ok(Some(leveler_agent::coding::ExecutionOverrides {
        main_reasoning_effort: Some(effort),
        ..Default::default()
    }))
}

/// MA-PE ablation seam, EVAL ONLY: `LEVELER_EVAL_POST_EDIT_ACTION_THROUGHPUT`
/// adds the generic independent-action guidance to the top-level run. Its value
/// selects WHEN the guidance becomes visible: `1`/`true`/`on`/`post_edit`
/// activates it only after the run's first effective mutation (the experiment
/// arm); `always` keeps it in the system prompt from round 1 (debug only).
/// Unset leaves the run untouched; a value other than the accepted switches is
/// refused rather than silently ignored.
const POST_EDIT_THROUGHPUT_ENV: &str = "LEVELER_EVAL_POST_EDIT_ACTION_THROUGHPUT";

fn post_edit_action_throughput_override(
    raw: Option<String>,
) -> anyhow::Result<Option<leveler_agent::coding::ExecutionOverrides>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let mode = match raw.as_str() {
        "1" | "true" | "on" | "post_edit" => {
            leveler_agent::coding::PostEditThroughputMode::PostEdit
        }
        "always" => leveler_agent::coding::PostEditThroughputMode::Always,
        other => anyhow::bail!(
            "{POST_EDIT_THROUGHPUT_ENV}: expected 1/true/on/post_edit/always, got {other:?}"
        ),
    };
    Ok(Some(leveler_agent::coding::ExecutionOverrides {
        post_edit_action_throughput: Some(mode),
        ..Default::default()
    }))
}

/// Reasoning-retention ablation seam, EVAL ONLY:
/// `LEVELER_EVAL_REASONING_RETENTION` projects historical assistant
/// `Reasoning` parts out of the provider request without touching the durable
/// transcript. Accepted values: `all` (explicit baseline), `none`, and
/// `last_<N>` (keep the N most recent reasoning-bearing assistant turns).
/// Unset leaves production behaviour (`All`) untouched; an unknown value is
/// refused rather than silently ignored.
const REASONING_RETENTION_ENV: &str = "LEVELER_EVAL_REASONING_RETENTION";

fn reasoning_retention_override(
    raw: Option<String>,
) -> anyhow::Result<Option<leveler_agent::coding::ExecutionOverrides>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let policy = match raw.as_str() {
        "all" => leveler_agent::coding::ReasoningRetention::All,
        "none" => leveler_agent::coding::ReasoningRetention::None,
        other => match other.strip_prefix("last_") {
            Some(n) => {
                let keep: usize = n.parse().map_err(|_| {
                    anyhow::anyhow!(
                        "{REASONING_RETENTION_ENV}: expected all/none/last_<N>, got {other:?}"
                    )
                })?;
                leveler_agent::coding::ReasoningRetention::LastTurns(keep)
            }
            None => anyhow::bail!(
                "{REASONING_RETENTION_ENV}: expected all/none/last_<N>, got {other:?}"
            ),
        },
    };
    Ok(Some(leveler_agent::coding::ExecutionOverrides {
        reasoning_retention: Some(policy),
        ..Default::default()
    }))
}

/// Read every eval-only override seam and fold them into ONE override set for
/// `with_execution_overrides` (which replaces, so the seams must be merged
/// here rather than applied one after another). `None` when no seam is set.
fn eval_env_overrides() -> anyhow::Result<Option<leveler_agent::coding::ExecutionOverrides>> {
    let mut merged = leveler_agent::coding::ExecutionOverrides::default();
    let mut any = false;
    if let Some(o) = parent_reasoning_override(std::env::var(PARENT_REASONING_ENV).ok())? {
        merged.main_reasoning_effort = o.main_reasoning_effort;
        any = true;
    }
    if let Some(o) =
        post_edit_action_throughput_override(std::env::var(POST_EDIT_THROUGHPUT_ENV).ok())?
    {
        merged.post_edit_action_throughput = o.post_edit_action_throughput;
        any = true;
    }
    if let Some(o) = reasoning_retention_override(std::env::var(REASONING_RETENTION_ENV).ok())? {
        merged.reasoning_retention = o.reasoning_retention;
        any = true;
    }
    Ok(any.then_some(merged))
}

#[cfg(test)]
mod parent_reasoning_tests {
    use super::{
        parent_reasoning_override, post_edit_action_throughput_override,
        reasoning_retention_override,
    };
    use leveler_model::ReasoningEffort;

    #[test]
    fn unset_leaves_the_run_without_overrides() {
        assert!(parent_reasoning_override(None).unwrap().is_none());
    }

    #[test]
    fn a_level_sets_only_the_top_level_seat() {
        let o = parent_reasoning_override(Some("high".into()))
            .unwrap()
            .expect("override");
        assert_eq!(o.main_reasoning_effort, Some(ReasoningEffort::High));
        assert_eq!(
            o,
            leveler_agent::coding::ExecutionOverrides {
                main_reasoning_effort: Some(ReasoningEffort::High),
                ..Default::default()
            }
        );
    }

    #[test]
    fn an_unknown_level_is_refused() {
        assert!(parent_reasoning_override(Some("hihg".into())).is_err());
    }

    #[test]
    fn the_post_edit_knob_is_unset_unless_the_eval_seam_asks_for_it() {
        assert!(
            post_edit_action_throughput_override(None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn the_post_edit_knob_flips_only_that_input() {
        for raw in ["1", "true", "on", "post_edit"] {
            let o = post_edit_action_throughput_override(Some(raw.into()))
                .unwrap()
                .expect("override");
            assert_eq!(
                o.post_edit_action_throughput,
                Some(leveler_agent::coding::PostEditThroughputMode::PostEdit)
            );
            assert_eq!(
                o,
                leveler_agent::coding::ExecutionOverrides {
                    post_edit_action_throughput: Some(
                        leveler_agent::coding::PostEditThroughputMode::PostEdit
                    ),
                    ..Default::default()
                },
                "only the post-edit knob may change"
            );
        }
        let always = post_edit_action_throughput_override(Some("always".into()))
            .unwrap()
            .expect("override");
        assert_eq!(
            always.post_edit_action_throughput,
            Some(leveler_agent::coding::PostEditThroughputMode::Always)
        );
    }

    #[test]
    fn an_unknown_post_edit_value_is_refused() {
        let err = post_edit_action_throughput_override(Some("maybe".into())).unwrap_err();
        assert!(
            err.to_string()
                .contains("expected 1/true/on/post_edit/always"),
            "{err}"
        );
    }

    #[test]
    fn the_reasoning_retention_seam_is_unset_unless_asked_for() {
        assert!(reasoning_retention_override(None).unwrap().is_none());
    }

    #[test]
    fn each_reasoning_retention_arm_flips_only_that_input() {
        use leveler_agent::coding::{ExecutionOverrides, ReasoningRetention};
        for (raw, expected) in [
            ("all", ReasoningRetention::All),
            ("none", ReasoningRetention::None),
            ("last_3", ReasoningRetention::LastTurns(3)),
            ("last_0", ReasoningRetention::LastTurns(0)),
        ] {
            let o = reasoning_retention_override(Some(raw.into()))
                .unwrap()
                .expect("override");
            assert_eq!(o.reasoning_retention, Some(expected), "{raw}");
            assert_eq!(
                o,
                ExecutionOverrides {
                    reasoning_retention: Some(expected),
                    ..Default::default()
                },
                "only the retention knob may change ({raw})"
            );
        }
    }

    #[test]
    fn an_unknown_reasoning_retention_value_is_refused() {
        for raw in ["last_three", "last_", "keep3", ""] {
            assert!(
                reasoning_retention_override(Some(raw.into())).is_err(),
                "{raw:?} must be refused"
            );
        }
    }
}

#[cfg(all(test, unix))]
mod handoff_status_tests {
    use super::*;
    use leveler_client_protocol::{RuntimeHealth, UiBackgroundTaskBlocker};

    fn blocker(
        task_id: &str,
        program: &str,
        args: &[&str],
        elapsed_ms: u64,
    ) -> UiBackgroundTaskBlocker {
        UiBackgroundTaskBlocker {
            task_id: task_id.to_string(),
            program: program.to_string(),
            args: args.iter().map(|arg| arg.to_string()).collect(),
            elapsed_ms,
            session_id: None,
            log_tail: String::new(),
        }
    }

    /// PV1/PV2 — the version-handover copy is bilingual product text, and the
    /// user never sees the internal word "runtime".
    #[test]
    fn the_handover_copy_is_bilingual_and_never_says_runtime() {
        let health = RuntimeHealth {
            active_background_tasks: 1,
            blockers: vec![blocker("bg-1", "make", &["up"], 1_080_000)],
            ..Default::default()
        };
        let zh = retiring_status_lang(&health, HandoffLang::Zh);
        assert!(zh.contains("旧版本 CodeLeveler"), "{zh}");
        assert!(zh.contains("后台任务"), "{zh}");
        assert!(!zh.contains("旧运行时"), "{zh}");

        let en = retiring_status_lang(&health, HandoffLang::En);
        assert!(en.contains("previous CodeLeveler version"), "{en}");
        assert!(!en.contains("previous runtime"), "{en}");
        assert!(!en.contains("quiescent"), "{en}");
    }

    /// PV3/PV4 — a known handover reason must not leak a raw build hash, and an
    /// unknown version must not invent one. The status line carries counts and
    /// user wording only.
    #[test]
    fn the_handover_status_has_no_raw_build_hash() {
        let health = RuntimeHealth {
            quiescent: true,
            ..Default::default()
        };
        let en = retiring_status_lang(&health, HandoffLang::En);
        assert!(!en.contains("fingerprint"), "{en}");
        assert!(!en.contains("pgid"), "{en}");
    }

    #[test]
    fn an_idle_runtime_shows_no_blocker_list() {
        let health = RuntimeHealth {
            quiescent: true,
            ..Default::default()
        };
        let text = retiring_status_lang(&health, HandoffLang::En);
        assert!(text.contains("exiting"), "{text}");
        assert!(!text.contains("running tasks"), "{text}");
    }

    #[test]
    fn a_blocked_handover_names_the_command_age_and_stop_path() {
        let health = RuntimeHealth {
            active_background_tasks: 1,
            blockers: vec![blocker("bg-1", "make", &["up"], 90_061_000)],
            ..Default::default()
        };
        let text = retiring_status_lang(&health, HandoffLang::En);
        assert!(text.contains("finishing existing work"), "{text}");
        assert!(
            text.contains("previous CodeLeveler version still has background tasks"),
            "{text}"
        );
        assert!(text.contains("bg-1"), "{text}");
        assert!(text.contains("make up"), "{text}");
        assert!(text.contains("1d 01h"), "{text}");
        assert!(text.contains("leveler background stop"), "{text}");
    }

    #[test]
    fn every_live_blocker_is_listed_oldest_first() {
        let health = RuntimeHealth {
            active_background_tasks: 3,
            blockers: vec![
                blocker("bg-1", "make", &["up"], 90_061_000),
                blocker("bg-4", "pnpm", &["dev"], 1_080_000),
                blocker("bg-7", "cargo", &["watch"], 4_000),
            ],
            ..Default::default()
        };
        let text = retiring_status_lang(&health, HandoffLang::En);
        let first = text.find("bg-1").expect("bg-1 named");
        let second = text.find("bg-4").expect("bg-4 named");
        let third = text.find("bg-7").expect("bg-7 named");
        assert!(first < second && second < third, "{text}");
    }

    #[test]
    fn task_ages_render_compactly() {
        assert_eq!(format_task_age(90_061_000), "1d 01h");
        assert_eq!(format_task_age(1_080_000), "18m 00s");
        assert_eq!(format_task_age(4_000), "4s");
    }
}

#[cfg(all(test, unix))]
mod handoff_key_tests {
    use super::*;
    use leveler_client_protocol::{RuntimeHealth, UiBackgroundTaskBlocker};

    fn blocker(task_id: &str, elapsed_ms: u64) -> UiBackgroundTaskBlocker {
        UiBackgroundTaskBlocker {
            task_id: task_id.to_string(),
            program: "make".to_string(),
            args: vec!["up".to_string()],
            elapsed_ms,
            session_id: None,
            log_tail: String::new(),
        }
    }

    /// A stable blocker's growing age must not restate the handoff block.
    #[test]
    fn a_growing_age_does_not_change_the_key() {
        let a = RuntimeHealth {
            active_background_tasks: 1,
            blockers: vec![blocker("bg-1", 1_000)],
            ..Default::default()
        };
        let b = RuntimeHealth {
            active_background_tasks: 1,
            blockers: vec![blocker("bg-1", 90_000)],
            ..Default::default()
        };
        assert_eq!(handoff_key(&a), handoff_key(&b));
    }

    #[test]
    fn a_changed_blocker_set_changes_the_key() {
        let a = RuntimeHealth {
            active_background_tasks: 1,
            blockers: vec![blocker("bg-1", 1_000)],
            ..Default::default()
        };
        let none = RuntimeHealth::default();
        assert_ne!(handoff_key(&a), handoff_key(&none));
    }
}

#[cfg(all(test, unix))]
mod handover_recovery_tests {
    use super::*;
    use leveler_client_protocol::{RuntimeHealth, UiTurnBlocker};

    fn turn(session: &str, elapsed_ms: u64, idle_ms: u64) -> UiTurnBlocker {
        UiTurnBlocker {
            session_id: leveler_core::SessionId::new(session),
            elapsed_ms,
            idle_ms,
        }
    }

    fn stale_ms() -> u64 {
        leveler_client_protocol::STALE_TURN_WARN_AFTER.as_millis() as u64
    }

    /// A) A turn that keeps making observable progress is never labelled
    /// stalled, however long it has run: only idle time counts.
    #[test]
    fn a_long_but_active_turn_is_not_stalled() {
        let health = RuntimeHealth {
            active_turns: 1,
            turn_blockers: vec![turn("s1", 4 * 60 * 60 * 1_000, 1_000)],
            ..Default::default()
        };
        assert!(stalled_turn_sessions(&health).is_empty());
        assert_eq!(
            turn_status_label(&health.turn_blockers[0], HandoffLang::En),
            "running"
        );
    }

    /// B) Crossing the no-progress threshold changes the description only —
    /// and nothing acts on its own. Silence is not an interrupt request.
    #[test]
    fn a_quiet_turn_is_described_but_not_acted_on() {
        let health = RuntimeHealth {
            active_turns: 1,
            turn_blockers: vec![turn("s1", 6 * 24 * 3_600_000, stale_ms() + 1)],
            ..Default::default()
        };
        assert_eq!(stalled_turn_sessions(&health).len(), 1);
        assert_eq!(
            turn_status_label(&health.turn_blockers[0], HandoffLang::En),
            "no observable progress"
        );
        // No implicit action: only an explicit key ever interrupts or forces.
        for line in ["", " ", "\n", "y", "yes", "kill", "stop"] {
            assert_eq!(parse_handover_input(line), None, "{line:?}");
        }
    }

    /// The recovery keys are explicit and case-insensitive; nothing else is
    /// accepted, so a stray keystroke cannot discard work.
    #[test]
    fn only_explicit_keys_interrupt_or_force() {
        for line in ["i", "I", "interrupt", " Interrupt "] {
            assert_eq!(parse_handover_input(line), Some(HandoverInput::Interrupt));
        }
        for line in ["f", "F", "force", " Force "] {
            assert_eq!(parse_handover_input(line), Some(HandoverInput::Force));
        }
        assert_eq!(parse_handover_input("w"), None);
        assert_eq!(parse_handover_input("wait"), None);
    }

    /// H) Force is refused while real background work would be destroyed; it
    /// becomes available only once the user has settled it.
    #[test]
    fn force_is_refused_while_background_work_runs() {
        let busy = RuntimeHealth {
            active_turns: 1,
            active_background_tasks: 1,
            ..Default::default()
        };
        assert!(!force_handover_allowed(&busy));

        let settled = RuntimeHealth {
            active_turns: 1,
            active_background_tasks: 0,
            ..Default::default()
        };
        assert!(force_handover_allowed(&settled));
    }

    /// The key restates once when a turn crosses the threshold, but not on
    /// every poll as its age grows within the same state.
    #[test]
    fn the_key_restates_only_when_the_stall_flag_changes() {
        let below = RuntimeHealth {
            active_turns: 1,
            turn_blockers: vec![turn("s1", 10_000, stale_ms() - 1)],
            ..Default::default()
        };
        let at = RuntimeHealth {
            active_turns: 1,
            turn_blockers: vec![turn("s1", 20_000, stale_ms() + 1)],
            ..Default::default()
        };
        assert_ne!(handoff_key(&below), handoff_key(&at));

        let later = RuntimeHealth {
            active_turns: 1,
            turn_blockers: vec![turn("s1", 90_000, stale_ms() + 60_000)],
            ..Default::default()
        };
        assert_eq!(handoff_key(&at), handoff_key(&later));
    }

    /// The handover line names the session, its age, its last activity, and
    /// whether it shows progress — no pid, generation or fingerprint.
    #[test]
    fn the_turn_blocker_line_is_specific_and_leaks_nothing_internal() {
        let health = RuntimeHealth {
            active_turns: 1,
            turn_blockers: vec![turn("sess-42", 90_061_000, stale_ms() + 1)],
            ..Default::default()
        };
        let en = retiring_status_lang(&health, HandoffLang::En);
        assert!(en.contains("sess-42"), "{en}");
        assert!(en.contains("elapsed"), "{en}");
        assert!(en.contains("1d 01h"), "{en}");
        assert!(en.contains("no observable progress"), "{en}");
        assert!(!en.contains("pgid"), "{en}");
        assert!(!en.contains("fingerprint"), "{en}");
        assert!(!en.contains("generation"), "{en}");
    }

    /// A live turn without stalls adds no recovery prompt to the wait.
    #[test]
    fn a_healthy_handover_offers_no_recovery_prompt() {
        let health = RuntimeHealth {
            active_turns: 1,
            turn_blockers: vec![turn("s1", 5_000, 1_000)],
            ..Default::default()
        };
        assert!(stalled_turn_sessions(&health).is_empty());
    }
}

#[cfg(test)]
mod outcome_exit_code_tests {
    use super::{NOT_COMPLETED_EXIT_CODE, outcome_exit_code};
    use leveler_agent::StopReason;
    use leveler_app::CollaborationExecution;

    fn code(reason: StopReason, execution: CollaborationExecution) -> std::process::ExitCode {
        outcome_exit_code(reason, execution)
    }

    /// The one contract an external caller depends on: an honest refusal is
    /// never reported with the generic failure code, so it cannot be confused
    /// with a crash.
    #[test]
    fn honest_non_completion_is_distinct_from_failure() {
        for reason in [
            StopReason::Blocked,
            StopReason::Stalled,
            StopReason::Incomplete,
            StopReason::BudgetExhausted,
            StopReason::TurnLimitReached,
        ] {
            for execution in [
                CollaborationExecution::Goal,
                CollaborationExecution::Chat,
                CollaborationExecution::Plan,
            ] {
                assert_eq!(
                    code(reason, execution),
                    std::process::ExitCode::from(NOT_COMPLETED_EXIT_CODE),
                    "{reason:?} / {execution:?}"
                );
                assert_ne!(
                    code(reason, execution),
                    std::process::ExitCode::FAILURE,
                    "{reason:?} / {execution:?} must not look like a crash"
                );
            }
        }
    }

    /// Goal succeeds only on a declared completion; an answer is unresolved.
    #[test]
    fn only_completed_is_success_for_goal() {
        assert_eq!(
            code(StopReason::Completed, CollaborationExecution::Goal),
            std::process::ExitCode::SUCCESS
        );
        assert_eq!(
            code(StopReason::Answered, CollaborationExecution::Goal),
            std::process::ExitCode::from(NOT_COMPLETED_EXIT_CODE),
            "a goal that only answered has not been resolved"
        );
    }

    /// Chat and Plan exist to answer: `Answered` is their own terminal, and a
    /// declared goal completion stays success as well.
    #[test]
    fn answer_is_success_for_chat_and_plan() {
        for execution in [CollaborationExecution::Chat, CollaborationExecution::Plan] {
            assert_eq!(
                code(StopReason::Answered, execution),
                std::process::ExitCode::SUCCESS,
                "{execution:?}"
            );
            assert_eq!(
                code(StopReason::Completed, execution),
                std::process::ExitCode::SUCCESS,
                "{execution:?}"
            );
        }
    }
}

#[cfg(test)]
mod interactive_bootstrap_tests {
    use super::*;

    /// A bare `leveler` / `leveler tui` opens a conversation.
    ///
    /// The axis must not come from `CollaborationMode::default()`: that value
    /// is the product's coding-session default and the wire's omitted-field
    /// contract, and a terminal session may not silently inherit it every time
    /// that default moves.
    #[test]
    fn a_new_interactive_session_is_chat() {
        let request = interactive_session_request(
            None,
            leveler_client_protocol::PermissionProfile::Assisted,
            false,
        );
        assert_eq!(
            request.collaboration,
            leveler_local_transport::CollaborationMode::Chat,
            "a new terminal session is a conversation; `/goal <task>` is how a user asks for the goal lifecycle"
        );
        assert_eq!(request.goal, "interactive session");
        assert_eq!(
            request.workspace,
            leveler_local_transport::CreateWorkspaceSelection::RuntimeDefault
        );
        assert_eq!(
            request.approval_policy,
            leveler_client_protocol::ApprovalPolicy::Interactive
        );
        assert_eq!(
            request.mode,
            leveler_client_protocol::PermissionProfile::Assisted
        );
        assert!(request.model.is_none());
    }

    /// The single statement of the interactive axis. Both transports consume
    /// it: the daemon writes it into the wire request, and the embedded runtime
    /// passes it to the create. This test is what keeps a second, hand-written
    /// axis from reappearing on one of the two paths.
    #[test]
    fn both_transports_consume_the_one_interactive_axis() {
        let from_the_daemon_request = interactive_session_request(
            None,
            leveler_client_protocol::PermissionProfile::Assisted,
            false,
        )
        .collaboration;
        assert_eq!(
            from_the_daemon_request,
            interactive_session_collaboration(),
            "the daemon request must state the shared interactive axis, not its own copy"
        );
        assert_eq!(
            interactive_session_collaboration(),
            leveler_local_transport::CollaborationMode::Chat,
            "a new terminal session is a conversation; `/goal <task>` is how a user asks for the goal lifecycle"
        );
    }

    /// The create-time policy and permission overrides move the axis not at
    /// all: they are orthogonal facts about the same session.
    #[test]
    fn terminal_overrides_do_not_change_the_axis() {
        let request = interactive_session_request(
            Some(leveler_client_protocol::ModelRef::parse("deepseek/v3").unwrap()),
            leveler_client_protocol::PermissionProfile::FullAccess,
            true,
        );
        assert_eq!(
            request.collaboration,
            leveler_local_transport::CollaborationMode::Chat
        );
        assert_eq!(
            request.approval_policy,
            leveler_client_protocol::ApprovalPolicy::AutoApprove
        );
        assert_eq!(
            request.mode,
            leveler_client_protocol::PermissionProfile::FullAccess
        );
        assert!(request.model.is_some());
    }

    /// The entry-level fix leaves the product default and the headless path
    /// alone: `leveler run` (and a wire request that omits the field) still
    /// resolve to Goal.
    #[test]
    fn the_product_default_stays_goal() {
        assert_eq!(
            leveler_local_transport::CollaborationMode::default(),
            leveler_local_transport::CollaborationMode::Goal,
            "only the terminal entry point moved, not the coding-session default"
        );
    }
}
