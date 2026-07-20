//! Command-line argument definitions (clap derive).

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// CodeLeveler — a model-agnostic coding agent CLI.
#[derive(Debug, Parser)]
#[command(name = "leveler", version, about, long_about = None)]
pub struct Cli {
    /// Repository root (defaults to the current directory).
    #[arg(long, global = true)]
    pub repo: Option<PathBuf>,

    /// Config bundle directory (defaults to $LEVELER_CONFIG_DIR or <repo>/configs).
    #[arg(long, global = true)]
    pub config_dir: Option<PathBuf>,

    /// Extra directory the agent may **read** (not write). Repeatable.
    /// Also: env `LEVELER_READONLY_ROOTS` and `.leveler/config.yaml` `readonly_roots`.
    #[arg(long = "readonly-root", global = true, value_name = "DIR")]
    pub readonly_root: Vec<PathBuf>,

    /// Increase log verbosity (-v, -vv).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Subcommand to run. With no subcommand, `leveler` opens the interactive
    /// terminal UI (equivalent to `leveler tui`).
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Open the interactive terminal UI (the default with no subcommand).
    Tui {
        /// Model reference (defaults to the configured default).
        #[arg(long)]
        model: Option<String>,
        /// Permission profile: request-approval | assisted | full-access.
        #[arg(long = "permission", value_enum, default_value_t = RunMode::Assisted)]
        mode: RunMode,
        /// Approve risky actions automatically (no approval overlay). Required
        /// for unattended PTY/expect driving of the interactive UI.
        #[arg(long)]
        auto_approve: bool,
        /// Force the runtime to stay in the TUI process instead of reusing an
        /// existing `leveler serve` daemon.
        #[arg(long)]
        in_process: bool,
        /// Override the per-repository Unix socket path.
        #[arg(long, value_name = "PATH")]
        socket: Option<PathBuf>,
        /// Reopen an existing session (continue chat history) instead of creating
        /// a new one. Prefer this over `leveler resume` for interactive TUI chats.
        #[arg(long, value_name = "ID")]
        session: Option<String>,
    },

    /// Run the long-lived local runtime daemon for this repository.
    Serve {
        /// Default model for newly created sessions.
        #[arg(long)]
        model: Option<String>,
        /// Default permission profile: request-approval | assisted | full-access.
        #[arg(long = "permission", value_enum, default_value_t = RunMode::Assisted)]
        mode: RunMode,
        /// Auto-approve risky actions for sessions owned by this daemon.
        #[arg(long)]
        auto_approve: bool,
        /// Deny network access to run_command processes.
        #[arg(long)]
        sandbox: bool,
        /// Override the per-repository Unix socket path.
        #[arg(long, value_name = "PATH")]
        socket: Option<PathBuf>,
    },

    /// Diagnose the environment, tooling, and configuration.
    Doctor,

    /// Manage project-scoped durable memory (approved conclusions / preferences).
    #[command(subcommand)]
    Memory(MemoryCommand),

    /// Manage durable permission rules (`.leveler/permissions.yaml`).
    #[command(subcommand)]
    Permissions(PermissionsCommand),

    /// Inspect configuration.
    #[command(subcommand)]
    Config(ConfigCommand),

    /// Manage configured models.
    #[command(subcommand)]
    Models(ModelsCommand),

    /// Probe a model's text and streaming behavior.
    Model(ModelCommand),

    /// Manage sessions.
    #[command(subcommand)]
    Sessions(SessionsCommand),

    /// Run an agent task: the model uses tools to investigate and edit the repo.
    Run {
        /// The natural-language task. Omit only with `--resume`.
        task: Option<String>,
        /// Resume an interrupted non-interactive run by session id (headless,
        /// streams events). For interactive chat use `leveler resume <id>`.
        #[arg(long, value_name = "ID")]
        resume: Option<String>,
        /// Model reference (defaults to the only configured model).
        #[arg(long)]
        model: Option<String>,
        /// Permission profile: request-approval | assisted | full-access.
        #[arg(long = "permission", value_enum, default_value_t = RunMode::Assisted)]
        mode: RunMode,
        /// Approve risky actions automatically (no prompts).
        #[arg(long)]
        auto_approve: bool,
        /// Output format.
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        output: OutputFormat,
        /// Drive the task through the full state machine (Understand → Plan →
        /// Execute) instead of the direct tool loop.
        #[arg(long)]
        orchestrate: bool,
        /// After a successful run, commit the changes.
        #[arg(long)]
        commit: bool,
        /// Branch to create for the change (implies a dedicated branch).
        #[arg(long)]
        branch: Option<String>,
        /// After a successful run, push the branch (implies --commit).
        #[arg(long)]
        push: bool,
        /// After a successful run, open a pull request via `gh` (implies --push).
        #[arg(long)]
        pr: bool,
        /// Base branch for the pull request.
        #[arg(long)]
        pr_base: Option<String>,
        /// Deny network access to run_command processes (macOS OS sandbox).
        #[arg(long)]
        sandbox: bool,
        /// Work profile: economy | balanced | delivery (default balanced).
        #[arg(long, default_value = "balanced")]
        work_mode: String,
        /// Collaboration axis: chat | plan | goal.
        /// Default **chat** (ordinary turns). Use `goal` for
        /// delivery runs that must call update_goal to finish.
        #[arg(long, default_value = "chat")]
        collaboration: String,
        /// Run N agents concurrently in isolated worktrees and integrate the
        /// results (>=2 enables parallel multi-agent editing).
        #[arg(long, default_value_t = 1)]
        parallel: usize,
    },

    /// Analyze a task read-only: derive a requirement and a task plan (no edits).
    Plan {
        /// The natural-language task.
        task: String,
        /// Model reference (defaults to the only configured model).
        #[arg(long)]
        model: Option<String>,
    },

    /// Multi-agent discussion: several perspectives debate a topic, then synthesize.
    Discuss {
        /// The topic / question to discuss.
        topic: String,
        /// Number of rounds each participant speaks.
        #[arg(long, default_value_t = 2)]
        rounds: u32,
        /// Model reference (defaults to the configured default).
        #[arg(long)]
        model: Option<String>,
    },

    /// Run the evaluation harness.
    #[command(subcommand)]
    Eval(EvalCommand),

    /// Query a language server (LSP) for a file's symbols and diagnostics.
    Lsp {
        /// Source file to inspect (relative to the repo).
        file: PathBuf,
        /// Also wait for and print diagnostics.
        #[arg(long)]
        diagnostics: bool,
    },

    /// Manage MCP (Model Context Protocol) servers in the global config.
    #[command(subcommand)]
    Mcp(McpCommand),

    /// Reopen a session in the interactive TUI. With no id, lists recent
    /// sessions to pick from. (Headless task recovery moved to `run --resume`.)
    Resume {
        /// Session id to reopen. Omit to list recent sessions.
        id: Option<String>,
    },

    /// Create the global config (~/.leveler/config.toml) interactively.
    /// Refuses to overwrite an existing config; prints a template when not a TTY.
    Init,

    /// Check for or install a newer CodeLeveler release from GitHub.
    ///
    /// Prefers a matching prebuilt asset for this host. When no asset is
    /// published, falls back to `cargo install --git … --locked --force`.
    /// Override the repository with `LEVELER_GITHUB_REPO=owner/name`.
    Upgrade {
        /// Only report whether an update is available (exit 2 if yes).
        #[arg(long)]
        check: bool,
        /// Reinstall even when already on the requested version.
        #[arg(long)]
        force: bool,
        /// Install a specific release tag (`0.1.0` or `v0.1.0`).
        #[arg(long, value_name = "TAG")]
        version: Option<String>,
    },
}

/// Durable permission rules (`~/.leveler/permissions.yaml` + project
/// `.leveler/permissions.yaml`). Written by interactive **Always** approvals
/// and by hand-editing the YAML files; evaluated before the permission profile.
#[derive(Debug, Subcommand)]
pub enum PermissionsCommand {
    /// List global and project permission rules.
    List,
    /// Remove all project rules (delete `<repo>/.leveler/permissions.yaml`).
    /// Global `~/.leveler/permissions.yaml` is left untouched.
    Clear,
}

/// Project memory commands (`~/.leveler/projects/<repo>/memory/`).
///
/// Agent `remember`/`forget` still require interactive approval (K36). These
/// CLI commands are user-authoritative: the human is writing/archiving.
#[derive(Debug, Subcommand)]
pub enum MemoryCommand {
    /// List active memories.
    List {
        /// Also list archived (forgotten) entries.
        #[arg(long)]
        archived: bool,
    },
    /// Lexical search over active memories.
    Search {
        /// Query string (BM25 over title/body/tags).
        query: String,
        /// Max hits.
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// Show one active memory by id (or title slug).
    Show {
        /// Memory id.
        id: String,
    },
    /// Archive (soft-delete) an active memory by id.
    Forget {
        /// Memory id.
        id: String,
    },
    /// Create/update an active memory (user-authoritative write; no model round).
    Remember {
        /// Short title.
        title: String,
        /// Body text.
        body: String,
        /// Optional tags (repeatable).
        #[arg(long = "tag")]
        tags: Vec<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum McpCommand {
    /// List configured MCP servers.
    List,

    /// Add a stdio MCP server: `leveler mcp add <name> [--env K=V]... -- <command> [args...]`.
    ///
    /// `--env` stores environment **name references** only (never secret values).
    /// Use `KEY=KEY`, `KEY=`, or `KEY=$OTHER_ENV`; cleartext tokens are rejected.
    Add {
        /// Name for the server (its tools appear as `mcp__<name>__*`).
        name: String,
        /// Env var name references to forward into the server process (repeatable).
        /// Value must be an env **name** (`KEY=KEY`, `KEY=`, or `KEY=$OTHER`), not a secret.
        #[arg(long, value_parser = parse_env_pair, value_name = "KEY=VALUE")]
        env: Vec<(String, String)>,
        /// The launch command and its arguments, after `--`.
        #[arg(last = true, required = true, value_name = "COMMAND")]
        command: Vec<String>,
    },

    /// Remove an MCP server by name.
    Remove {
        /// The server name to remove.
        name: String,
    },
}

/// Parse a `KEY=VALUE` pair for `--env`.
///
/// On failure, do not echo the raw input (may contain a pasted secret).
fn parse_env_pair(s: &str) -> Result<(String, String), String> {
    match s.split_once('=') {
        Some((k, v)) if !k.is_empty() => Ok((k.to_string(), v.to_string())),
        _ => Err(
            "expected KEY=VALUE (value is an env name reference such as KEY= or KEY=$OTHER, not a secret)"
                .into(),
        ),
    }
}

#[derive(Debug, Subcommand)]
pub enum EvalCommand {
    /// Run all cases with one model and report metrics.
    Run {
        /// Model reference.
        #[arg(long)]
        model: Option<String>,
        /// Directory of eval case YAML files (`evals/smoke`, `evals/hard`, …).
        #[arg(long, default_value = "evals/smoke")]
        cases: PathBuf,
        /// Use the direct tool loop instead of the orchestrated state machine
        /// (for ablation: measures the value of the orchestration scaffold).
        #[arg(long)]
        direct: bool,
        /// Ablation: run WITHOUT the post-edit verification gate and its repair
        /// loop, so the model's own "done" is final. The case still passes or
        /// fails on the independent `expect` command, so this measures how often
        /// verify→repair rescues a run the model would have gotten wrong.
        /// Requires --direct (the orchestrated path has its own gates).
        #[arg(long, requires = "direct")]
        no_verify_gate: bool,
        /// Repeat every case to expose run-to-run variance.
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..))]
        repetitions: u32,
        /// Write a durable JSON baseline (report + meta) to this path.
        #[arg(long, value_name = "PATH")]
        json_out: Option<PathBuf>,
    },
    /// Run cases with two models and report the capability gap.
    Compare {
        /// First model.
        model_a: String,
        /// Second model.
        model_b: String,
        /// Directory of eval case YAML files (`evals/smoke`, `evals/hard`, …).
        #[arg(long, default_value = "evals/hard")]
        cases: PathBuf,
        /// Repeat every case for each model under the same conditions.
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..))]
        repetitions: u32,
        /// Write a durable JSON baseline (both reports + gap + meta) to this path.
        #[arg(long, value_name = "PATH")]
        json_out: Option<PathBuf>,
    },
    /// Single-knob ablation: run the SAME model twice — knob as configured
    /// (control) vs flipped (ablated) — and report what the knob is worth.
    /// Run once per model to measure whether the mechanism helps or hurts it.
    Ablate {
        /// The resolver input to flip: explicit_plan, step_summary,
        /// completion_evidence, or repeated_read_guard (legacy require_*
        /// names accepted).
        knob: String,
        /// Model reference.
        #[arg(long)]
        model: Option<String>,
        /// Directory of eval case YAML files (`evals/smoke`, `evals/hard`, …).
        #[arg(long, default_value = "evals/hard")]
        cases: PathBuf,
        /// Use the direct tool loop (recommended: fewer confounders than the
        /// orchestrated state machine).
        #[arg(long)]
        direct: bool,
        /// Repeat every case under both arms to expose run-to-run variance.
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..))]
        repetitions: u32,
        /// Write a durable JSON baseline (both reports + meta) to this path.
        #[arg(long, value_name = "PATH")]
        json_out: Option<PathBuf>,
    },
}

/// Progress output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum OutputFormat {
    /// Human-readable text.
    Text,
    /// One JSON object per line (for tools/CI).
    Jsonl,
}

/// CLI-facing three-tier permission profile.
#[derive(Debug, Clone, Copy, clap::ValueEnum, Default)]
pub enum RunMode {
    /// 请求批准 — always ask for external edits and network.
    RequestApproval,
    /// 替我审批 — default; only risky ops need approval.
    #[default]
    Assisted,
    /// 完全访问 — unrestricted FS + network (use with care).
    FullAccess,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Show the resolved configuration (secrets are never printed).
    Show,
}

#[derive(Debug, Subcommand)]
pub enum ModelsCommand {
    /// List configured models.
    List,
    /// Show a model's profile (capabilities, limits, reasoning).
    Show {
        /// Model reference, e.g. `deepseek/deepseek-v4-pro`.
        model: String,
    },
}

#[derive(Debug, clap::Args)]
pub struct ModelCommand {
    #[command(subcommand)]
    pub command: ModelSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum ModelSubcommand {
    /// Send a basic text + streaming probe to the model.
    Probe {
        /// Model reference, e.g. `deepseek/deepseek-v4-pro`.
        model: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum SessionsCommand {
    /// List stored sessions.
    List,
    /// Show a session by id: its config, turns, token usage and event log.
    Show {
        id: String,
        /// Print the raw session record as JSON instead of the readable view.
        #[arg(long)]
        json: bool,
    },
    /// Delete a session by id.
    Delete { id: String },
    /// Migrate this repository's pre-hash state directory without merging.
    MigrateState {
        /// Confirm the displayed source should be renamed to the destination.
        #[arg(long)]
        confirm: bool,
    },
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).expect("args must parse")
    }

    #[test]
    fn no_subcommand_defaults_to_tui() {
        let cli = parse(&["leveler"]);
        assert!(cli.command.is_none());
        assert_eq!(cli.verbose, 0);
    }

    #[test]
    fn state_migration_requires_an_explicit_confirmation_flag() {
        let cli = parse(&["leveler", "sessions", "migrate-state", "--confirm"]);
        assert!(matches!(
            cli.command,
            Some(Command::Sessions(SessionsCommand::MigrateState {
                confirm: true
            }))
        ));
    }

    #[test]
    fn run_parses_task_and_workflow_flags() {
        let cli = parse(&[
            "leveler",
            "run",
            "fix the bug",
            "--model",
            "deepseek/v4",
            "--commit",
            "--push",
            "--pr",
            "--branch",
            "fix/bug",
            "--parallel",
            "3",
        ]);
        match cli.command {
            Some(Command::Run {
                task,
                model,
                commit,
                push,
                pr,
                branch,
                parallel,
                ..
            }) => {
                assert_eq!(task.as_deref(), Some("fix the bug"));
                assert_eq!(model.as_deref(), Some("deepseek/v4"));
                assert!(commit && push && pr);
                assert_eq!(branch.as_deref(), Some("fix/bug"));
                assert_eq!(parallel, 3);
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn global_flags_apply_after_subcommand() {
        let cli = parse(&["leveler", "doctor", "--repo", "/tmp/x", "-vv"]);
        assert!(matches!(cli.command, Some(Command::Doctor)));
        assert_eq!(cli.repo.as_deref(), Some(std::path::Path::new("/tmp/x")));
        assert_eq!(cli.verbose, 2);
    }

    #[test]
    fn run_parses_collaboration_axis() {
        let cli = parse(&[
            "leveler",
            "run",
            "plan the feature",
            "--collaboration",
            "plan",
            "--work-mode",
            "delivery",
        ]);
        match cli.command {
            Some(Command::Run {
                collaboration,
                work_mode,
                ..
            }) => {
                assert_eq!(collaboration, "plan");
                assert_eq!(work_mode, "delivery");
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn memory_list_parses() {
        let cli = parse(&["leveler", "memory", "list", "--archived"]);
        match cli.command {
            Some(Command::Memory(MemoryCommand::List { archived })) => assert!(archived),
            other => panic!("expected Memory::List, got {other:?}"),
        }
    }

    #[test]
    fn memory_search_parses() {
        let cli = parse(&["leveler", "memory", "search", "workspace", "--limit", "3"]);
        match cli.command {
            Some(Command::Memory(MemoryCommand::Search { query, limit })) => {
                assert_eq!(query, "workspace");
                assert_eq!(limit, 3);
            }
            other => panic!("expected Memory::Search, got {other:?}"),
        }
    }

    #[test]
    fn mcp_add_takes_command_after_double_dash_and_env_pairs() {
        let cli = parse(&[
            "leveler",
            "mcp",
            "add",
            "fs",
            "--env",
            "TOKEN=abc",
            "--",
            "npx",
            "-y",
            "server",
        ]);
        match cli.command {
            Some(Command::Mcp(McpCommand::Add { name, env, command })) => {
                assert_eq!(name, "fs");
                assert_eq!(env, vec![("TOKEN".to_string(), "abc".to_string())]);
                assert_eq!(command, vec!["npx", "-y", "server"]);
            }
            other => panic!("expected Mcp Add, got {other:?}"),
        }
    }

    #[test]
    fn mcp_add_rejects_malformed_env() {
        let err = Cli::try_parse_from([
            "leveler",
            "mcp",
            "add",
            "fs",
            "--env",
            "no-equals",
            "--",
            "x",
        ]);
        assert!(err.is_err(), "KEY=VALUE validation must reject `no-equals`");
    }

    #[test]
    fn run_rejects_unknown_mode() {
        let err = Cli::try_parse_from(["leveler", "run", "t", "--permission", "nope"]);
        assert!(err.is_err());
    }

    #[test]
    fn eval_run_parses_json_out_and_direct() {
        let cli = parse(&[
            "leveler",
            "eval",
            "run",
            "--model",
            "deepseek/v4",
            "--cases",
            "evals/smoke",
            "--direct",
            "--repetitions",
            "3",
            "--json-out",
            "evals/baselines/run.json",
        ]);
        match cli.command {
            Some(Command::Eval(EvalCommand::Run {
                model,
                cases,
                direct,
                no_verify_gate,
                repetitions,
                json_out,
            })) => {
                assert!(!no_verify_gate, "the ablation is opt-in");
                assert_eq!(model.as_deref(), Some("deepseek/v4"));
                assert_eq!(cases, PathBuf::from("evals/smoke"));
                assert!(direct);
                assert_eq!(repetitions, 3);
                assert_eq!(
                    json_out.as_deref(),
                    Some(std::path::Path::new("evals/baselines/run.json"))
                );
            }
            other => panic!("expected Eval Run, got {other:?}"),
        }
    }

    #[test]
    fn eval_compare_parses_json_out() {
        let cli = parse(&[
            "leveler",
            "eval",
            "compare",
            "model-a",
            "model-b",
            "--repetitions",
            "2",
            "--json-out",
            "out.json",
        ]);
        match cli.command {
            Some(Command::Eval(EvalCommand::Compare {
                model_a,
                model_b,
                cases,
                repetitions,
                json_out,
            })) => {
                assert_eq!(model_a, "model-a");
                assert_eq!(model_b, "model-b");
                assert_eq!(cases, PathBuf::from("evals/hard"));
                assert_eq!(repetitions, 2);
                assert_eq!(json_out.as_deref(), Some(std::path::Path::new("out.json")));
            }
            other => panic!("expected Eval Compare, got {other:?}"),
        }
    }

    #[test]
    fn eval_run_defaults_to_smoke_suite() {
        let cli = parse(&["leveler", "eval", "run"]);
        match cli.command {
            Some(Command::Eval(EvalCommand::Run { cases, direct, .. })) => {
                assert_eq!(cases, PathBuf::from("evals/smoke"));
                assert!(!direct);
            }
            other => panic!("expected Eval Run, got {other:?}"),
        }
    }

    #[test]
    fn tui_parses_auto_approve_for_unattended_interactive() {
        let cli = parse(&[
            "leveler",
            "tui",
            "--model",
            "deepseek/v4",
            "--permission",
            "assisted",
            "--auto-approve",
            "--in-process",
        ]);
        match cli.command {
            Some(Command::Tui {
                model,
                mode,
                auto_approve,
                in_process,
                socket,
                session,
            }) => {
                assert_eq!(model.as_deref(), Some("deepseek/v4"));
                assert!(matches!(mode, RunMode::Assisted));
                assert!(auto_approve);
                assert!(in_process);
                assert!(socket.is_none());
                assert!(session.is_none());
            }
            other => panic!("expected Tui, got {other:?}"),
        }
    }

    #[test]
    fn serve_parses_local_runtime_options() {
        let cli = parse(&[
            "leveler",
            "serve",
            "--model",
            "deepseek/v4",
            "--permission",
            "full-access",
            "--sandbox",
            "--socket",
            "/tmp/leveler.sock",
        ]);
        match cli.command {
            Some(Command::Serve {
                model,
                mode,
                auto_approve,
                sandbox,
                socket,
            }) => {
                assert_eq!(model.as_deref(), Some("deepseek/v4"));
                assert!(matches!(mode, RunMode::FullAccess));
                assert!(!auto_approve);
                assert!(sandbox);
                assert_eq!(socket, Some(PathBuf::from("/tmp/leveler.sock")));
            }
            other => panic!("expected Serve, got {other:?}"),
        }
    }

    #[test]
    fn resume_parses_optional_id() {
        let with_id = parse(&["leveler", "resume", "sess-42"]);
        match with_id.command {
            Some(Command::Resume { id }) => assert_eq!(id.as_deref(), Some("sess-42")),
            other => panic!("expected Resume, got {other:?}"),
        }
        let no_id = parse(&["leveler", "resume"]);
        match no_id.command {
            Some(Command::Resume { id }) => assert_eq!(id, None),
            other => panic!("expected Resume, got {other:?}"),
        }
    }

    #[test]
    fn run_parses_resume_id() {
        let cli = parse(&["leveler", "run", "--resume", "sess-9"]);
        match cli.command {
            Some(Command::Run { task, resume, .. }) => {
                assert_eq!(task, None);
                assert_eq!(resume.as_deref(), Some("sess-9"));
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn tui_parses_session_id() {
        let cli = parse(&[
            "leveler",
            "tui",
            "--session",
            "79c12757-d3ad-4899-ab90-52a4997b8832",
        ]);
        match cli.command {
            Some(Command::Tui { session, .. }) => {
                assert_eq!(
                    session.as_deref(),
                    Some("79c12757-d3ad-4899-ab90-52a4997b8832")
                );
            }
            other => panic!("expected Tui, got {other:?}"),
        }
    }
}
