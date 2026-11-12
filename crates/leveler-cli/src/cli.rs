//! Command-line argument definitions (clap derive).

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

/// CodeLeveler — a model-agnostic coding agent CLI.
#[derive(Debug, Parser)]
#[command(name = "leveler", disable_version_flag = true, about, long_about = None)]
pub struct Cli {
    /// Print version and build provenance.
    #[arg(short = 'V', long, action = clap::ArgAction::SetTrue)]
    pub version: bool,

    /// Repository root (defaults to the current directory).
    #[arg(long, global = true)]
    pub repo: Option<PathBuf>,

    /// Config bundle directory (defaults to $LEVELER_CONFIG_DIR or <repo>/configs).
    #[arg(long, global = true)]
    pub config_dir: Option<PathBuf>,

    /// Override the CodeLeveler home (`$LEVELER_HOME`) for this invocation.
    ///
    /// Internal: OS wakeup registrations use it so the job names the exact home
    /// it was enabled for instead of inheriting an ambient environment.
    #[arg(long, global = true, value_name = "PATH", hide = true)]
    pub home: Option<PathBuf>,

    /// Increase log verbosity (-v, -vv).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Subcommand to run. With no subcommand, `leveler` opens the interactive
    /// terminal UI (equivalent to `leveler tui`).
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
/// A CLI command line is parsed once per process and dispatched immediately;
/// boxing the schedule subcommand would only add deref noise at every match
/// site (clap still accepts a boxed subcommand, but nothing here stores the
/// enum or moves it in bulk).
#[allow(clippy::large_enum_variant)]
pub enum Command {
    /// Internal JSONL adapter for the desktop shell.
    #[command(hide = true)]
    DesktopBridge,
    /// Internal stable owner for project background services.
    #[command(hide = true)]
    ExecutionHost {
        #[arg(long, value_name = "PATH")]
        state_dir: PathBuf,
    },
    /// Open the interactive terminal UI (the default with no subcommand).
    Tui {
        /// Model reference (defaults to the configured default).
        #[arg(long)]
        model: Option<String>,
        /// Permission profile: request-approval | assisted | full-access.
        ///
        /// Omitted: a new session uses the project/default profile and a
        /// resumed `--session` keeps its persisted profile. Supplied: it is an
        /// explicit override of that persisted profile.
        #[arg(long = "permission", value_enum)]
        mode: Option<RunMode>,
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
        /// Serve the app-owned source without a primary workspace.
        #[arg(long)]
        no_workspace: bool,
        /// Default model for newly created sessions.
        #[arg(long)]
        model: Option<String>,
        /// Default permission profile: request-approval | assisted | full-access.
        #[arg(long = "permission", value_enum)]
        mode: Option<RunMode>,
        /// Auto-approve risky actions for sessions owned by this daemon.
        #[arg(long)]
        auto_approve: bool,
        /// Deny network access to run_command processes.
        #[arg(long = "deny-network", alias = "sandbox")]
        deny_network: bool,
        /// Override the per-repository Unix socket path.
        #[arg(long, value_name = "PATH")]
        socket: Option<PathBuf>,
        /// Also expose the daemon over loopback TCP at this address (e.g.
        /// `127.0.0.1:7878`) for external / WebUI clients. The per-repo Unix
        /// socket is still bound (ownership lock + token-less local clients).
        /// The bearer token comes from LEVELER_DAEMON_TOKEN when set, else a
        /// fresh one is generated with the OS CSPRNG and printed once.
        #[arg(long, value_name = "ADDR")]
        tcp: Option<SocketAddr>,
        /// Write a machine-readable readiness JSON ({pid, socket, addr, token})
        /// to this path once bound. Used by the supervising WebUI aggregator.
        #[arg(long, value_name = "PATH")]
        ready_json: Option<PathBuf>,
    },

    /// Diagnose the environment, tooling, and configuration.
    Doctor,

    /// Probe and run deterministic actions over installed system applications.
    ///
    /// This path is direct: no model, daemon, session or agent loop is
    /// involved. The action still runs through the host execution authority.
    #[command(subcommand)]
    Native(NativeCommand),

    /// Manage user-global scheduled automation.
    ///
    /// A schedule is one typed action (shell, native or agent) on one trigger.
    /// A dispatcher connects to the workspace's ordinary runtime as a local
    /// client, so the run reuses the same Boot, ownership, permission and
    /// storage paths as an interactive session — and never starts a second
    /// agent runtime.
    #[command(subcommand)]
    Schedule(ScheduleCommand),

    /// Start the browser WebUI server for this repository.
    Web {
        /// Bind address (loopback only; defaults to an ephemeral port).
        #[arg(long, default_value = "127.0.0.1:0")]
        addr: SocketAddr,
        /// Connect to an existing `leveler serve --tcp` daemon at this address
        /// instead of running the runtime in-process.
        #[arg(long, value_name = "ADDR")]
        connect: Option<SocketAddr>,
        /// Bearer token. Required with --connect (the daemon's token, reused as
        /// the WebUI token); without --connect a fresh token is generated.
        #[arg(long)]
        token: Option<String>,
        /// Default model for newly created sessions (in-process runtime only).
        #[arg(long)]
        model: Option<String>,
        /// Default permission profile: request-approval | assisted | full-access.
        #[arg(long = "permission", value_enum)]
        mode: Option<RunMode>,
        /// Auto-approve risky actions for sessions owned by this runtime
        /// (in-process runtime only).
        #[arg(long)]
        auto_approve: bool,
        /// Deny network access to run_command processes (in-process runtime only).
        #[arg(long = "deny-network", alias = "sandbox")]
        deny_network: bool,
    },

    /// Manage durable permission rules (`.leveler/permissions.yaml`).
    #[command(subcommand)]
    Permissions(PermissionsCommand),
    /// Trust this repository's in-repo `.leveler/hooks.yaml` /
    /// `.leveler/permissions.yaml`. Both are ignored until trusted, and trust
    /// is keyed to the file contents — editing one drops it back to untrusted.
    #[command(subcommand_negates_reqs = true)]
    Trust {
        #[command(subcommand)]
        command: Option<TrustCommand>,
    },

    /// Inspect configuration.
    #[command(subcommand)]
    Config(ConfigCommand),

    /// Inspect TUI semantic themes (preview palettes; no live session).
    #[command(subcommand)]
    Theme(ThemeCommand),

    /// Manage configured models.
    #[command(subcommand)]
    Models(ModelsCommand),

    /// List and inspect agent definitions (project, user, built-in).
    #[command(subcommand)]
    Agents(AgentsCommand),

    /// List and inspect skills (project, user, external, built-in).
    #[command(subcommand)]
    Skills(SkillsCommand),

    /// Probe a model's text and streaming behavior.
    Model(ModelCommand),

    /// Manage sessions.
    #[command(subcommand)]
    Sessions(SessionsCommand),

    /// Inspect and stop background tasks on this repository's local runtime.
    ///
    /// A background task (a dev server, a watcher) outlives the turn that
    /// started it, and a retiring runtime waits for it instead of interrupting
    /// it. `list` names the tasks a handover is waiting on, `logs` reads one,
    /// and `stop` asks the runtime to end one — the runtime, not this CLI,
    /// signals the process.
    #[command(subcommand)]
    Background(BackgroundCommand),

    /// Analyze and reclaim CodeLeveler's own local storage.
    ///
    /// With no action flag this is a dry run: it reports what could be
    /// reclaimed and deletes nothing. `--safe` reclaims only provably-safe
    /// items — rebuildable caches, expired run residue, dead socket/lock
    /// metadata, historical automation state. Durable sessions and artifacts
    /// are never removed by `--safe`.
    Clean {
        /// Reclaim only provably-safe items.
        #[arg(long)]
        safe: bool,
        /// Reclaim rebuildable tool caches over the size budget.
        #[arg(long)]
        cache: bool,
        /// Reclaim expired ephemeral run residue.
        #[arg(long)]
        ephemeral: bool,
        /// Reclaim dead sockets and orphan lock metadata.
        #[arg(long)]
        stale_state: bool,
        /// Include items that require explicit confirmation.
        #[arg(long)]
        needs_confirmation: bool,
        /// Print the plan as JSON.
        #[arg(long)]
        json: bool,
        /// Write the frozen cleanup plan here before deleting anything.
        #[arg(long, value_name = "PATH")]
        manifest: Option<PathBuf>,
    },

    /// Inspect durable runtime observation (EventLog + model requests).
    Trace {
        /// Session id (defaults to the most recently updated session).
        session: Option<String>,
        /// Center the event window on this sequence.
        #[arg(long)]
        seq: Option<i64>,
        #[arg(long, default_value_t = 20)]
        before: u32,
        #[arg(long, default_value_t = 80)]
        after: u32,
        /// Machine-readable JSON.
        #[arg(long)]
        json: bool,
    },

    /// Run an agent task: the model uses tools to investigate and edit the repo.
    ///
    /// Exit codes: 0 completed | 1 execution failure | 2 usage error |
    /// 3 agent did not complete (honest, see stop_reason) | 130 interrupted.
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
        ///
        /// Omitted: a new task uses the project/default profile and
        /// `--resume` keeps the session's persisted profile. Supplied: it is
        /// an explicit override of that persisted profile.
        #[arg(long = "permission", value_enum)]
        mode: Option<RunMode>,
        /// Approve risky actions automatically (no prompts).
        #[arg(long)]
        auto_approve: bool,
        /// With `--resume`, acknowledge a crash-recovery stop: you have
        /// inspected the workspace; close the interrupted tool call(s) and
        /// proceed. Ignored without `--resume`.
        #[arg(long)]
        confirm_recovery: bool,
        /// Output format.
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        output: OutputFormat,
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
        /// Deny network access to run_command processes (OS sandbox).
        #[arg(long = "deny-network", alias = "sandbox")]
        deny_network: bool,
        /// Model-step safety ceiling for this run: a mechanical circuit
        /// breaker against a runaway model/tool loop, NOT a task budget. The
        /// task ends on its goal lifecycle or a resource budget. Default 200,
        /// far above normal runs (a task typically closes in 9-25 model
        /// steps); `0` pins no ceiling. One model step = one logical model
        /// request (provider retries included) plus the tool batch it produced.
        #[arg(
            long = "max-model-steps",
            visible_alias = "max-rounds",
            value_name = "N"
        )]
        max_model_steps: Option<u32>,
        /// Collaboration axis: chat | plan | goal.
        /// Default **goal** (a task is driven to completion). Use `chat` for
        /// an ordinary conversation that ends when the model answers.
        #[arg(long, default_value = "goal")]
        collaboration: String,
        /// Run N agents concurrently in isolated worktrees and integrate the
        /// results (>=2 enables parallel multi-agent editing). Requires
        /// `--collaboration goal`: only a goal-typed candidate is driven to a
        /// committed edit the integration step can merge. Shipping flags
        /// (--commit/--push/--pr/--branch/--pr-base) are not supported here;
        /// review the integrated result before shipping it explicitly.
        #[arg(long, default_value_t = 1)]
        parallel: usize,
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

    /// Store an API key for a provider in `~/.leveler/config.toml`.
    ///
    /// Prompts for the key without echoing it and tightens the config to
    /// owner-only. An exported `*_API_KEY` still takes precedence, so this
    /// never silently shadows an environment variable.
    Login {
        /// Provider id. Omit to pick from the configured providers.
        provider: Option<String>,
    },

    /// Remove a provider's stored API key (the `api_key_env` fallback stays).
    Logout {
        /// Provider id.
        provider: String,
    },

    /// Print a shell completion script on stdout.
    ///
    /// Source it from your shell rc, e.g.
    /// `leveler completions zsh > ~/.zfunc/_leveler` (with `~/.zfunc` on
    /// `$fpath`), or `eval "$(leveler completions bash)"`.
    Completions {
        /// Target shell.
        #[arg(value_name = "SHELL")]
        shell: clap_complete::Shell,
    },

    /// Update CodeLeveler to the latest stable release.
    ///
    /// Prefers a matching prebuilt asset for this host. Override the
    /// repository with `LEVELER_GITHUB_REPO=owner/name`.
    #[command(visible_alias = "upgrade")]
    Update {
        /// Only report whether an update is available (exit 2 if yes).
        #[arg(long)]
        check: bool,
        /// Reinstall even when already on the requested version.
        #[arg(long)]
        force: bool,
        /// Install a specific release tag (`1.0.0` or `v1.0.0`).
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

/// In-repo config trust (`<repo>/.leveler/hooks.yaml`, `permissions.yaml`).
///
/// With no subcommand, `leveler trust` lists what is untrusted and asks to
/// confirm — the same as `leveler trust allow`.
#[derive(Debug, Subcommand)]
pub enum TrustCommand {
    /// Trust the current contents of this repository's gated files.
    Allow {
        /// Skip the confirmation prompt (required when stdin is not a terminal).
        #[arg(long)]
        yes: bool,
    },
    /// Show what is trusted, untrusted, or absent for this repository.
    Show,
    /// Drop every trust record for this repository.
    Revoke,
}

#[derive(Debug, Subcommand)]
pub enum McpCommand {
    /// List configured MCP servers.
    List,

    /// Add an MCP server.
    ///
    /// Stdio: `leveler mcp add <name> [--env K=V]... -- <command> [args...]`.
    /// Streamable HTTP: `leveler mcp add <name> --url <url> [--header Name=ENV]... [--bearer-env ENV]`.
    ///
    /// Env and header values are environment **name references** only (never secret values).
    Add {
        /// Name for the server (its tools appear as `mcp__<name>__*`).
        name: String,
        /// Streamable HTTP endpoint (`http://` or `https://`). Omit for a stdio server.
        #[arg(long)]
        url: Option<String>,
        /// Request header whose value is an env-name reference (repeatable).
        /// Example: `--header X-API-Key=DEVORDER_MCP_API_KEY`.
        #[arg(long, value_parser = parse_env_pair, value_name = "NAME=ENV")]
        header: Vec<(String, String)>,
        /// Environment variable holding a bearer token. The `Bearer ` prefix is added at request time.
        #[arg(long, value_name = "ENV")]
        bearer_env: Option<String>,
        /// Env var name references to forward into a stdio server process (repeatable).
        /// Value must be an env **name** (`KEY=KEY`, `KEY=`, or `KEY=$OTHER`), not a secret.
        #[arg(long, value_parser = parse_env_pair, value_name = "KEY=VALUE")]
        env: Vec<(String, String)>,
        /// The launch command and its arguments, after `--`. Omit when `--url` is set.
        #[arg(last = true, value_name = "COMMAND")]
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
        /// Directory of eval case YAML files.
        #[arg(long)]
        cases: PathBuf,
        /// Accepted for backward compatibility; eval always uses the direct
        /// tool loop (the multi-phase orchestrate path was removed).
        #[arg(long)]
        direct: bool,
        /// Ablation: run WITHOUT the post-edit verification gate, so the
        /// model's own "done" is final. The case still passes or fails on the
        /// independent `expect` command.
        #[arg(long)]
        no_verify_gate: bool,
        /// Repeat every case to expose run-to-run variance (`--runs` is the same flag).
        #[arg(long, visible_alias = "runs", default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..))]
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
        /// Directory of eval case YAML files.
        #[arg(long)]
        cases: PathBuf,
        /// Repeat every case for each model under the same conditions.
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..))]
        repetitions: u32,
        /// Write a durable JSON baseline (both reports + gap + meta) to this path.
        #[arg(long, value_name = "PATH")]
        json_out: Option<PathBuf>,
    },
    /// Version-over-version trend: read a directory of run baselines and render
    /// a Markdown quality-trend table + regression flags.
    Trend {
        /// Directory of baseline JSON files written by `--json-out`.
        #[arg(long)]
        history: PathBuf,
        /// Write the report here.
        /// Without it, the report prints to stdout.
        #[arg(long, value_name = "PATH")]
        out: Option<PathBuf>,
    },
    /// Single-knob ablation: run the SAME model twice — knob as configured
    /// (control) vs flipped (ablated) — and report what the knob is worth.
    /// Run once per model to measure whether the mechanism helps or hurts it.
    Ablate {
        /// The resolver input to flip: `investigation_batching` or
        /// `post_edit_action_throughput` (current).
        /// Legacy `explicit_plan` / `repeated_read_guard` names are refused
        /// because those mechanisms no longer exist.
        knob: String,
        /// Model reference.
        #[arg(long)]
        model: Option<String>,
        /// Directory of eval case YAML files.
        #[arg(long)]
        cases: PathBuf,
        /// Accepted for backward compatibility; eval always uses the direct
        /// tool loop.
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

/// TUI theme inspection.
#[derive(Debug, Subcommand)]
pub enum ThemeCommand {
    /// Print a semantic-token preview. Omit the id to show dark, light, and
    /// high-contrast.
    Preview {
        /// Palette: auto | dark | light | high-contrast
        #[arg(value_name = "ID")]
        id: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum AgentsCommand {
    /// List the agents this project resolves, with status and shadowing.
    List {
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Show one agent's full definition.
    Show {
        /// Agent name, e.g. `security-reviewer`.
        name: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum SkillsCommand {
    /// List the skills this project resolves, with status and shadowing.
    List {
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Show one skill's description, body and bundled files.
    Show {
        /// Skill name, e.g. `windows-ci-debug`.
        name: String,
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum ModelsCommand {
    /// List configured models.
    List,
    /// Show a model's profile (capabilities, limits, reasoning).
    Show {
        /// Model reference, e.g. `deepseek/deepseek-flash`.
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
        /// Model reference, e.g. `deepseek/deepseek-flash`.
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
}

#[derive(Debug, Subcommand)]
pub enum BackgroundCommand {
    /// List live tasks owned by the runtime and independent execution host.
    List {
        /// Print machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Print a live background task's recent output.
    Logs { task_id: String },
    /// Ask the runtime to stop one background task and its process group.
    Stop { task_id: String },
    /// Inspect and release semantic write reservations the execution host left.
    ///
    /// A reservation exists from admission until the host reports the task's
    /// exit and the runtime settles it. One the host never admitted owns no
    /// process, and no host will ever report its exit — it would block every
    /// future runtime start until it is released.
    Journal {
        #[command(subcommand)]
        command: BackgroundJournalCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum BackgroundJournalCommand {
    /// List reservations that are still pending, with the facts that decide them.
    List {
        /// Print machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Release one reservation the host provably never admitted.
    Abandon { task_id: String },
}

#[derive(Debug, Subcommand)]
pub enum NativeCommand {
    /// Probe what this machine can actually do right now.
    ///
    /// Live probe: it does not launch applications, request permission or
    /// send anything.
    Capabilities {
        /// Print the structured report as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Run one deterministic action without a model, daemon or session.
    Action {
        /// Action id: mail.draft.create, mail.send.open_ui, mail.send.submit
        /// or mail.sent.check.
        #[arg(long)]
        action: String,
        /// One plain recipient address.
        #[arg(long)]
        to: String,
        /// Exact subject; line breaks are rejected.
        #[arg(long)]
        subject: String,
        /// Message body, empty when omitted. Not used by mail.sent.check.
        #[arg(long)]
        body: Option<String>,
        /// Read the body from a UTF-8 file instead of --body.
        #[arg(long, value_name = "PATH", conflicts_with = "body")]
        body_file: Option<PathBuf>,
        /// Print the structured outcome as JSON.
        #[arg(long)]
        json: bool,
    },
}

/// What a scheduled slot does when no dispatcher reached it in time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ScheduleMisfire {
    /// Record the slot as missed and do not execute it.
    Skip,
    /// Keep one overdue slot and execute it once on the next wake.
    #[value(name = "run-once")]
    RunOnce,
}

/// Who started one dispatch pass. Recorded with the wake, never treated as
/// execution authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ScheduleSource {
    /// A human ran `leveler schedule dispatch`.
    Manual,
    /// An operating-system registration (launchd, Task Scheduler).
    Os,
    /// The resident `leveler schedule serve` driver.
    Serve,
}

/// How a recurring schedule treats the session across runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ScheduleSessionPolicy {
    /// Reuse one durable session; context accumulates deliberately.
    Continue,
    /// Create a new session for every run; no context is carried over.
    Fresh,
}

/// What to do when a slot becomes due while the previous run is still active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ScheduleOverlap {
    /// Record the slot as skipped. Safe for actions with side effects.
    #[value(name = "skip-if-running")]
    SkipIfRunning,
    /// Keep at most one pending slot; a later wake runs it once.
    #[value(name = "queue-one")]
    QueueOne,
    /// Let runs overlap. Requires a fresh session per run, so each run gets
    /// its own execution context, and a side-effect-free or concurrency-safe
    /// action.
    #[value(name = "allow-concurrent")]
    AllowConcurrent,
}

#[derive(Debug, Subcommand)]
pub enum ScheduleCommand {
    /// Add a user-global scheduled action.
    #[command(group(
        clap::ArgGroup::new("action_kind").required(true).multiple(false)
    ))]
    Add {
        /// Display name.
        #[arg(long)]
        name: String,
        /// Workspace whose runtime executes the action (defaults to the
        /// current repository).
        #[arg(long, value_name = "PATH")]
        workspace: Option<PathBuf>,
        /// The exact shell command (a shell action).
        #[arg(long, group = "action_kind")]
        command: Option<String>,
        /// A typed native action id, for example `mail.draft.create`.
        #[arg(long, value_name = "ACTION_ID", group = "action_kind")]
        native_action: Option<String>,
        /// Structured JSON input for --native-action (defaults to `{}`).
        #[arg(long, value_name = "JSON", requires = "native_action")]
        native_input: Option<String>,
        /// The goal submitted to the ordinary agent runtime.
        #[arg(long, value_name = "TEXT", group = "action_kind")]
        agent_goal: Option<String>,
        /// Run once at this RFC3339 instant.
        #[arg(
            long,
            value_name = "RFC3339",
            conflicts_with_all = ["every", "daily", "weekly", "cron"]
        )]
        at: Option<String>,
        /// Run every N seconds.
        #[arg(
            long,
            value_name = "SECONDS",
            conflicts_with_all = ["at", "daily", "weekly", "cron"]
        )]
        every: Option<u64>,
        /// Run once per local calendar day at this HH:MM.
        #[arg(
            long,
            value_name = "HH:MM",
            conflicts_with_all = ["at", "every", "weekly", "cron"]
        )]
        daily: Option<String>,
        /// Run on these local weekdays at --local-time: `mon,wed` or `mon-fri`.
        #[arg(
            long,
            value_name = "DAYS",
            requires = "local_time",
            conflicts_with_all = ["at", "every", "daily", "cron"]
        )]
        weekly: Option<String>,
        /// Wall-clock time for --weekly.
        #[arg(long, value_name = "HH:MM", requires = "weekly")]
        local_time: Option<String>,
        /// Five-field cron expression, evaluated in --timezone.
        #[arg(
            long,
            value_name = "CRON",
            conflicts_with_all = ["at", "every", "daily", "weekly"]
        )]
        cron: Option<String>,
        /// IANA timezone for --daily, --weekly or --cron (defaults to the
        /// system zone).
        #[arg(long, value_name = "IANA")]
        timezone: Option<String>,
        /// First instant for --every (defaults to now).
        #[arg(long, value_name = "RFC3339", requires = "every")]
        anchor: Option<String>,
        /// Permission profile the action's session runs under.
        #[arg(long = "permission", value_enum, default_value = "assisted")]
        mode: RunMode,
        /// Model reference for an agent action.
        #[arg(long)]
        model: Option<String>,
        /// Session reuse for an agent or shell action. `allow-concurrent`
        /// requires `fresh`.
        #[arg(long, value_enum, default_value = "continue")]
        session_policy: ScheduleSessionPolicy,
        /// How an overlapping run is handled.
        #[arg(long, value_enum, default_value = "skip-if-running")]
        overlap: ScheduleOverlap,
        /// Persist an explicit unattended grant for this schedule identity.
        /// Without it a native or agent action is refused before it starts.
        #[arg(long)]
        grant_unattended: bool,
        /// Let the unattended grant cover actions with external side effects.
        #[arg(long, requires = "grant_unattended")]
        allow_external_side_effects: bool,
        /// Give up waiting for the action's terminal record after this many
        /// seconds; the run becomes unknown rather than retried.
        #[arg(long, default_value_t = 600)]
        timeout: u64,
        /// What to do with a slot no dispatcher reached in time.
        #[arg(long, value_enum, default_value = "skip")]
        misfire: ScheduleMisfire,
        /// Seconds a due slot may wait for a dispatcher before the misfire
        /// policy applies.
        #[arg(long, default_value_t = 300)]
        misfire_grace: u64,
        /// Print the created definition as JSON.
        #[arg(long)]
        json: bool,
    },
    /// List schedule definitions.
    List {
        /// Print the definitions as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show one definition: action, policy, grant, next occurrence and last run.
    Show {
        /// Definition id.
        id: String,
        /// Print the definition as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Replace fields of an existing definition in place, keeping its id and
    /// run history. Omitted fields keep their stored value.
    #[command(group(
        clap::ArgGroup::new("update_action_kind").multiple(false)
    ))]
    Update {
        /// Definition id.
        id: String,
        /// New display name.
        #[arg(long)]
        name: Option<String>,
        /// New workspace whose runtime executes the action.
        #[arg(long, value_name = "PATH")]
        workspace: Option<PathBuf>,
        /// Replace the action with this exact shell command.
        #[arg(long, group = "update_action_kind")]
        command: Option<String>,
        /// Replace the action with this typed native action id.
        #[arg(long, value_name = "ACTION_ID", group = "update_action_kind")]
        native_action: Option<String>,
        /// Structured JSON input for --native-action (defaults to `{}`).
        #[arg(long, value_name = "JSON", requires = "native_action")]
        native_input: Option<String>,
        /// Replace the action with this goal for the ordinary agent runtime.
        #[arg(long, value_name = "TEXT", group = "update_action_kind")]
        agent_goal: Option<String>,
        /// Replace the rule with a one-off at this RFC3339 instant.
        #[arg(
            long,
            value_name = "RFC3339",
            conflicts_with_all = ["every", "daily", "weekly", "cron"]
        )]
        at: Option<String>,
        /// Replace the rule with a run every N seconds.
        #[arg(
            long,
            value_name = "SECONDS",
            conflicts_with_all = ["at", "daily", "weekly", "cron"]
        )]
        every: Option<u64>,
        /// Replace the rule with one run per local calendar day at this HH:MM.
        #[arg(
            long,
            value_name = "HH:MM",
            conflicts_with_all = ["at", "every", "weekly", "cron"]
        )]
        daily: Option<String>,
        /// Replace the rule with runs on these local weekdays at --local-time.
        #[arg(
            long,
            value_name = "DAYS",
            requires = "local_time",
            conflicts_with_all = ["at", "every", "daily", "cron"]
        )]
        weekly: Option<String>,
        /// Wall-clock time for --weekly.
        #[arg(long, value_name = "HH:MM", requires = "weekly")]
        local_time: Option<String>,
        /// Replace the rule with a five-field cron expression.
        #[arg(
            long,
            value_name = "CRON",
            conflicts_with_all = ["at", "every", "daily", "weekly"]
        )]
        cron: Option<String>,
        /// IANA timezone for a new calendar rule.
        #[arg(long, value_name = "IANA")]
        timezone: Option<String>,
        /// First instant for --every (defaults to now).
        #[arg(long, value_name = "RFC3339", requires = "every")]
        anchor: Option<String>,
        /// New permission profile for the action's session.
        #[arg(long = "permission", value_enum)]
        mode: Option<RunMode>,
        /// New model reference for an agent action; an empty value clears it.
        #[arg(long)]
        model: Option<String>,
        /// New session reuse policy.
        #[arg(long, value_enum)]
        session_policy: Option<ScheduleSessionPolicy>,
        /// New overlapping-run policy.
        #[arg(long, value_enum)]
        overlap: Option<ScheduleOverlap>,
        /// New cap on waiting for the action's terminal record, in seconds.
        #[arg(long)]
        timeout: Option<u64>,
        /// New misfire policy.
        #[arg(long, value_enum)]
        misfire: Option<ScheduleMisfire>,
        /// New misfire grace, in seconds.
        #[arg(long)]
        misfire_grace: Option<u64>,
        /// The revision the editor last observed. A mismatch is refused
        /// instead of overwriting a newer definition.
        #[arg(long, value_name = "RFC3339")]
        expected_revision: Option<String>,
        /// Print the updated definition as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Issue an unattended grant for this definition. The grant is bound to
    /// its current action, workspace, permission mode and model; a later edit
    /// to any of those drops it again.
    Grant {
        /// Definition id.
        id: String,
        /// Let the grant cover actions with external side effects.
        #[arg(long)]
        allow_external_side_effects: bool,
        /// Print the updated definition as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show run history, newest first.
    Runs {
        /// Only runs of this definition id.
        #[arg(long)]
        definition: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Print the runs as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Remove a definition and its occurrence/run history. Refused while a
    /// run is active, so a runtime task is never orphaned.
    Remove {
        /// Definition id.
        id: String,
    },
    /// Stop creating future occurrences. An active run is not killed.
    Pause {
        /// Definition id.
        id: String,
    },
    /// Resume future occurrences.
    Resume {
        /// Definition id.
        id: String,
    },
    /// Create one explicit slot and dispatch it now, even while paused.
    RunNow {
        /// Definition id.
        id: String,
        /// Print the resulting run as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Ask the owning dispatcher to cancel this definition's active runs.
    Cancel {
        /// Definition id.
        id: String,
        /// Print the observed result as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Revoke the persisted unattended grant. Later runs are refused until a
    /// new grant is created.
    RevokeGrant {
        /// Definition id.
        id: String,
    },
    /// Run one dispatch pass: apply the misfire policy, recover expired
    /// leases, then execute due occurrences. This is the short-lived path an
    /// OS timer invokes.
    Dispatch {
        /// Maximum occurrences to execute in this pass.
        #[arg(long, default_value_t = 4)]
        max: usize,
        /// Who started this pass. Recorded with the wake.
        #[arg(long, value_enum, default_value = "manual")]
        source: ScheduleSource,
        /// Print the summary as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Stay resident and dispatch due occurrences as they become due. This is
    /// the persistent lightweight scheduler: one process, no second runtime.
    Serve {
        /// Maximum occurrences to execute per pass.
        #[arg(long, default_value_t = 4)]
        max: usize,
        /// Longest sleep between passes, in seconds.
        #[arg(long, default_value_t = 30)]
        poll_secs: u64,
    },
    /// Register this user's OS wakeup for scheduled actions. Opt-in: it is
    /// never enabled by adding a schedule.
    Enable {
        /// Print the observed OS registration as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Stop the OS wakeup without touching schedule definitions or history.
    Disable {
        /// Print the observed OS registration as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Remove the OS wakeup registration. Schedule data is kept.
    Uninstall {
        /// Print the observed OS registration as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show the OS registration and schedule counts side by side.
    Status {
        /// Print the state as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show dispatch-pass (wake) history, newest first. This is the record of
    /// what each OS wake, manual pass or resident driver observed.
    Wakeups {
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Print the wake history as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Check that schedule data, the OS registration and the executable
    /// agree. Read-only; exits non-zero on a failed check.
    Doctor {
        /// Print the checks as JSON.
        #[arg(long)]
        json: bool,
    },
}

#[cfg(test)]
mod tests {
    #[test]
    fn background_journal_subcommands_parse() {
        let list =
            super::Cli::try_parse_from(["leveler", "background", "journal", "list", "--json"]);
        assert!(list.is_ok(), "journal list must parse: {list:?}");
        let abandon = super::Cli::try_parse_from([
            "leveler",
            "background",
            "journal",
            "abandon",
            "host-orphan",
        ]);
        assert!(abandon.is_ok(), "journal abandon must parse: {abandon:?}");
    }

    #[test]
    fn execution_host_daemon_accepts_explicit_private_state_directory() {
        let parsed = super::Cli::try_parse_from([
            "leveler",
            "--repo",
            "/tmp/project",
            "execution-host",
            "--state-dir",
            "/tmp/private-home/project/execution-host",
        ]);
        assert!(
            parsed.is_ok(),
            "host launch arguments must parse: {parsed:?}"
        );
    }

    use clap::{CommandFactory, Parser};

    use super::*;

    #[test]
    fn schedule_subcommands_parse() {
        let cli = parse(&[
            "leveler",
            "schedule",
            "add",
            "--name",
            "nightly",
            "--command",
            "printf ok",
            "--every",
            "60",
            "--permission",
            "full-access",
            "--timeout",
            "30",
            "--json",
        ]);
        match cli.command {
            Some(Command::Schedule(ScheduleCommand::Add {
                name,
                command,
                every,
                mode,
                timeout,
                json,
                ..
            })) => {
                assert_eq!(name, "nightly");
                assert_eq!(command.as_deref(), Some("printf ok"));
                assert_eq!(every, Some(60));
                assert!(matches!(mode, RunMode::FullAccess));
                assert_eq!(timeout, 30);
                assert!(json);
            }
            other => panic!("expected Schedule::Add, got {other:?}"),
        }

        assert!(
            Cli::try_parse_from([
                "leveler",
                "schedule",
                "add",
                "--name",
                "x",
                "--command",
                "true",
                "--at",
                "2026-01-01T00:00:00Z",
                "--every",
                "60",
            ])
            .is_err(),
            "--at and --every are mutually exclusive"
        );

        // Exactly one action kind is required, and the kinds are exclusive.
        assert!(
            Cli::try_parse_from([
                "leveler",
                "schedule",
                "add",
                "--name",
                "x",
                "--at",
                "2026-01-01T00:00:00Z",
            ])
            .is_err(),
            "an action kind is required"
        );
        assert!(
            Cli::try_parse_from([
                "leveler",
                "schedule",
                "add",
                "--name",
                "x",
                "--command",
                "true",
                "--agent-goal",
                "do work",
                "--at",
                "2026-01-01T00:00:00Z",
            ])
            .is_err(),
            "two action kinds are refused"
        );
        let native = parse(&[
            "leveler",
            "schedule",
            "add",
            "--name",
            "draft",
            "--native-action",
            "mail.draft.create",
            "--native-input",
            "{\"to\":\"a@example.test\",\"subject\":\"s\",\"body\":\"b\"}",
            "--session-policy",
            "fresh",
            "--overlap",
            "queue-one",
            "--grant-unattended",
            "--allow-external-side-effects",
            "--at",
            "2026-01-01T00:00:00Z",
        ]);
        assert!(
            Cli::try_parse_from([
                "leveler",
                "schedule",
                "add",
                "--name",
                "draft",
                "--native-action",
                "mail.draft.create",
                "--daily",
                "09:00",
                "--session-policy",
                "fresh",
                "--overlap",
                "queue-one",
                "--grant-unattended",
                "--allow-external-side-effects",
                "--at",
                "2026-01-01T00:00:00Z",
            ])
            .is_err(),
            "--daily and --at are mutually exclusive"
        );
        match native.command {
            Some(Command::Schedule(ScheduleCommand::Add {
                native_action,
                session_policy,
                overlap,
                grant_unattended,
                allow_external_side_effects,
                ..
            })) => {
                assert_eq!(native_action.as_deref(), Some("mail.draft.create"));
                assert!(matches!(session_policy, ScheduleSessionPolicy::Fresh));
                assert!(matches!(overlap, ScheduleOverlap::QueueOne));
                assert!(grant_unattended);
                assert!(allow_external_side_effects);
            }
            other => panic!("expected a native Schedule::Add, got {other:?}"),
        }

        let dispatch = parse(&["leveler", "schedule", "dispatch", "--max", "2", "--json"]);
        assert!(matches!(
            dispatch.command,
            Some(Command::Schedule(ScheduleCommand::Dispatch {
                max: 2,
                source: ScheduleSource::Manual,
                json: true
            }))
        ));

        let os_dispatch = parse(&["leveler", "schedule", "dispatch", "--source", "os"]);
        assert!(matches!(
            os_dispatch.command,
            Some(Command::Schedule(ScheduleCommand::Dispatch {
                source: ScheduleSource::Os,
                ..
            }))
        ));

        for argv in [
            vec![
                "leveler",
                "--home",
                "/tmp/leveler home",
                "schedule",
                "dispatch",
                "--source",
                "os",
            ],
            vec![
                "leveler",
                "schedule",
                "dispatch",
                "--source",
                "os",
                "--home",
                "/tmp/leveler home",
            ],
        ] {
            let parsed = parse(&argv);
            assert_eq!(parsed.home, Some(PathBuf::from("/tmp/leveler home")));
            assert!(matches!(
                parsed.command,
                Some(Command::Schedule(ScheduleCommand::Dispatch {
                    source: ScheduleSource::Os,
                    ..
                }))
            ));
        }

        for (name, expected) in [
            ("enable", "enable"),
            ("disable", "disable"),
            ("uninstall", "uninstall"),
            ("status", "status"),
            ("doctor", "doctor"),
        ] {
            let parsed = parse(&["leveler", "schedule", name, "--json"]);
            let matched = matches!(
                parsed.command,
                Some(Command::Schedule(command))
                    if matches!(
                        (name, &command),
                        ("enable", ScheduleCommand::Enable { json: true })
                            | ("disable", ScheduleCommand::Disable { json: true })
                            | ("uninstall", ScheduleCommand::Uninstall { json: true })
                            | ("status", ScheduleCommand::Status { json: true })
                            | ("doctor", ScheduleCommand::Doctor { json: true })
                    )
            );
            assert!(matched, "{expected} must parse");
        }

        let add_misfire = parse(&[
            "leveler",
            "schedule",
            "add",
            "--name",
            "x",
            "--command",
            "true",
            "--at",
            "2026-01-01T00:00:00Z",
            "--misfire",
            "run-once",
            "--misfire-grace",
            "60",
        ]);
        match add_misfire.command {
            Some(Command::Schedule(ScheduleCommand::Add {
                misfire,
                misfire_grace,
                ..
            })) => {
                assert_eq!(misfire, ScheduleMisfire::RunOnce);
                assert_eq!(misfire_grace, 60);
            }
            other => panic!("expected Schedule::Add, got {other:?}"),
        }

        let serve = parse(&["leveler", "schedule", "serve", "--poll-secs", "5"]);
        assert!(matches!(
            serve.command,
            Some(Command::Schedule(ScheduleCommand::Serve {
                max: 4,
                poll_secs: 5,
            }))
        ));

        let runs = parse(&[
            "leveler",
            "schedule",
            "runs",
            "--definition",
            "abc",
            "--limit",
            "3",
            "--json",
        ]);
        assert!(matches!(
            runs.command,
            Some(Command::Schedule(ScheduleCommand::Runs {
                limit: 3,
                json: true,
                ..
            }))
        ));
    }

    #[test]
    fn retired_memory_subcommand_is_rejected() {
        assert!(Cli::try_parse_from(["leveler", "memory", "list"]).is_err());
    }

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
    fn help_and_shell_completions_do_not_advertise_retired_work_mode() {
        let mut command = Cli::command();
        let run = command.find_subcommand_mut("run").unwrap();
        assert!(!run.render_long_help().to_string().contains("--work-mode"));
        for shell in [
            clap_complete::Shell::Bash,
            clap_complete::Shell::Zsh,
            clap_complete::Shell::Fish,
            clap_complete::Shell::PowerShell,
        ] {
            let mut script = Vec::new();
            clap_complete::generate(shell, &mut Cli::command(), "leveler", &mut script);
            assert!(!String::from_utf8(script).unwrap().contains("--work-mode"));
        }
    }

    #[test]
    fn retired_work_mode_is_not_a_cli_option() {
        let error = Cli::try_parse_from(["leveler", "run", "fix it", "--work-mode", "balanced"])
            .expect_err("there is a single product tool-surface policy");
        assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn run_parses_collaboration_axis() {
        let cli = parse(&[
            "leveler",
            "run",
            "plan the feature",
            "--collaboration",
            "plan",
        ]);
        match cli.command {
            Some(Command::Run { collaboration, .. }) => {
                assert_eq!(collaboration, "plan");
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn run_parses_the_model_step_ceiling_under_both_spellings() {
        let cli = Cli::parse_from(["leveler", "run", "fix it", "--max-model-steps", "40"]);
        match cli.command {
            Some(Command::Run {
                max_model_steps, ..
            }) => assert_eq!(max_model_steps, Some(40)),
            other => panic!("unexpected: {other:?}"),
        }
        // The pre-rename spelling stays a working alias with the same meaning.
        let cli = Cli::parse_from(["leveler", "run", "fix it", "--max-rounds", "40"]);
        match cli.command {
            Some(Command::Run {
                max_model_steps, ..
            }) => assert_eq!(max_model_steps, Some(40)),
            other => panic!("unexpected: {other:?}"),
        }
        let cli = Cli::parse_from(["leveler", "run", "fix it"]);
        match cli.command {
            Some(Command::Run {
                max_model_steps, ..
            }) => assert_eq!(max_model_steps, None),
            other => panic!("unexpected: {other:?}"),
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
            Some(Command::Mcp(McpCommand::Add {
                name,
                env,
                command,
                url,
                header,
                bearer_env,
            })) => {
                assert_eq!(name, "fs");
                assert_eq!(env, vec![("TOKEN".to_string(), "abc".to_string())]);
                assert_eq!(command, vec!["npx", "-y", "server"]);
                assert!(url.is_none());
                assert!(header.is_empty());
                assert!(bearer_env.is_none());
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
    fn plan_and_discuss_are_not_product_subcommands() {
        // Multi-phase orchestrate stack removed: these must not parse as top-level
        // commands (they used to be frozen product surface in STABILITY drafts).
        assert!(
            Cli::try_parse_from(["leveler", "plan", "do something"]).is_err(),
            "plan must not be a subcommand"
        );
        assert!(
            Cli::try_parse_from(["leveler", "discuss", "topic"]).is_err(),
            "discuss must not be a subcommand"
        );
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
            "/tmp/dogfood/cases",
            "--direct",
            "--repetitions",
            "3",
            "--json-out",
            "/tmp/dogfood/run.json",
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
                assert_eq!(cases, PathBuf::from("/tmp/dogfood/cases"));
                assert!(direct);
                assert_eq!(repetitions, 3);
                assert_eq!(
                    json_out.as_deref(),
                    Some(std::path::Path::new("/tmp/dogfood/run.json"))
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
            "--cases",
            "/tmp/dogfood/cases",
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
                assert_eq!(cases, PathBuf::from("/tmp/dogfood/cases"));
                assert_eq!(repetitions, 2);
                assert_eq!(json_out.as_deref(), Some(std::path::Path::new("out.json")));
            }
            other => panic!("expected Eval Compare, got {other:?}"),
        }
    }

    #[test]
    fn eval_case_commands_require_explicit_cases() {
        assert!(Cli::try_parse_from(["leveler", "eval", "run"]).is_err());
        assert!(Cli::try_parse_from(["leveler", "eval", "compare", "model-a", "model-b"]).is_err());
        assert!(
            Cli::try_parse_from(["leveler", "eval", "ablate", "investigation_batching"]).is_err()
        );
        assert!(Cli::try_parse_from(["leveler", "eval", "trend"]).is_err());
    }

    #[test]
    fn repository_owned_eval_entrypoints_are_removed() {
        for command in ["quick", "daily", "release", "adoption-micro"] {
            assert!(
                Cli::try_parse_from(["leveler", "eval", command]).is_err(),
                "{command} must not remain as a built-in eval entrypoint"
            );
        }
        assert!(
            Cli::try_parse_from([
                "leveler",
                "eval",
                "run",
                "--suite",
                "adoption",
                "--experiment",
                "baseline",
            ])
            .is_err(),
            "framework runs belong to the external dogfood project"
        );
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
                assert!(matches!(mode, Some(RunMode::Assisted)));
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
            "--deny-network",
            "--socket",
            "/tmp/leveler.sock",
        ]);
        match cli.command {
            Some(Command::Serve {
                no_workspace,
                model,
                mode,
                auto_approve,
                deny_network,
                socket,
                tcp,
                ready_json,
            }) => {
                assert!(!no_workspace);
                assert_eq!(model.as_deref(), Some("deepseek/v4"));
                assert!(matches!(mode, Some(RunMode::FullAccess)));
                assert!(!auto_approve);
                assert!(deny_network);
                assert_eq!(socket, Some(PathBuf::from("/tmp/leveler.sock")));
                assert!(tcp.is_none(), "no --tcp given → Unix-only daemon");
                assert!(ready_json.is_none());
            }
            other => panic!("expected Serve, got {other:?}"),
        }
    }

    /// PR 6: the flag says what it does. The old spelling keeps working for
    /// scripts that still pass it.
    #[test]
    fn deny_network_flag_and_its_legacy_sandbox_alias_both_parse() {
        for flag in ["--deny-network", "--sandbox"] {
            let cli = parse(&["leveler", "run", "do it", flag]);
            match cli.command {
                Some(Command::Run { deny_network, .. }) => assert!(deny_network, "{flag}"),
                other => panic!("expected Run, got {other:?}"),
            }
        }
    }

    #[test]
    fn serve_parses_tcp_and_ready_json() {
        let cli = parse(&[
            "leveler",
            "serve",
            "--tcp",
            "127.0.0.1:0",
            "--ready-json",
            "/tmp/leveler-ready.json",
        ]);
        match cli.command {
            Some(Command::Serve {
                tcp, ready_json, ..
            }) => {
                assert_eq!(tcp, Some("127.0.0.1:0".parse().unwrap()));
                assert_eq!(ready_json, Some(PathBuf::from("/tmp/leveler-ready.json")));
            }
            other => panic!("expected Serve, got {other:?}"),
        }
    }

    #[test]
    fn web_parses_addr_connect_and_token() {
        let cli = parse(&[
            "leveler",
            "web",
            "--addr",
            "127.0.0.1:9000",
            "--connect",
            "127.0.0.1:7878",
            "--token",
            "abc123",
        ]);
        match cli.command {
            Some(Command::Web {
                addr,
                connect,
                token,
                ..
            }) => {
                assert_eq!(addr.to_string(), "127.0.0.1:9000");
                assert_eq!(
                    connect.map(|addr| addr.to_string()).as_deref(),
                    Some("127.0.0.1:7878")
                );
                assert_eq!(token.as_deref(), Some("abc123"));
            }
            other => panic!("expected Web, got {other:?}"),
        }
    }

    #[test]
    fn web_defaults_to_ephemeral_loopback_in_process() {
        let cli = parse(&["leveler", "web"]);
        match cli.command {
            Some(Command::Web {
                addr,
                connect,
                token,
                ..
            }) => {
                assert_eq!(addr.to_string(), "127.0.0.1:0");
                assert!(connect.is_none());
                assert!(token.is_none(), "no --connect → token is generated");
            }
            other => panic!("expected Web, got {other:?}"),
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
    fn background_parses_list_logs_and_stop() {
        assert!(matches!(
            parse(&["leveler", "background", "list"]).command,
            Some(Command::Background(BackgroundCommand::List { .. }))
        ));
        assert!(matches!(
            parse(&["leveler", "background", "logs", "bg-1"]).command,
            Some(Command::Background(BackgroundCommand::Logs { task_id })) if task_id == "bg-1"
        ));
        assert!(matches!(
            parse(&["leveler", "background", "stop", "bg-1"]).command,
            Some(Command::Background(BackgroundCommand::Stop { task_id })) if task_id == "bg-1"
        ));
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

    /// `--permission` absence and an explicit value must be distinguishable.
    /// A defaulted flag was silently overwriting a resumed session's persisted
    /// mode; `None` is "absent", `Some(..)` is an explicit override.
    #[test]
    fn permission_flag_absence_is_distinguishable_from_an_explicit_value() {
        match parse(&["leveler", "tui", "--session", "s1"]).command {
            Some(Command::Tui { mode, .. }) => {
                assert!(mode.is_none(), "absence must be None, never a default")
            }
            other => panic!("expected Tui, got {other:?}"),
        }
        match parse(&[
            "leveler",
            "tui",
            "--session",
            "s1",
            "--permission",
            "full-access",
        ])
        .command
        {
            Some(Command::Tui { mode, .. }) => {
                assert!(matches!(mode, Some(RunMode::FullAccess)))
            }
            other => panic!("expected Tui, got {other:?}"),
        }
        match parse(&["leveler", "run", "--resume", "s1"]).command {
            Some(Command::Run { mode, .. }) => assert!(mode.is_none()),
            other => panic!("expected Run, got {other:?}"),
        }
        match parse(&[
            "leveler",
            "run",
            "--resume",
            "s1",
            "--permission",
            "assisted",
        ])
        .command
        {
            Some(Command::Run { mode, .. }) => {
                assert!(matches!(mode, Some(RunMode::Assisted)))
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod native_command_tests {
    use clap::Parser;

    #[test]
    fn native_capabilities_and_action_parse() {
        let capabilities =
            super::Cli::try_parse_from(["leveler", "native", "capabilities", "--json"]);
        assert!(capabilities.is_ok(), "{capabilities:?}");
        let action = super::Cli::try_parse_from([
            "leveler",
            "native",
            "action",
            "--action",
            "mail.send.submit",
            "--to",
            "controlled@example.test",
            "--subject",
            "hello",
            "--body",
            "world",
            "--json",
        ]);
        assert!(action.is_ok(), "{action:?}");
        let conflict = super::Cli::try_parse_from([
            "leveler",
            "native",
            "action",
            "--action",
            "mail.send.submit",
            "--to",
            "controlled@example.test",
            "--subject",
            "hello",
            "--body",
            "world",
            "--body-file",
            "body.txt",
        ]);
        assert!(conflict.is_err(), "body and body-file must conflict");
    }
}
