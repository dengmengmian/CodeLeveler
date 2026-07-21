//! Run-style subcommands: run, orchestrated run, parallel run, plan, discuss,
//! tui, and resume, plus their shared finish/ship helpers.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(unix)]
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use leveler_agent::StopReason;
use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::InteractiveRuntimeClient;
use leveler_local_transport::{
    CreateSessionRequest, LocalSocketRuntimeClient, LocalSocketServer, TcpRuntimeServer,
    TransportError,
};
use leveler_project::Layout;

use crate::cli::{OutputFormat, RunMode};
use crate::common::{build_approver, map_mode, resolve_model, spawn_interrupt_handler};
use crate::output::Line;
use crate::render::{emit_jsonl, render_engine_event, render_event, render_orch_event};

#[allow(clippy::too_many_arguments)]
pub(crate) async fn cmd_run(
    layout: Layout,
    task: String,
    model: Option<String>,
    mode: RunMode,
    auto_approve: bool,
    output: OutputFormat,
    ship: leveler_app::ShipOptions,
    sandbox: bool,
    work_profile: leveler_lifecycle::WorkProfile,
    collaboration: leveler_lifecycle::CollaborationMode,
) -> anyhow::Result<std::process::ExitCode> {
    let app = Application::assemble(layout)?
        .with_work_profile(work_profile)
        .with_collaboration(collaboration);
    let model_ref = resolve_model(&app, model)?;
    let execution_mode = map_mode(mode);

    let session_id = app.create_session(&model_ref, &task).await?;

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

    // Ship on success; direct runs with edits are verification-gated by the app.
    if ship.any()
        && output == OutputFormat::Text
        && let Ok(outcome) = &result
        && outcome.stop_reason == StopReason::Completed
        && !outcome.modified_files.is_empty()
    {
        ship_changes_and_print(
            &app,
            &model_ref,
            &task,
            &outcome.modified_files,
            true,
            &ship,
        )
        .await;
    }

    finish(result, &session_id.to_string(), output)
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn cmd_run_orchestrated(
    layout: Layout,
    task: String,
    model: Option<String>,
    mode: RunMode,
    auto_approve: bool,
    ship: leveler_app::ShipOptions,
    sandbox: bool,
    work_profile: leveler_lifecycle::WorkProfile,
) -> anyhow::Result<std::process::ExitCode> {
    let app = Application::assemble(layout)?.with_work_profile(work_profile);
    let model_ref = resolve_model(&app, model)?;
    let execution_mode = map_mode(mode);

    println!(
        "{}",
        Line::heading(&format!("Orchestrating task with {model_ref}"))
    );
    println!("  mode: {execution_mode:?}");
    println!("  task: {task}\n");

    let approver = build_approver(auto_approve);
    let cancellation = CancellationToken::new();
    spawn_interrupt_handler(cancellation.clone());

    let (session_id, outcome) = app
        .orchestrate_task(
            &model_ref,
            execution_mode,
            &task,
            approver,
            Arc::new(leveler_agent::AutoClarify),
            sandbox,
            &mut render_engine_event,
            cancellation,
            None,
        )
        .await?;

    println!();
    if let Some(report) = &outcome.verification {
        println!("{}", Line::heading("Verification"));
        for check in &report.checks {
            let mark = match check.status {
                leveler_verifier::CheckStatus::Passed => console::style("✓").green(),
                leveler_verifier::CheckStatus::Failed => console::style("✗").red(),
                leveler_verifier::CheckStatus::Skipped => console::style("–").dim(),
                leveler_verifier::CheckStatus::ToolMissing => console::style("?").yellow(),
            };
            println!("  {mark} {} ({:?})", check.name, check.status);
        }
        if !report.scope_ok {
            println!(
                "  {} out-of-scope edits: {}",
                console::style("✗").red(),
                report.scope_violations.join(", ")
            );
        }
        // Show evidence for failed gates.
        for check in report.failed_gates() {
            println!(
                "\n  {} evidence for {}:",
                console::style("↳").red(),
                check.name
            );
            for line in check.evidence.lines().take(20) {
                println!("    {}", console::style(line).dim());
            }
        }
        println!();
    }
    if let Some(findings) = &outcome.review
        && !findings.is_empty()
    {
        println!("{}", Line::heading("Review findings"));
        for f in findings {
            println!(
                "  [{:?}/{}] {}{}",
                f.severity,
                f.lens,
                f.file
                    .as_deref()
                    .map(|p| format!("{p}: "))
                    .unwrap_or_default(),
                f.issue
            );
        }
        println!();
    }
    if let Some(ledger) = &outcome.acceptance
        && !ledger.items.is_empty()
    {
        println!("{}", Line::heading("Acceptance criteria"));
        for item in &ledger.items {
            let (mark, style): (&str, _) = match item.status {
                leveler_verifier::AcceptanceStatus::Met => ("✓", console::Style::new().green()),
                leveler_verifier::AcceptanceStatus::Unmet => ("✗", console::Style::new().red()),
                leveler_verifier::AcceptanceStatus::Unverifiable => {
                    ("–", console::Style::new().dim())
                }
            };
            let req = if item.required {
                "required"
            } else {
                "optional"
            };
            println!(
                "  {} [{}] {} ({req})",
                style.apply_to(mark),
                item.id,
                item.description
            );
        }
        println!();
    }
    if !outcome.modified_files.is_empty() {
        println!("{}", Line::heading("Modified files"));
        for f in &outcome.modified_files {
            println!("  {f}");
        }
        println!();
    }
    println!("  session: {session_id}");

    let verdict = outcome
        .verification
        .as_ref()
        .map(leveler_verifier::VerificationReport::verdict);

    let completed = outcome.outcome.is_success();
    if completed && ship.any() && !outcome.modified_files.is_empty() {
        let verified = verdict == Some(leveler_verifier::Verdict::Verified);
        ship_changes_and_print(
            &app,
            &model_ref,
            &task,
            &outcome.modified_files,
            verified,
            &ship,
        )
        .await;
    }

    match completed {
        true => {
            match verdict {
                Some(leveler_verifier::Verdict::Verified) => {
                    println!("{}", Line::ok("Task completed and verified."));
                }
                Some(leveler_verifier::Verdict::Unverified(reason)) => {
                    println!(
                        "{}",
                        Line::warn(&format!("Task completed, but unverified: {reason}"))
                    );
                }
                // Complete with a failed verdict cannot happen (the state
                // machine transitions to Failed), but never claim success.
                Some(leveler_verifier::Verdict::Failed) => {
                    println!("{}", Line::warn("Task completed, but verification failed."));
                }
                None => {
                    println!(
                        "{}",
                        Line::warn("Task completed, but unverified: verification did not run.")
                    );
                }
            }
            Ok(std::process::ExitCode::SUCCESS)
        }
        false => {
            println!(
                "{}",
                Line::warn(&format!(
                    "Ended {:?} (verification not satisfied).",
                    outcome.outcome
                ))
            );
            Ok(std::process::ExitCode::FAILURE)
        }
    }
}

pub(crate) async fn cmd_run_parallel(
    layout: Layout,
    task: String,
    model: Option<String>,
    mode: RunMode,
    parallel: usize,
) -> anyhow::Result<std::process::ExitCode> {
    let app = Application::assemble(layout)?;
    let model_ref = resolve_model(&app, model)?;
    let execution_mode = map_mode(mode);

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

pub(crate) async fn cmd_plan(
    layout: Layout,
    task: String,
    model: Option<String>,
) -> anyhow::Result<std::process::ExitCode> {
    let app = Application::assemble(layout)?;
    let model_ref = resolve_model(&app, model)?;

    println!("{}", Line::heading(&format!("Planning with {model_ref}")));
    println!("  task: {task}\n");

    let cancellation = CancellationToken::new();
    spawn_interrupt_handler(cancellation.clone());

    let (requirement, graph) = app
        .plan_task(&model_ref, &task, &mut render_orch_event, cancellation)
        .await?;

    println!("\n{}", Line::heading("Requirement"));
    println!("  goal: {}", requirement.goal);
    println!(
        "  type: {:?}   risk: {:?}",
        requirement.task_type, requirement.risk
    );
    if !requirement.constraints.is_empty() {
        println!("  constraints:");
        for c in &requirement.constraints {
            println!("    - {c}");
        }
    }
    if !requirement.acceptance_criteria.is_empty() {
        println!("  acceptance criteria:");
        for ac in &requirement.acceptance_criteria {
            let req = if ac.required { "required" } else { "optional" };
            println!("    - [{}] {} ({req})", ac.id, ac.description);
        }
    }

    println!("\n{}", Line::heading("Plan"));
    for (i, node) in graph.nodes.iter().enumerate() {
        println!("  {}. [{:?}] {}", i + 1, node.kind, node.description);
        if !node.allowed_paths.is_empty() {
            println!("       paths: {}", node.allowed_paths.join(", "));
        }
    }
    Ok(std::process::ExitCode::SUCCESS)
}

pub(crate) async fn cmd_discuss(
    layout: Layout,
    topic: String,
    rounds: u32,
    model: Option<String>,
) -> anyhow::Result<std::process::ExitCode> {
    let app = Application::assemble(layout)?;
    let model_ref = resolve_model(&app, model)?;

    println!(
        "{}",
        Line::heading(&format!("Discussion ({rounds} rounds)"))
    );
    println!("  topic: {topic}\n");

    let cancellation = CancellationToken::new();
    spawn_interrupt_handler(cancellation.clone());

    let outcome = app
        .discuss(
            &model_ref,
            &topic,
            rounds,
            &mut |e| match e {
                leveler_orchestrator::DiscussionEvent::Turn(t) => {
                    println!(
                        "{} {}",
                        console::style(format!("{}:", t.speaker)).cyan().bold(),
                        t.content
                    );
                    println!();
                }
                leveler_orchestrator::DiscussionEvent::Synthesis(_) => {}
            },
            cancellation,
        )
        .await?;

    println!("{}", Line::heading("Synthesis"));
    println!("{}", outcome.synthesis);
    Ok(std::process::ExitCode::SUCCESS)
}

#[cfg(unix)]
const DEFAULT_DAEMON_CONNECT_TIMEOUT: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SocketIntent {
    Embedded,
    ProbeDefault,
    RequireExplicit,
}

fn socket_intent(
    in_process: bool,
    auto_approve: bool,
    explicit_socket: bool,
    config_overridden: bool,
) -> SocketIntent {
    if in_process {
        SocketIntent::Embedded
    } else if explicit_socket {
        SocketIntent::RequireExplicit
    } else if auto_approve || config_overridden {
        // A running daemon cannot inherit these invocation-scoped settings.
        SocketIntent::Embedded
    } else {
        SocketIntent::ProbeDefault
    }
}

#[cfg(unix)]
async fn connect_default_runtime(
    path: &Path,
) -> Result<Option<LocalSocketRuntimeClient>, TransportError> {
    match tokio::time::timeout(
        DEFAULT_DAEMON_CONNECT_TIMEOUT,
        tokio::net::UnixStream::connect(path),
    )
    .await
    {
        Ok(Ok(_probe)) => LocalSocketRuntimeClient::connect(path).await.map(Some),
        Ok(Err(error)) => {
            tracing::debug!(%error, socket = %path.display(), "skipping unavailable local runtime");
            Ok(None)
        }
        Err(_) => {
            tracing::debug!(
                socket = %path.display(),
                timeout_ms = DEFAULT_DAEMON_CONNECT_TIMEOUT.as_millis(),
                "timed out probing local runtime"
            );
            Ok(None)
        }
    }
}

#[cfg(not(unix))]
async fn connect_default_runtime(
    _path: &Path,
) -> Result<Option<LocalSocketRuntimeClient>, TransportError> {
    Ok(None)
}

/// Open the interactive terminal UI. Reuses a healthy per-repository daemon
/// when possible and otherwise starts the runtime inside the TUI process.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn cmd_tui(
    layout: Layout,
    model: Option<String>,
    mode: RunMode,
    auto_approve: bool,
    in_process: bool,
    socket: Option<PathBuf>,
    session: Option<String>,
    config_overridden: bool,
) -> anyhow::Result<std::process::ExitCode> {
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
        SocketIntent::ProbeDefault => connect_default_runtime(&socket_path).await.map_err(
            |error| {
                anyhow::anyhow!(
                    "the local runtime at {} answered the probe but rejected the client: {error}",
                    socket_path.display()
                )
            },
        )?,
        SocketIntent::RequireExplicit => Some(
            LocalSocketRuntimeClient::connect(&socket_path)
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
            let _snap = client.snapshot(&session_id).await.map_err(|e| {
                anyhow::anyhow!(
                    "cannot open session {id}: {e}\n\
                     list sessions: leveler resume"
                )
            })?;
            // Daemon path has no local registry; gauge may show 0 until first turn.
            (session_id, 0u32)
        } else {
            let bootstrap = client
                .create_session(CreateSessionRequest {
                    goal: "interactive session".to_string(),
                    model,
                    mode: match map_mode(mode) {
                        leveler_execution::PermissionProfile::RequestApproval => {
                            leveler_client_protocol::PermissionProfile::RequestApproval
                        }
                        leveler_execution::PermissionProfile::Assisted => {
                            leveler_client_protocol::PermissionProfile::Assisted
                        }
                        leveler_execution::PermissionProfile::FullAccess => {
                            leveler_client_protocol::PermissionProfile::FullAccess
                        }
                    },
                })
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
        };
        let client: Arc<dyn InteractiveRuntimeClient> = client;
        leveler_tui::run(client, boot).await?;
        return Ok(std::process::ExitCode::SUCCESS);
    }

    let app = Arc::new(Application::assemble(layout)?);
    let model_ref = resolve_model(app.as_ref(), model)?;
    let mode = map_mode(mode);

    let session_id = if let Some(id) = session.as_deref() {
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
        session_id
    } else {
        app.create_session(&model_ref, "interactive session")
            .await?
    };

    let in_process_client = Arc::new(InProcessRuntimeClient::new_with_options(
        app.clone(),
        model_ref.clone(),
        mode,
        false,
        auto_approve,
    ));
    // Do not overwrite persisted mode/model with process defaults when reopening.
    if session.is_none() {
        in_process_client.attach_session(session_id.clone());
    }
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
    };

    leveler_tui::run(client, boot).await?;
    Ok(std::process::ExitCode::SUCCESS)
}

/// A running daemon transport: a per-repo Unix socket, or a loopback TCP
/// listener whose generated bearer token is printed once for external clients.
enum BoundServer {
    Unix(LocalSocketServer),
    Tcp { server: TcpRuntimeServer, token: String },
}

/// A 256-bit bearer token from the OS CSPRNG, hex-encoded. Never derived from
/// time/pid — it is the daemon's only network auth secret, so it must be
/// unpredictable.
fn generate_daemon_token() -> String {
    use std::fmt::Write;
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).expect("OS CSPRNG unavailable");
    let mut token = String::with_capacity(64);
    for b in bytes {
        let _ = write!(token, "{b:02x}");
    }
    token
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn cmd_serve(
    layout: Layout,
    model: Option<String>,
    mode: RunMode,
    auto_approve: bool,
    sandbox: bool,
    socket: Option<PathBuf>,
    tcp: Option<SocketAddr>,
) -> anyhow::Result<std::process::ExitCode> {
    let socket_path = socket.unwrap_or_else(|| layout.socket_path());
    let app = Arc::new(Application::assemble(layout)?);
    let model_ref = resolve_model(app.as_ref(), model)?;

    let runtime = Arc::new(InProcessRuntimeClient::new_with_options(
        app.clone(),
        model_ref.clone(),
        map_mode(mode),
        sandbox,
        auto_approve,
    ));
    let service: Arc<dyn leveler_local_transport::LocalRuntimeService> = runtime.clone();

    // Bind first — a successful bind (Unix socket file or TCP port) proves no
    // live daemon owns this repo, so afterwards startup may classify old
    // `running` rows as crash leftovers.
    let bound = match tcp {
        Some(addr) => {
            let token = generate_daemon_token();
            let server = TcpRuntimeServer::bind(addr, token.clone(), service).await?;
            BoundServer::Tcp { server, token }
        }
        None => BoundServer::Unix(LocalSocketServer::bind(&socket_path, service).await?),
    };

    let db = app.open_database().await?;
    let reaped = leveler_engine::reap_running_turns(&db, None).await?.len();
    if reaped > 0 {
        tracing::warn!(reaped, "reaped zombie turns before daemon startup");
    }
    println!("{}", Line::heading("Local runtime ready"));
    match &bound {
        BoundServer::Unix(server) => println!("  socket: {}", server.path().display()),
        BoundServer::Tcp { server, token } => {
            println!("  tcp: {}", server.local_addr()?);
            // Printed once to the operator's own terminal: this is how a WebUI /
            // external client authenticates. Not logged elsewhere.
            println!("  token: {token}");
        }
    }
    println!("  model: {model_ref}");
    println!("  press Ctrl+C to stop the daemon");

    let shutdown = CancellationToken::new();
    let signal_shutdown = shutdown.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal_shutdown.cancel();
        }
    });
    let result = match bound {
        BoundServer::Unix(server) => server.serve(shutdown).await,
        BoundServer::Tcp { server, .. } => server.serve(shutdown).await,
    };
    // Stopping the daemon is an explicit runtime shutdown, unlike closing a
    // TUI client. Cancel and reap any remaining turns before process exit.
    let _ = runtime
        .send(leveler_client_protocol::ClientCommand::Quit)
        .await;
    result?;
    Ok(std::process::ExitCode::SUCCESS)
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
    cmd_tui(
        layout,
        None,
        RunMode::Assisted,
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

/// Headless recovery of an interrupted non-interactive run (`run --resume`).
pub(crate) async fn cmd_run_resume(
    layout: Layout,
    id: String,
    auto_approve: bool,
    confirm_recovery: bool,
    output: OutputFormat,
) -> anyhow::Result<std::process::ExitCode> {
    // Axes SoT is the session row: resume_session reloads work_profile /
    // collaboration from DB. assemble() defaults (balanced) must not stick.
    let app = Application::assemble(layout)?;
    let session_id = leveler_core::SessionId::new(id.clone());

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
        if let Ok((wp, collab)) = app.session_product_axes(&session_id).await {
            println!("  work-mode: {} · collab: {}", wp.as_str(), collab.as_str());
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

    finish(result, &id, output)
}

/// Run the git/GitHub workflow for the produced changes and print the result.
async fn ship_changes_and_print(
    app: &Application,
    model: &leveler_model::ModelRef,
    goal: &str,
    modified: &[String],
    verified: bool,
    ship: &leveler_app::ShipOptions,
) {
    println!("{}", Line::heading("Shipping changes"));
    match app
        .ship_changes(
            goal,
            modified,
            verified,
            model,
            ship,
            CancellationToken::new(),
        )
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

/// Render the final summary and pick an exit code, handling cancellation
/// gracefully (a cancelled run is resumable, not a hard error).
fn finish(
    result: Result<leveler_agent::AgentOutcome, leveler_app::AppError>,
    session_id: &str,
    output: OutputFormat,
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
                        Line::ok(&format!("Completed in {} round(s).", outcome.rounds))
                    ),
                    StopReason::Answered => println!(
                        "{}",
                        Line::warn(&format!(
                            "Answer ended after {} round(s); task completion was not independently verified.",
                            outcome.rounds
                        ))
                    ),
                    StopReason::CloseoutForced => println!(
                        "{}",
                        Line::warn(&format!(
                            "Plan complete; stopped redundant closeout after {} round(s).",
                            outcome.rounds
                        ))
                    ),
                    StopReason::Incomplete => println!(
                        "{}",
                        Line::warn(&format!(
                            "Stopped after {} round(s): completeness could not be established.",
                            outcome.rounds
                        ))
                    ),
                    StopReason::BudgetExhausted => println!(
                        "{}",
                        Line::warn(&format!(
                            "Stopped after {} round(s): {} Resume with: leveler resume {session_id}",
                            outcome.rounds, outcome.final_text
                        ))
                    ),
                    StopReason::TurnLimitReached => println!(
                        "{}",
                        Line::warn(&format!(
                            "Hit absolute round ceiling after {} round(s). \
                             The turn was force-stopped to guarantee termination; \
                             check if the model was looping.",
                            outcome.rounds
                        ))
                    ),
                    StopReason::Blocked => println!(
                        "{}",
                        Line::warn(&format!(
                            "Stopped: the model reported the goal blocked after {} round(s).",
                            outcome.rounds
                        ))
                    ),
                    StopReason::Stalled => println!(
                        "{}",
                        Line::warn(&format!(
                            "Stopped: the model went quiet without resolving the goal \
                             after {} round(s) (not verified).",
                            outcome.rounds
                        ))
                    ),
                    StopReason::CompletedUnverified => println!(
                        "{}",
                        Line::warn(&format!(
                            "Completed in {} round(s), but not independently verified (no verification gate).",
                            outcome.rounds
                        ))
                    ),
                }
            } else {
                emit_jsonl(serde_json::json!({
                    "type": "session_completed",
                    "session_id": session_id,
                    "stop_reason": format!("{:?}", outcome.stop_reason),
                    "rounds": outcome.rounds,
                    "modified_files": outcome.modified_files,
                }));
            }
            let ok = outcome.stop_reason == StopReason::Completed;
            Ok(if ok {
                std::process::ExitCode::SUCCESS
            } else {
                std::process::ExitCode::FAILURE
            })
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

    #[test]
    fn explicit_socket_is_required_and_never_silently_downgraded() {
        assert_eq!(
            socket_intent(false, false, true, false),
            SocketIntent::RequireExplicit,
        );
    }

    #[test]
    fn non_replayable_launch_options_force_the_embedded_runtime() {
        assert_eq!(
            socket_intent(false, true, false, false),
            SocketIntent::Embedded,
        );
        assert_eq!(
            socket_intent(false, false, false, true),
            SocketIntent::Embedded,
        );
        assert_eq!(
            socket_intent(true, false, false, false),
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
}
