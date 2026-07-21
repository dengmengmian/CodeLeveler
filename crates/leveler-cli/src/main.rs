//! The `leveler` CLI entry point.
//!
//! This crate only parses arguments, dispatches to `leveler-app`, and renders
//! output. No agent loop, provider request, tool, or verification logic lives
//! here .
#![forbid(unsafe_code)]

mod approver;
mod cli;
mod common;
mod eval_cmd;
mod eval_signals;
mod info_cmds;
mod init_cmd;
mod mcp_lsp_cmds;
mod memory_cmds;
mod output;
mod permissions_cmds;
mod render;
mod run_cmds;
mod sessions_cmd;
mod upgrade_cmd;

use std::path::PathBuf;

use clap::Parser;

use leveler_project::Layout;

use cli::{Cli, Command, ConfigCommand, ModelSubcommand, ModelsCommand, RunMode};
use eval_cmd::cmd_eval;
use info_cmds::{cmd_config_show, cmd_doctor, cmd_model_probe, cmd_models_list, cmd_models_show};
use mcp_lsp_cmds::{cmd_lsp, cmd_mcp};
use memory_cmds::cmd_memory;
use permissions_cmds::cmd_permissions;
use run_cmds::{
    cmd_discuss, cmd_plan, cmd_resume, cmd_run, cmd_run_orchestrated, cmd_run_parallel,
    cmd_run_resume, cmd_serve, cmd_tui,
};
use sessions_cmd::cmd_sessions;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let _ = leveler_core::install_environment(leveler_core::EnvSnapshot::new(
        std::env::vars_os(),
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        std::env::temp_dir(),
    ));
    let args = Cli::parse();
    // No subcommand or `tui` takes over the terminal (ratatui alternate
    // screen). Logs written to stderr there paint straight over the UI and
    // corrupt it, so TUI mode logs to a file instead.
    let is_tui = matches!(args.command, None | Some(Command::Tui { .. }));
    init_tracing(args.verbose, is_tui);

    match run(args).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{}", output::error_prefix());
            eprintln!("  {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn init_tracing(verbose: u8, is_tui: bool) {
    let level = match verbose {
        0 => "warn",
        1 => "info",
        _ => "debug",
    };
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| format!("leveler={level}"));
    if is_tui {
        // The TUI owns the terminal; stderr must stay clean. Redirect to a log
        // file, or disable logging entirely — never fall back to stderr, which
        // would be the very corruption we are avoiding.
        if let Some(file) = tui_log_file() {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(file))
                .try_init();
        }
        return;
    }
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

/// Append-mode `~/.leveler/leveler.log` (next to the global config) for TUI
/// runs. `None` disables file logging rather than risk corrupting the screen.
fn tui_log_file() -> Option<std::fs::File> {
    let path = leveler_app::GlobalConfig::path()?
        .parent()?
        .join("leveler.log");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()
}

fn resolve_layout(repo: Option<PathBuf>, config_dir: Option<PathBuf>) -> anyhow::Result<Layout> {
    let repo_root = match repo {
        Some(p) => p,
        None => std::env::current_dir()?,
    };
    Ok(Layout::resolve(repo_root, config_dir))
}

/// Register CLI `--readonly-root` values for every subsequent `Application::assemble`.
fn merge_cli_readonly_roots(roots: &[PathBuf]) {
    if roots.is_empty() {
        return;
    }
    leveler_app::set_process_readonly_roots(roots.to_vec());
}

async fn run(args: Cli) -> anyhow::Result<std::process::ExitCode> {
    let config_overridden = args.config_dir.is_some();
    // Merge CLI readonly roots into the env the composition root already reads
    // (`Application::default_readonly_roots`), so every assemble path inherits them.
    merge_cli_readonly_roots(&args.readonly_root);
    let layout = resolve_layout(args.repo, args.config_dir)?;

    // No subcommand (or explicit `tui`) opens the interactive terminal UI.
    let command = match args.command {
        Some(cmd) => cmd,
        None => {
            return cmd_tui(
                layout,
                None,
                RunMode::Assisted,
                false,
                false,
                None,
                None,
                config_overridden,
            )
            .await;
        }
    };

    match command {
        Command::Tui {
            model,
            mode,
            auto_approve,
            in_process,
            socket,
            session,
        } => {
            cmd_tui(
                layout,
                model,
                mode,
                auto_approve,
                in_process,
                socket,
                session,
                config_overridden,
            )
            .await
        }
        Command::Serve {
            model,
            mode,
            auto_approve,
            sandbox,
            socket,
            tcp,
        } => cmd_serve(layout, model, mode, auto_approve, sandbox, socket, tcp).await,
        Command::Doctor => cmd_doctor(layout),
        Command::Memory(mc) => cmd_memory(layout, mc),
        Command::Permissions(pc) => cmd_permissions(layout, pc),
        Command::Config(ConfigCommand::Show) => cmd_config_show(layout),
        Command::Models(ModelsCommand::List) => cmd_models_list(layout),
        Command::Models(ModelsCommand::Show { model }) => cmd_models_show(layout, &model).await,
        Command::Model(m) => match m.command {
            ModelSubcommand::Probe { model } => cmd_model_probe(layout, &model).await,
        },
        Command::Sessions(sc) => cmd_sessions(layout, sc).await,
        Command::Run {
            task,
            resume,
            model,
            mode,
            auto_approve,
            confirm_recovery,
            output,
            orchestrate,
            commit,
            branch,
            push,
            pr,
            pr_base,
            sandbox,
            work_mode,
            collaboration,
            parallel,
        } => {
            // `--resume <id>` continues an interrupted non-interactive run
            // (headless event stream); it does not take a fresh task.
            if let Some(id) = resume {
                return cmd_run_resume(layout, id, auto_approve, confirm_recovery, output).await;
            }
            let task = task.ok_or_else(|| {
                anyhow::anyhow!("a task is required (or pass --resume <id> to continue a run)")
            })?;
            let work_profile: leveler_lifecycle::WorkProfile =
                work_mode.parse().map_err(|e| anyhow::anyhow!("{e}"))?;
            let collab: leveler_lifecycle::CollaborationMode =
                collaboration.parse().map_err(|e| anyhow::anyhow!("{e}"))?;
            // pr implies push implies commit.
            let ship = leveler_app::ShipOptions {
                branch,
                commit: commit || push || pr,
                push: push || pr,
                open_pr: pr,
                pr_base,
            };
            if parallel > 1 {
                cmd_run_parallel(layout, task, model, mode, parallel).await
            } else if orchestrate {
                cmd_run_orchestrated(
                    layout,
                    task,
                    model,
                    mode,
                    auto_approve,
                    ship,
                    sandbox,
                    work_profile,
                )
                .await
            } else {
                cmd_run(
                    layout,
                    task,
                    model,
                    mode,
                    auto_approve,
                    output,
                    ship,
                    sandbox,
                    work_profile,
                    collab,
                )
                .await
            }
        }
        Command::Plan { task, model } => cmd_plan(layout, task, model).await,
        Command::Discuss {
            topic,
            rounds,
            model,
        } => cmd_discuss(layout, topic, rounds, model).await,
        Command::Eval(ec) => cmd_eval(layout, ec).await,
        Command::Lsp { file, diagnostics } => cmd_lsp(layout, file, diagnostics).await,
        Command::Mcp(mc) => cmd_mcp(mc),
        Command::Resume { id } => cmd_resume(layout, id, config_overridden).await,
        Command::Init => init_cmd::cmd_init(),
        Command::Upgrade {
            check,
            force,
            version,
        } => upgrade_cmd::cmd_upgrade(check, force, version).await,
    }
}
