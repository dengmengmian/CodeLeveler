//! The `leveler` CLI entry point.
//!
//! This crate only parses arguments, dispatches to `leveler-app`, and renders
//! output. No agent loop, provider request, tool, or verification logic lives
//! here .
#![forbid(unsafe_code)]

mod approver;
mod cli;
mod common;
mod completions_cmd;
mod crash;
mod eval_cmd;
mod eval_commitment;
mod eval_signals;
mod info_cmds;
mod init_cmd;
mod login_cmd;
mod mcp_lsp_cmds;
mod memory_cmds;
mod output;
mod permissions_cmds;
mod remote_cmds;
mod remote_invite;
mod render;
mod run_cmds;
mod sessions_cmd;
mod trust_cmds;
mod upgrade_cmd;

use std::path::PathBuf;

use clap::Parser;

use leveler_project::Layout;

use cli::{Cli, Command, ConfigCommand, ModelSubcommand, ModelsCommand, RunMode, ThemeCommand};
use eval_cmd::cmd_eval;
use info_cmds::{
    cmd_config_show, cmd_doctor, cmd_model_probe, cmd_models_list, cmd_models_show,
    cmd_theme_preview,
};
use mcp_lsp_cmds::{cmd_lsp, cmd_mcp};
use memory_cmds::cmd_memory;
use permissions_cmds::cmd_permissions;
use run_cmds::{
    cmd_resume, cmd_run, cmd_run_parallel, cmd_run_resume, cmd_serve, cmd_tui, cmd_web,
};
use sessions_cmd::cmd_sessions;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let _ = leveler_core::install_environment(leveler_core::EnvSnapshot::new(
        std::env::vars_os(),
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        std::env::temp_dir(),
    ));
    // Record panics before anything else can install a hook — the TUI's
    // terminal-restore hook chains to this one.
    crash::install(env!("CARGO_PKG_VERSION"));
    // Build provenance is part of the version string: an official binary must
    // be traceable to a commit, and one built from a dirty tree must say so
    // rather than let a reader assume it IS that commit. Handled before clap
    // so it replaces the default --version output.
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("{}", build_provenance());
        return std::process::ExitCode::SUCCESS;
    }
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

/// Tell the user when this repository ships `.leveler/hooks.yaml` or
/// `.leveler/permissions.yaml` that is being ignored for lack of trust.
///
/// Goes to stderr so it never contaminates machine-readable stdout, and runs
/// before dispatch so it precedes any alternate-screen UI.
fn warn_untrusted_project_config(layout: &leveler_project::Layout) {
    let home = leveler_core::LevelerHome::resolve(leveler_core::environment())
        .root()
        .to_path_buf();
    let ignored = leveler_execution::untrusted_project_files(&home, &layout.repo_root);
    if ignored.is_empty() {
        return;
    }
    for entry in &ignored {
        eprintln!(
            "{} 忽略了仓库内 {}（未信任）",
            crate::output::Line::warn("!"),
            entry.path.display()
        );
    }
    eprintln!("  它可以执行命令或授予免批准权限。确认内容后运行：leveler trust");
}

async fn run(args: Cli) -> anyhow::Result<std::process::ExitCode> {
    let config_overridden = args.config_dir.is_some();
    // Merge CLI readonly roots into the env the composition root already reads
    // (`Application::default_readonly_roots`), so every assemble path inherits them.
    merge_cli_readonly_roots(&args.readonly_root);
    let layout = resolve_layout(args.repo, args.config_dir)?;
    warn_untrusted_project_config(&layout);

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
            ready_json,
        } => {
            cmd_serve(
                layout,
                model,
                mode,
                auto_approve,
                sandbox,
                socket,
                tcp,
                ready_json,
            )
            .await
        }
        Command::Web {
            addr,
            connect,
            token,
            model,
            mode,
            auto_approve,
            sandbox,
        } => {
            cmd_web(
                layout,
                addr,
                connect,
                token,
                model,
                mode,
                auto_approve,
                sandbox,
            )
            .await
        }
        Command::Doctor => cmd_doctor(layout),
        Command::Memory(mc) => cmd_memory(layout, mc),
        Command::Permissions(pc) => cmd_permissions(layout, pc),
        Command::Trust { command } => crate::trust_cmds::cmd_trust(layout, command),
        Command::Remote(command) => crate::remote_cmds::cmd_remote(command).await,
        Command::Config(ConfigCommand::Show) => cmd_config_show(layout),
        Command::Theme(ThemeCommand::Preview { id }) => cmd_theme_preview(id),
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
        Command::Eval(ec) => cmd_eval(layout, ec).await,
        Command::Lsp { file, diagnostics } => cmd_lsp(layout, file, diagnostics).await,
        Command::Mcp(mc) => cmd_mcp(mc),
        Command::Resume { id } => cmd_resume(layout, id, config_overridden).await,
        Command::Init => init_cmd::cmd_init(),
        Command::Completions { shell } => completions_cmd::cmd_completions(shell),
        Command::Login { provider } => login_cmd::cmd_login(provider).await,
        Command::Logout { provider } => login_cmd::cmd_logout(provider),
        Command::Upgrade {
            check,
            force,
            version,
        } => upgrade_cmd::cmd_upgrade(check, force, version).await,
    }
}

/// One line identifying exactly which source this binary was built from.
///
/// `UNTRUSTED` is not decoration: a binary built from a dirty tree is not the
/// commit it names, and Batch #1 lost an investigation cycle to exactly that
/// confusion.
fn build_provenance() -> String {
    let commit = env!("LEVELER_BUILD_COMMIT");
    let dirty = env!("LEVELER_BUILD_DIRTY") == "true";
    format_provenance(env!("CARGO_PKG_VERSION"), commit, dirty)
}

fn format_provenance(version: &str, commit: &str, dirty: bool) -> String {
    let short = commit.get(..12).unwrap_or(commit);
    if dirty {
        format!("leveler {version} ({short}-dirty) UNTRUSTED: built from a modified working tree")
    } else {
        format!("leveler {version} ({short})")
    }
}

#[cfg(test)]
mod provenance_tests {
    use super::format_provenance;

    /// A clean build names its commit and claims nothing more.
    #[test]
    fn a_clean_build_reports_its_commit() {
        let line = format_provenance("0.1.4", "c3bf11ba01c3d5e5ef66244dcc3a6ae787036268", false);
        assert!(line.contains("0.1.4"), "{line}");
        assert!(line.contains("c3bf11ba01c3"), "{line}");
        assert!(!line.contains("UNTRUSTED"), "{line}");
    }

    /// The accident this exists for: HEAD said one commit, the binary carried
    /// another session's uncommitted work, and nothing in the binary said so.
    #[test]
    fn a_dirty_build_marks_itself_untrusted() {
        let line = format_provenance("0.1.4", "c3bf11ba01c3d5e5ef66244dcc3a6ae787036268", true);
        assert!(line.contains("UNTRUSTED"), "{line}");
        assert!(line.contains("dirty"), "{line}");
        assert!(
            line.contains("c3bf11ba01c3"),
            "the commit is still named so the drift is diagnosable: {line}"
        );
    }

    /// A short or absent commit must not panic the version path.
    #[test]
    fn an_unknown_commit_is_reported_not_fatal() {
        let line = format_provenance("0.1.4", "unknown", true);
        assert!(line.contains("unknown"), "{line}");
        assert!(line.contains("UNTRUSTED"), "{line}");
    }
}
