//! A basic async command runner for `run_command` (spec §18 run_command, §19).
//!
//! Provides: argument-array execution (no shell), separate stdout/stderr
//! captured without deadlock, timeout, cancellation, exit-code capture, and
//! scrubbing of known secret environment variables. Process-tree supervision:
//! Unix process groups (`killpg`) and Windows Job Objects via `process-wrap`
//! (WS1; no in-crate `unsafe`).

use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

/// Environment variables scrubbed from every spawned process so provider
/// credentials never leak into tool subprocesses.
const SECRET_ENV_DENYLIST: &[&str] = &[
    "DEEPSEEK_API_KEY",
    "BIGMODEL_API_KEY",
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "GEMINI_API_KEY",
    "GROQ_API_KEY",
];

/// Whether an environment variable name looks like a credential. Applied to
/// every spawned child in addition to the explicit denylists.
pub fn is_credential_env_name(name: &str) -> bool {
    const SECRET_SUFFIXES: &[&str] = &["_API_KEY", "_TOKEN", "_SECRET", "_PASSWORD", "_CREDENTIAL"];
    let normalized = name.to_ascii_uppercase();
    SECRET_ENV_DENYLIST.contains(&normalized.as_str())
        || SECRET_SUFFIXES.iter().any(|s| normalized.ends_with(s))
        || matches!(
            normalized.as_str(),
            "AWS_ACCESS_KEY_ID" | "AWS_SECRET_ACCESS_KEY" | "AWS_SESSION_TOKEN"
        )
}

/// Credential-like variables currently present in the parent environment,
/// plus caller-declared names. External subprocess adapters use this one policy
/// instead of maintaining incomplete local denylists.
pub fn credential_env_names(additional: &[String]) -> Vec<std::ffi::OsString> {
    credential_env_names_from(leveler_core::environment(), additional)
}

pub fn credential_env_names_from(
    environment: &leveler_core::EnvSnapshot,
    additional: &[String],
) -> Vec<std::ffi::OsString> {
    let mut names: Vec<std::ffi::OsString> = environment
        .vars_os()
        .filter_map(|(name, _)| {
            name.to_str()
                .is_some_and(is_credential_env_name)
                .then_some(name.clone())
        })
        .collect();
    for name in SECRET_ENV_DENYLIST
        .iter()
        .map(std::ffi::OsString::from)
        .chain(additional.iter().map(std::ffi::OsString::from))
    {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

/// A request to run one program with explicit arguments.
#[derive(Debug, Clone)]
pub struct ProcessRequest {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub timeout: Duration,
    /// Deny network access for this process (macOS seatbelt; no-op elsewhere).
    pub deny_network: bool,
    /// Confine filesystem *writes* to this root (plus the standard temp dirs).
    /// `Some(workspace_root)` in workspace-write mode blocks a stray `rm -rf` or
    /// an edit outside the repo at the OS level; `None` applies no write
    /// confinement (full-access / legacy).
    pub write_root: Option<PathBuf>,
    /// Extra project trees for host-side absolute-arg preflight (e.g.
    /// `--readonly-root`). OS sandbox reads are unrestricted;
    /// writes still use [`Self::write_root`] + toolchain caches.
    /// **Windows has no OS FS sandbox yet** — preflight is primary.
    pub extra_read_roots: Vec<PathBuf>,
    /// Per-stream cap on captured output. The process keeps running (its pipes
    /// are drained to EOF) but only the first and last halves of this many
    /// bytes are kept in memory; the middle is dropped and counted.
    pub max_output_bytes: usize,
    /// Additional environment variable names scrubbed from the child, on top
    /// of the built-in denylist and the secret-suffix patterns. Populated from
    /// the configured providers' `api_key_env` names.
    pub deny_env: Vec<String>,
    /// Credential-like variables intentionally granted to this trusted child.
    /// Tool/model-controlled requests must leave this empty.
    pub allow_env: Vec<String>,
    /// Host-trusted filesystem intent (WS2). When set, overrides the legacy
    /// write_root → restricted mapping for Windows capability gates. Models
    /// never choose this; only host policy may populate it.
    pub filesystem_intent: Option<crate::windows_sandbox::FilesystemIntent>,
}

/// Default per-stream output cap (1 MiB).
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 1024 * 1024;

impl ProcessRequest {
    pub fn new(program: impl Into<String>, args: Vec<String>, cwd: PathBuf) -> Self {
        Self {
            program: program.into(),
            args,
            cwd,
            timeout: Duration::from_secs(600),
            deny_network: false,
            write_root: None,
            extra_read_roots: Vec::new(),
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            deny_env: Vec::new(),
            allow_env: Vec::new(),
            filesystem_intent: None,
        }
    }
}

/// Whether a confined command should expose the host's existing dependency
/// caches through a read-only overlay. Network-denied requests always do; the
/// token check also covers explicit `cargo --offline`/`npm --offline`, including
/// commands carried inside a shell `-c` argument.
///
/// Only the macOS/Linux sandbox paths consult this; Windows has no host-cache
/// overlay, so the function is gated to avoid a dead-code warning there.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) fn should_read_host_caches(request: &ProcessRequest) -> bool {
    request.deny_network
        || request.args.iter().any(|arg| {
            arg == "--offline"
                || arg
                    .split_ascii_whitespace()
                    .any(|token| token == "--offline")
        })
}

/// Network policy when building a verify / acceptance [`ProcessRequest`].
///
/// Model acceptance hints always force deny; repo/builtin verify gates inherit
/// the session default (allow network unless the caller sets deny later).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyNetworkPolicy {
    /// Leave `deny_network` false (session / host default).
    InheritSession,
    /// Force `deny_network: true` (model-supplied acceptance commands).
    ForceDeny,
}

/// Build a sandbox-confined ProcessRequest for verification or acceptance.
///
/// Never HostTrusted: always sets `write_root` + `FilesystemIntent::WorkspaceWrite`
/// on `workspace_root`. Built-in credential env scrubbing still applies
/// (`deny_env` left empty for additional names). Models never choose the intent.
pub fn process_request_for_verify_check(
    program: impl Into<String>,
    args: Vec<String>,
    workspace_root: PathBuf,
    network: VerifyNetworkPolicy,
) -> ProcessRequest {
    let write_root = workspace_root.clone();
    let mut req = ProcessRequest::new(program, args, workspace_root);
    req.write_root = Some(write_root.clone());
    req.filesystem_intent = Some(crate::windows_sandbox::FilesystemIntent::WorkspaceWrite {
        write_root,
        extra_read_roots: Vec::new(),
    });
    req.deny_network = matches!(network, VerifyNetworkPolicy::ForceDeny);
    req
}

/// Wrap a command in an OS sandbox. Independent tightenings:
/// - `deny_network`: block network access.
/// - `write_root`: confine filesystem *writes* to the workspace (+ temp/toolchain
///   caches). Reads stay broad. Host-side absolute-argument
///   preflight on `run_command` still blocks model-supplied absolute paths
///   outside the workspace for non-full-access modes.
///
/// macOS uses `sandbox-exec` with a closed-by-default seatbelt profile; writable
/// roots are `-D` params. Linux uses bubblewrap with full ro-bind of `/` and
/// re-bind of writable roots.
///
/// Known debt: Apple has deprecated `sandbox-exec` (it still ships and works on
/// current macOS). If it disappears, switch to a `sandbox_init`-based wrapper.
/// Wrap a command for OS sandbox (seatbelt/bwrap). Used by [`CommandRunner`] and
/// background task spawn so both paths honor the same `ProcessRequest` fields.
pub(crate) fn sandbox_command(
    program: &str,
    args: &[String],
    deny_network: bool,
    write_root: Option<&Path>,
    extra_read_roots: &[PathBuf],
    scratch_root: Option<&Path>,
    cache_write_roots: &[PathBuf],
) -> (String, Vec<String>) {
    if !deny_network && write_root.is_none() {
        return (program.to_string(), args.to_vec());
    }
    #[cfg(target_os = "macos")]
    {
        macos_sandbox_command(
            program,
            args,
            deny_network,
            write_root,
            extra_read_roots,
            scratch_root,
            cache_write_roots,
        )
    }
    #[cfg(target_os = "linux")]
    {
        let _ = extra_read_roots;
        linux_sandbox_command(
            program,
            args,
            deny_network,
            write_root,
            scratch_root,
            cache_write_roots,
        )
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (
            write_root,
            extra_read_roots,
            scratch_root,
            cache_write_roots,
        );
        (program.to_string(), args.to_vec())
    }
}

#[cfg(target_os = "macos")]
const SEATBELT_BASE: &str = include_str!("seatbelt_base.sbpl");
#[cfg(target_os = "macos")]
const SEATBELT_NETWORK: &str = include_str!("seatbelt_network.sbpl");

/// Build the `sandbox-exec` argv on macOS (see [`sandbox_command`]).
#[cfg(target_os = "macos")]
fn macos_sandbox_command(
    program: &str,
    args: &[String],
    deny_network: bool,
    write_root: Option<&Path>,
    extra_read_roots: &[PathBuf],
    scratch_root: Option<&Path>,
    cache_write_roots: &[PathBuf],
) -> (String, Vec<String>) {
    let Some(root) = write_root else {
        // No write confinement (full-access dropping only the network): keep the
        // long-standing open profile so that path is unchanged.
        let profile = "(version 1)(allow default)(deny network*)".to_string();
        let mut wrapped = vec!["-p".to_string(), profile, program.to_string()];
        wrapped.extend_from_slice(args);
        return ("/usr/bin/sandbox-exec".to_string(), wrapped);
    };

    // Workspace-write mode allows broad reads (git needs ~/.gitconfig; tools
    // need system libs), writes confined to workspace + temp + toolchain caches.
    // Host-side absolute-arg preflight on run_command still blocks model-supplied
    // absolute paths outside the workspace for non-full-access modes.
    // `extra_read_roots` is reserved for future write/read carve-outs; write
    // roots already include toolchain trees.
    let _ = extra_read_roots;
    let write_roots = writable_roots(root, scratch_root, cache_write_roots);
    let protected = git_write_protected_paths(root);
    let mut policy = String::from(SEATBELT_BASE);
    policy.push_str("\n; unrestricted file reads, writes limited to approved roots\n");
    policy.push_str("(allow file-read*)\n");
    policy.push_str("(allow file-write*");
    for i in 0..write_roots.len() {
        policy.push_str(&format!(" (subpath (param \"WRITABLE_ROOT_{i}\"))"));
    }
    policy.push_str(")\n");
    // Keep .git metadata read-only even under a writable project root.
    for i in 0..protected.len() {
        policy.push_str(&format!(
            "(deny file-write* (subpath (param \"PROTECTED_WRITE_{i}\")))\n"
        ));
    }
    for i in 0..cache_write_roots.len() {
        policy.push_str(&format!(
            "(deny file-write* (literal (param \"CACHE_ROOT_{i}\")))\n"
        ));
    }
    if !deny_network {
        policy.push_str("(allow network-outbound)\n(allow network-inbound)\n");
        policy.push_str(SEATBELT_NETWORK);
    }

    let mut wrapped = vec!["-p".to_string(), policy];
    for (i, r) in write_roots.iter().enumerate() {
        wrapped.push(format!("-DWRITABLE_ROOT_{i}={}", r.display()));
    }
    for (i, r) in protected.iter().enumerate() {
        wrapped.push(format!("-DPROTECTED_WRITE_{i}={}", r.display()));
    }
    for (i, r) in cache_write_roots.iter().enumerate() {
        wrapped.push(format!("-DCACHE_ROOT_{i}={}", r.display()));
    }
    wrapped.push("--".to_string());
    wrapped.push(program.to_string());
    wrapped.extend_from_slice(args);
    ("/usr/bin/sandbox-exec".to_string(), wrapped)
}

/// Paths under the workspace that remain write-denied while the project root is
/// writable (`.git` protection).
#[cfg(any(target_os = "macos", target_os = "linux", test))]
pub fn git_write_protected_paths(write_root: &Path) -> Vec<PathBuf> {
    let git = write_root.join(".git");
    // Prefer real path when present so seatbelt matches the vnode.
    let path = git.canonicalize().unwrap_or(git);
    vec![path]
}

/// Directories a confined process may write to: its workspace and one private,
/// host-created scratch directory plus a Leveler-owned, per-workspace tool
/// cache. Environment redirection is applied by
/// [`apply_sandbox_environment`].
///
/// In particular, never add a shared temp directory or a whole user directory
/// here. Both allow a confined command to tamper with files consumed by other
/// host processes and turn a cache compatibility allowance into persistence.
#[cfg(any(target_os = "macos", target_os = "linux", test))]
fn writable_roots(
    root: &Path,
    scratch_root: Option<&Path>,
    cache_write_roots: &[PathBuf],
) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut add = |p: PathBuf| {
        if !p.is_dir() {
            return;
        }
        let real = p.canonicalize().unwrap_or(p);
        if !roots.contains(&real) {
            roots.push(real);
        }
    };
    add(root.to_path_buf());
    if let Some(scratch_root) = scratch_root {
        add(scratch_root.to_path_buf());
    }
    for cache_root in cache_write_roots {
        add(cache_root.clone());
    }
    roots
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod host_cache;
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) use host_cache::{SandboxPaths, apply_sandbox_environment, prepare_sandbox_paths};

/// True when `arg` looks like an absolute filesystem path the model might use
/// to bypass `read_file`.
///
/// - Unix: `/Users/…/other/repo/AGENTS.md`
/// - Windows: `C:\Users\…\other\repo\AGENTS.md`, `\\?\C:\…`, UNC `\\server\share\…`
///
/// Flags (`-n`) and relative paths are ignored. On non-Windows hosts, drive
/// letters like `C:\foo` are **not** treated as absolute (so Unix tests stay
/// stable); Windows builds use `Path::is_absolute`.
pub fn looks_like_absolute_path_arg(arg: &str) -> bool {
    if arg.is_empty() || arg.starts_with('-') {
        return false;
    }
    let p = Path::new(arg);
    if p.is_absolute() {
        return true;
    }
    // Explicit Windows shapes even when this binary is built for Unix (docs /
    // cross-tests). Real Windows relies on `Path::is_absolute` above.
    #[cfg(not(windows))]
    {
        let b = arg.as_bytes();
        // `C:\` or `C:/`
        if b.len() >= 3
            && b[0].is_ascii_alphabetic()
            && b[1] == b':'
            && (b[2] == b'\\' || b[2] == b'/')
        {
            return true;
        }
        // UNC `\\server\share`
        if arg.starts_with("\\\\") || arg.starts_with("//") {
            return true;
        }
    }
    false
}

/// First absolute path argument outside `allowed_roots` (canonicalized when
/// possible). Used by `run_command` for a **portable** preflight before spawn.
///
/// This is the only cross-platform read gate today:
/// - **macOS**: also OS seatbelt (deny `/Users`/`/home` except allowed roots)
/// - **Linux**: bwrap still ro-binds `/` (full read); rely on this preflight
/// - **Windows**: Job tree kill (WS1) + AppContainer RO/WW (WS3) when intent
///   is set — **preflight is
///   the primary defense** against `type`/`Get-Content`/`cat` of foreign trees
pub fn first_absolute_arg_outside_roots<'a>(
    args: &'a [String],
    allowed_roots: &[PathBuf],
) -> Option<&'a str> {
    let roots: Vec<PathBuf> = allowed_roots
        .iter()
        .filter_map(|r| r.canonicalize().ok().or_else(|| Some(r.clone())))
        .collect();
    for arg in args {
        if !looks_like_absolute_path_arg(arg) {
            continue;
        }
        let path = Path::new(arg.as_str());
        let probe = path
            .canonicalize()
            .unwrap_or_else(|_| lexical_abs_normalize(path));
        if !roots.iter().any(|r| path_is_under(&probe, r)) {
            return Some(arg.as_str());
        }
    }
    None
}

/// Path containment that is case-insensitive on Windows (drive letters / short
/// names) and prefix-safe (requires a boundary after the root).
fn path_is_under(path: &Path, root: &Path) -> bool {
    #[cfg(windows)]
    {
        let p = normalize_windows_path_key(path);
        let r = normalize_windows_path_key(root);
        p == r || p.starts_with(&(r.clone() + "\\")) || p.starts_with(&(r + "/"))
    }
    #[cfg(not(windows))]
    {
        path.starts_with(root)
    }
}

#[cfg(windows)]
fn normalize_windows_path_key(path: &Path) -> String {
    path.to_string_lossy()
        .trim_start_matches(r"\\?\")
        .replace('/', "\\")
        .to_ascii_lowercase()
}

fn lexical_abs_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::Prefix(prefix) => {
                out.push(prefix.as_os_str());
            }
            Component::RootDir => {
                out.push(comp.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(s) => out.push(s),
        }
    }
    out
}

/// Build the Linux `bwrap` (bubblewrap) invocation:
/// bind-mount the whole filesystem read-only, then re-`--bind` each writable
/// root read-write, with `--dev`/`--proc` for a minimal working environment and
/// `--unshare-net` when the network is denied. Requires `bwrap` on PATH.
///
/// NOTE: the argv assembly ([`bwrap_args`]) is unit-tested on any platform, but
/// the actual isolation can only be verified on Linux.
#[cfg(target_os = "linux")]
fn linux_sandbox_command(
    program: &str,
    args: &[String],
    deny_network: bool,
    write_root: Option<&Path>,
    scratch_root: Option<&Path>,
    cache_write_roots: &[PathBuf],
) -> (String, Vec<String>) {
    let Some(root) = write_root else {
        // No write confinement (full-access): only optionally drop the network,
        // via a fresh network namespace.
        if deny_network {
            let mut wrapped = vec![
                "--user".to_string(),
                "--map-root-user".to_string(),
                "--net".to_string(),
                program.to_string(),
            ];
            wrapped.extend_from_slice(args);
            return ("unshare".to_string(), wrapped);
        }
        return (program.to_string(), args.to_vec());
    };
    let roots = writable_roots(root, scratch_root, cache_write_roots);
    let protected = git_write_protected_paths(root);
    (
        "bwrap".to_string(),
        bwrap_args(program, args, deny_network, &roots, &protected),
    )
}

/// Assemble the `bwrap` argument vector for the given writable roots. Pure so it
/// can be tested on any platform. `/` is bound read-only, each writable root is
/// re-bound read-write, protected paths (e.g. `.git`) re-bound read-only, and
/// the real command is appended last.
#[cfg(any(target_os = "linux", test))]
fn bwrap_args(
    program: &str,
    args: &[String],
    deny_network: bool,
    roots: &[PathBuf],
    protected: &[PathBuf],
) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "--ro-bind".to_string(),
        "/".to_string(),
        "/".to_string(),
        "--dev".to_string(),
        "/dev".to_string(),
        "--proc".to_string(),
        "/proc".to_string(),
    ];
    for r in roots {
        let p = r.display().to_string();
        a.push("--bind".to_string());
        a.push(p.clone());
        a.push(p);
    }
    // After writable binds, re-lock .git (and similar) as read-only when present.
    for p in protected {
        if p.exists() {
            let s = p.display().to_string();
            a.push("--ro-bind".to_string());
            a.push(s.clone());
            a.push(s);
        }
    }
    if deny_network {
        a.push("--unshare-net".to_string());
    }
    a.push(program.to_string());
    a.extend_from_slice(args);
    a
}

/// The result of running a process.
#[derive(Debug, Clone)]
pub struct ProcessOutput {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    /// Whether output exceeded the cap and the middle was dropped.
    pub truncated: bool,
    /// How many bytes were dropped across both streams.
    pub dropped_bytes: u64,
}

impl ProcessOutput {
    pub fn success(&self) -> bool {
        self.exit_code == Some(0) && !self.timed_out
    }
}

/// Errors from running a process.
#[derive(Debug, thiserror::Error)]
pub enum ProcessError {
    #[error("failed to spawn `{program}`: {source}")]
    Spawn {
        program: String,
        source: std::io::Error,
    },
    #[error("io error while running `{program}`: {source}")]
    Io {
        program: String,
        source: std::io::Error,
    },
    #[error("command was cancelled")]
    Cancelled,
    #[error("{0}")]
    SandboxPolicy(String),
    /// Windows Job Object create/assign failed; process was not left running plain.
    #[error("process-tree (Job) setup failed: {0}")]
    ProcessTreeSetup(String),
}

/// Runs external commands.
#[derive(Debug, Clone)]
pub struct CommandRunner {
    environment: std::sync::Arc<leveler_core::EnvSnapshot>,
}

impl Default for CommandRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandRunner {
    pub fn new() -> Self {
        Self {
            environment: std::sync::Arc::new(leveler_core::environment().clone()),
        }
    }

    pub fn with_environment(environment: std::sync::Arc<leveler_core::EnvSnapshot>) -> Self {
        Self { environment }
    }

    /// Spawn the process and collect its output, honoring timeout and
    /// cancellation. stdout and stderr are drained concurrently so a chatty
    /// process cannot deadlock on a full pipe.
    pub async fn run(
        &self,
        request: ProcessRequest,
        cancellation: CancellationToken,
    ) -> Result<ProcessOutput, ProcessError> {
        // WS0/WS2: host-trusted intent (or legacy write_root mapping). On
        // Windows, restricted intents fail closed when FS backends are missing.
        let intent = request.filesystem_intent.clone().unwrap_or_else(|| {
            crate::windows_sandbox::FilesystemIntent::from_legacy(
                request.write_root.as_deref(),
                /* full_access */ request.write_root.is_none(),
            )
        });
        if let Err(err) =
            crate::windows_sandbox::assert_intent_spawn_allowed(&intent, request.deny_network)
        {
            return Err(ProcessError::SandboxPolicy(err.to_string()));
        }
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let sandbox_paths = request
            .write_root
            .as_ref()
            .map(|workspace| {
                prepare_sandbox_paths(
                    &self.environment,
                    workspace,
                    should_read_host_caches(&request),
                )
            })
            .transpose()
            .map_err(|source| {
                ProcessError::SandboxPolicy(format!(
                    "create private sandbox scratch directory: {source}"
                ))
            })?;
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let sandbox_scratch_root = sandbox_paths.as_ref().map(SandboxPaths::scratch_path);
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let sandbox_scratch_root: Option<&Path> = None;
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let sandbox_cache_write_roots = sandbox_paths
            .as_ref()
            .map(SandboxPaths::cache_write_roots)
            .unwrap_or(&[]);
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let sandbox_cache_write_roots: &[PathBuf] = &[];
        let (program, args) = sandbox_command(
            &request.program,
            &request.args,
            request.deny_network,
            request.write_root.as_deref(),
            &request.extra_read_roots,
            sandbox_scratch_root,
            sandbox_cache_write_roots,
        );

        #[cfg(windows)]
        {
            return run_windows_dispatch(
                request,
                intent,
                &program,
                &args,
                cancellation,
                self.environment.clone(),
            )
            .await;
        }

        #[cfg(not(windows))]
        {
            let _ = intent;
            run_unix_process_group(
                request,
                &program,
                &args,
                cancellation,
                &self.environment,
                #[cfg(any(target_os = "macos", target_os = "linux"))]
                sandbox_paths.as_ref(),
            )
            .await
        }
    }
}

/// Windows: Unrestricted → Job Object only; restricted intents → AppContainer FS.
#[cfg(windows)]
async fn run_windows_dispatch(
    request: ProcessRequest,
    intent: crate::windows_sandbox::FilesystemIntent,
    program: &str,
    args: &[String],
    cancellation: CancellationToken,
    environment: std::sync::Arc<leveler_core::EnvSnapshot>,
) -> Result<ProcessOutput, ProcessError> {
    use crate::windows_sandbox::FilesystemIntent;
    match intent {
        FilesystemIntent::Unrestricted => {
            run_with_windows_job(request, program, args, cancellation, &environment).await
        }
        FilesystemIntent::ReadOnly { .. } | FilesystemIntent::WorkspaceWrite { .. } => {
            crate::windows_appcontainer::run_appcontainer(
                request,
                intent,
                program,
                args,
                cancellation,
                environment,
            )
            .await
        }
    }
}

/// Linux: deliver SIGTERM to the child when this (parent) process dies — the
/// timeout/cancel paths already `killpg`, but a force-quit (`process::exit`,
/// SIGKILL, third Ctrl-C) runs no destructors and would orphan grandchildren
/// like `npm run dev`. macOS has no PDEATHSIG equivalent; process groups and
/// registry Drop reaping remain the cleanup there.
#[cfg(target_os = "linux")]
pub(crate) fn set_parent_death_signal(cmd: &mut Command) {
    // SAFETY: the pre-exec closure runs in the forked child before exec and
    // only calls `prctl`, which is async-signal-safe; it allocates nothing and
    // touches no locks.
    #[allow(unsafe_code)]
    unsafe {
        cmd.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
pub(crate) fn set_parent_death_signal(_cmd: &mut Command) {}

/// Unix path: process group + killpg for whole tree.
#[cfg(not(windows))]
async fn run_unix_process_group(
    request: ProcessRequest,
    program: &str,
    args: &[String],
    cancellation: CancellationToken,
    environment: &leveler_core::EnvSnapshot,
    #[cfg(any(target_os = "macos", target_os = "linux"))] sandbox_paths: Option<&SandboxPaths>,
) -> Result<ProcessOutput, ProcessError> {
    let mut cmd = Command::new(program);
    apply_common_command_env(&mut cmd, &request, args, environment);
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    if let Some(paths) = sandbox_paths {
        apply_sandbox_environment(&mut cmd, paths);
    }
    // Put the child in its own process group so we can terminate the whole
    // subtree (the child and any grandchildren) on timeout or cancellation.
    cmd.process_group(0);
    set_parent_death_signal(&mut cmd);

    let mut child = cmd.spawn().map_err(|source| ProcessError::Spawn {
        program: request.program.clone(),
        source,
    })?;

    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let cap = request.max_output_bytes;
    // A shared deadline that unblocks the pipe readers once the child has
    // exited (see below), so a detached grandchild holding the write end can't
    // wedge us on an EOF that never comes.
    let drain = CancellationToken::new();
    let stdout_task = {
        let drain = drain.clone();
        tokio::spawn(async move { read_capped(&mut stdout_pipe, cap, drain).await })
    };
    let stderr_task = {
        let drain = drain.clone();
        tokio::spawn(async move { read_capped(&mut stderr_pipe, cap, drain).await })
    };

    let child_pid = child.id();
    let mut timed_out = false;
    let status = tokio::select! {
        status = child.wait() => status,
        _ = tokio::time::sleep(request.timeout) => {
            timed_out = true;
            terminate_unix_tree(child_pid, &mut child).await;
            // Never block forever on a post-kill wait (unkillable / stuck
            // sandbox-exec children previously trapped cancel and the TUI).
            wait_child_deadline(&mut child).await
        }
        _ = cancellation.cancelled() => {
            terminate_unix_tree(child_pid, &mut child).await;
            let _ = wait_child_deadline(&mut child).await;
            return Err(ProcessError::Cancelled);
        }
    };

    let status = status.map_err(|source| ProcessError::Io {
        program: request.program.clone(),
        source,
    })?;

    // The child has exited. Its own output is already in the pipe buffers; only
    // a detached grandchild can still hold the write end open. Give the readers
    // a brief grace to drain, then cut them loose.
    {
        let drain = drain.clone();
        tokio::spawn(async move {
            tokio::time::sleep(PIPE_DRAIN_GRACE).await;
            drain.cancel();
        });
    }
    let (stdout, stdout_dropped) = stdout_task.await.unwrap_or_default();
    let (stderr, stderr_dropped) = stderr_task.await.unwrap_or_default();
    let dropped_bytes = stdout_dropped + stderr_dropped;
    // Reap any process still in the group so a foreground `cmd &` doesn't leave
    // an orphan running (no-op once the group is empty).
    reap_process_group(child_pid);

    Ok(ProcessOutput {
        exit_code: status.code(),
        stdout,
        stderr,
        timed_out,
        truncated: dropped_bytes > 0,
        dropped_bytes,
    })
}

/// How long the pipe readers keep draining after the child exits before giving
/// up on a write end still held open by a detached grandchild. The child's own
/// output is already buffered, so this only needs to cover reading it out.
const PIPE_DRAIN_GRACE: Duration = Duration::from_millis(250);

/// SIGKILL anything left in the child's process group. The child has already
/// exited; this only reaps detached grandchildren (`cmd &`) so a foreground run
/// leaves nothing behind. A no-op (`ESRCH`) when the group is already empty.
#[cfg(unix)]
fn reap_process_group(child_pid: Option<u32>) {
    if let Some(pid) = child_pid {
        use nix::sys::signal::{Signal, killpg};
        use nix::unistd::Pid;
        let _ = killpg(Pid::from_raw(pid as i32), Signal::SIGKILL);
    }
}

/// Windows path: Job Object via process-wrap (WS1). start_kill terminates the
/// whole job (grandchildren included). Job setup failure is typed — never plain
/// spawn without a job.
#[cfg(windows)]
async fn run_with_windows_job(
    request: ProcessRequest,
    program: &str,
    args: &[String],
    cancellation: CancellationToken,
    environment: &leveler_core::EnvSnapshot,
) -> Result<ProcessOutput, ProcessError> {
    use process_wrap::tokio::*;

    let mut cmd = Command::new(program);
    apply_common_command_env(&mut cmd, &request, args, environment);

    let mut wrap = TokioCommandWrap::from(cmd);
    wrap.wrap(JobObject);
    wrap.wrap(KillOnDrop);

    let mut child = wrap
        .spawn()
        .map_err(|source| map_windows_job_spawn_error(&request.program, source))?;

    let mut stdout_pipe = child.stdout().take();
    let mut stderr_pipe = child.stderr().take();
    let cap = request.max_output_bytes;
    let drain = CancellationToken::new();
    let stdout_task = {
        let drain = drain.clone();
        tokio::spawn(async move { read_capped(&mut stdout_pipe, cap, drain).await })
    };
    let stderr_task = {
        let drain = drain.clone();
        tokio::spawn(async move { read_capped(&mut stderr_pipe, cap, drain).await })
    };

    // Bound post-kill wait so a stuck job cannot trap the agent turn (same
    // failure mode as Unix child.wait hanging after killpg).
    let wait_deadline = async |child: &mut Box<dyn TokioChildWrapper>| match tokio::time::timeout(
        POST_KILL_WAIT,
        Box::into_pin(child.wait()),
    )
    .await
    {
        Ok(status) => status,
        Err(_elapsed) => {
            let _ = child.start_kill();
            match tokio::time::timeout(Duration::from_millis(500), Box::into_pin(child.wait()))
                .await
            {
                Ok(status) => status,
                Err(_elapsed) => {
                    use std::os::windows::process::ExitStatusExt;
                    Ok(std::process::ExitStatus::from_raw(1))
                }
            }
        }
    };

    let mut timed_out = false;
    let status = tokio::select! {
        status = Box::into_pin(child.wait()) => status,
        _ = tokio::time::sleep(request.timeout) => {
            timed_out = true;
            // JobObjectChild::start_kill terminates the entire job tree.
            let _ = child.start_kill();
            wait_deadline(&mut child).await
        }
        _ = cancellation.cancelled() => {
            let _ = child.start_kill();
            let _ = wait_deadline(&mut child).await;
            return Err(ProcessError::Cancelled);
        }
    };

    let status = status.map_err(|source| ProcessError::Io {
        program: request.program.clone(),
        source,
    })?;

    // Same drain bound as the Unix path: once the child exits, don't wait on a
    // pipe write end a detached job member might still hold. KillOnDrop reaps
    // the job when `child` drops at function exit.
    {
        let drain = drain.clone();
        tokio::spawn(async move {
            tokio::time::sleep(PIPE_DRAIN_GRACE).await;
            drain.cancel();
        });
    }
    let (stdout, stdout_dropped) = stdout_task.await.unwrap_or_default();
    let (stderr, stderr_dropped) = stderr_task.await.unwrap_or_default();
    let dropped_bytes = stdout_dropped + stderr_dropped;

    Ok(ProcessOutput {
        exit_code: status.code(),
        stdout,
        stderr,
        timed_out,
        truncated: dropped_bytes > 0,
        dropped_bytes,
    })
}

/// Map process-wrap / Job Object spawn failures to typed errors.
///
/// - `NotFound` → ordinary [`ProcessError::Spawn`] (bad program path)
/// - anything else during Job wrap/assign → [`ProcessError::ProcessTreeSetup`]
///   so callers never treat a failed job attach as a successful plain spawn.
///
/// Compiled for Windows (production path) and for tests on all hosts so the
/// mapping unit tests drive the same function `CommandRunner` uses.
#[cfg(any(windows, test))]
pub(crate) fn map_windows_job_spawn_error(program: &str, source: std::io::Error) -> ProcessError {
    if source.kind() == std::io::ErrorKind::NotFound {
        return ProcessError::Spawn {
            program: program.to_string(),
            source,
        };
    }
    ProcessError::ProcessTreeSetup(format!(
        "Job Object spawn/wrap failed for `{program}`: {source}"
    ))
}

fn apply_common_command_env(
    cmd: &mut Command,
    request: &ProcessRequest,
    args: &[String],
    environment: &leveler_core::EnvSnapshot,
) {
    cmd.args(args)
        .current_dir(&request.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    // Never inherit the live parent environment. Besides making execution
    // deterministic, this prevents a credential created after the application
    // snapshot from bypassing the scrub policy. Rebuild the child environment
    // solely from the immutable snapshot and apply deny/allow policy there.
    cmd.env_clear();
    for (name, value) in environment.vars_os() {
        let name_text = name.to_string_lossy();
        let explicitly_denied = request.deny_env.iter().any(|denied| denied == &name_text);
        let credential = is_credential_env_name(&name_text) || explicitly_denied;
        let explicitly_allowed = request
            .allow_env
            .iter()
            .any(|allowed| allowed == &name_text);
        if !credential || explicitly_allowed {
            cmd.env(name, value);
        }
    }
    // Prefer plain tool output for the transcript and the model; color is
    // useless in captured buffers and leaks as `[32m` if half-stripped.
    cmd.env("NO_COLOR", "1");
    cmd.env("FORCE_COLOR", "0");
    cmd.env("CLICOLOR", "0");
    cmd.env("CLICOLOR_FORCE", "0");
}

/// Terminate the child and its whole process group (Unix).
#[cfg(unix)]
async fn terminate_unix_tree(child_pid: Option<u32>, child: &mut tokio::process::Child) {
    if let Some(pid) = child_pid {
        use nix::sys::signal::{Signal, killpg};
        use nix::unistd::Pid;
        // SIGTERM the group, then SIGKILL as a backstop.
        let group = Pid::from_raw(pid as i32);
        let _ = killpg(group, Signal::SIGTERM);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let _ = killpg(group, Signal::SIGKILL);
    }
    child.start_kill().ok();
}

/// Upper bound for waiting after kill/timeout. Without this, a child that
/// never reaps (rare sandbox / zombie edge cases) holds the tool future, which
/// holds the whole turn, which holds the TUI in Busy with no escape.
const POST_KILL_WAIT: Duration = Duration::from_secs(2);

/// Wait for the child to exit, or give up after [`POST_KILL_WAIT`].
///
/// On deadline, attempt one more kill and return a synthetic signal-exit status
/// so callers can finish (timeout → timed_out; cancel → Cancelled) instead of
/// hanging the agent loop.
#[cfg(not(windows))]
async fn wait_child_deadline(
    child: &mut tokio::process::Child,
) -> Result<std::process::ExitStatus, std::io::Error> {
    match tokio::time::timeout(POST_KILL_WAIT, child.wait()).await {
        Ok(status) => status,
        Err(_elapsed) => {
            child.start_kill().ok();
            match tokio::time::timeout(Duration::from_millis(500), child.wait()).await {
                Ok(status) => status,
                Err(_elapsed) => {
                    // Fabricate a signalled exit so the tool path unwinds.
                    // Unix: status 9 ≈ SIGKILL. Prefer `from_raw` when available.
                    synthetic_killed_status()
                }
            }
        }
    }
}

#[cfg(not(windows))]
fn synthetic_killed_status() -> Result<std::process::ExitStatus, std::io::Error> {
    // `ExitStatus::from_raw(9)` is SIGKILL on Unix; used only when wait truly
    // will not return so the rest of the stack can still complete.
    use std::os::unix::process::ExitStatusExt;
    Ok(std::process::ExitStatus::from_raw(9))
}

/// Drain a pipe to EOF while keeping at most `cap` bytes in memory: the first
/// half is kept verbatim, the last half is a ring over the tail, and the
/// dropped middle is counted. The pipe is always drained so the child never
/// blocks on a full pipe. Returns the (lossy) text and the dropped byte count.
async fn read_capped(
    pipe: &mut Option<impl AsyncReadExt + Unpin>,
    cap: usize,
    drain: CancellationToken,
) -> (String, u64) {
    let Some(p) = pipe else {
        return (String::new(), 0);
    };
    let head_cap = cap / 2;
    let tail_cap = cap - head_cap;
    let mut head: Vec<u8> = Vec::new();
    let mut tail: std::collections::VecDeque<u8> = std::collections::VecDeque::new();
    let mut dropped: u64 = 0;
    let mut buf = [0u8; 16 * 1024];
    loop {
        // Stop reading once the drain deadline fires: after the child itself
        // exited, a detached grandchild (`cmd &`) can hold the write end open
        // indefinitely, and there is no more of the child's own output to get.
        let read = tokio::select! {
            biased;
            _ = drain.cancelled() => break,
            read = p.read(&mut buf) => read,
        };
        match read {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                for &byte in &buf[..n] {
                    if head.len() < head_cap {
                        head.push(byte);
                    } else {
                        if tail.len() == tail_cap {
                            tail.pop_front();
                            dropped += 1;
                        }
                        tail.push_back(byte);
                    }
                }
            }
        }
    }
    if dropped == 0 {
        head.extend(tail);
        return (String::from_utf8_lossy(&head).into_owned(), 0);
    }
    let mut text = String::from_utf8_lossy(&head).into_owned();
    text.push_str(&format!("\n…[{dropped} bytes dropped]…\n"));
    text.push_str(&String::from_utf8_lossy(tail.make_contiguous()));
    (text, dropped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn unix_host_runner() -> CommandRunner {
        CommandRunner::with_environment(std::sync::Arc::new(leveler_core::EnvSnapshot::new(
            std::env::vars_os(),
            std::env::current_dir().unwrap_or_default(),
            std::env::temp_dir(),
        )))
    }

    #[test]
    fn process_request_for_verify_check_matrix() {
        let root = PathBuf::from("/tmp/ws");
        let accept = process_request_for_verify_check(
            "sh",
            vec!["-c".into(), "test -d .".into()],
            root.clone(),
            VerifyNetworkPolicy::ForceDeny,
        );
        assert_eq!(accept.write_root.as_deref(), Some(root.as_path()));
        assert!(accept.deny_network, "model acceptance forces deny_network");
        assert!(matches!(
            accept.filesystem_intent,
            Some(crate::windows_sandbox::FilesystemIntent::WorkspaceWrite { .. })
        ));

        let repo = process_request_for_verify_check(
            "cargo",
            vec!["test".into()],
            root.clone(),
            VerifyNetworkPolicy::InheritSession,
        );
        assert_eq!(repo.write_root.as_deref(), Some(root.as_path()));
        assert!(
            !repo.deny_network,
            "repo verify inherits session network (not force deny)"
        );
        assert!(matches!(
            repo.filesystem_intent,
            Some(crate::windows_sandbox::FilesystemIntent::WorkspaceWrite { .. })
        ));
    }

    #[tokio::test]
    async fn runs_and_captures_stdout() {
        let out = CommandRunner::new()
            .run(
                ProcessRequest::new("echo", vec!["hello".into()], std::env::temp_dir()),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(out.success());
        assert_eq!(out.stdout.trim(), "hello");
    }

    /// A foreground command whose shell backgrounds a long-lived grandchild
    /// (`cmd &`) exits immediately, but the grandchild inherits the stdout pipe
    /// and holds its write end open. The runner must NOT block on the read task
    /// waiting for an EOF that won't come for 30s — it must drain the buffered
    /// output and return promptly. (This is the hang that `shell_guard` used to
    /// paper over with fragile command-string parsing.)
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[tokio::test]
    async fn fast_exit_does_not_hang_on_pipe_held_by_detached_grandchild() {
        let mut req = ProcessRequest::new(
            "sh",
            vec!["-c".into(), "echo hi; sleep 30 &".into()],
            std::env::temp_dir(),
        );
        req.timeout = Duration::from_secs(20);
        // The outer bound turns a hang into a failed assertion instead of a
        // 30s stall: the shell exits in milliseconds, so a correct runner
        // returns well under a second.
        let out = tokio::time::timeout(
            Duration::from_secs(5),
            unix_host_runner().run(req, CancellationToken::new()),
        )
        .await
        .expect("runner must return promptly, not hang on the detached grandchild's pipe")
        .expect("process ran");
        assert!(
            out.stdout.contains("hi"),
            "buffered output must survive: {out:?}"
        );
        assert!(
            !out.timed_out,
            "the shell exited normally; not a timeout: {out:?}"
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[tokio::test]
    async fn drain_deadline_is_not_starved_by_a_continuously_writable_pipe() {
        // The fallback killer makes this test self-cleaning even against the
        // broken implementation: the writer stops after two seconds. A correct
        // runner honors PIPE_DRAIN_GRACE and returns well before that.
        let mut req = ProcessRequest::new(
            "sh",
            vec![
                "-c".into(),
                "(while :; do echo x; done) & writer=$!; \
                 (sleep 2; kill \"$writer\") >/dev/null 2>&1 &"
                    .into(),
            ],
            std::env::temp_dir(),
        );
        req.timeout = Duration::from_secs(10);
        let started = std::time::Instant::now();
        let out = unix_host_runner()
            .run(req, CancellationToken::new())
            .await
            .expect("process ran");

        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a ready read branch must not starve the drain deadline"
        );
        assert!(out.stdout.contains('x'));
    }

    #[tokio::test]
    async fn an_already_fired_drain_deadline_wins_over_an_always_ready_reader() {
        use std::pin::Pin;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::task::{Context, Poll};
        use tokio::io::{AsyncRead, ReadBuf};

        struct AlwaysReady {
            remaining_reads: usize,
            reads: Arc<AtomicUsize>,
        }

        impl AsyncRead for AlwaysReady {
            fn poll_read(
                mut self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
                buf: &mut ReadBuf<'_>,
            ) -> Poll<std::io::Result<()>> {
                if self.remaining_reads == 0 {
                    return Poll::Ready(Ok(()));
                }
                self.remaining_reads -= 1;
                self.reads.fetch_add(1, Ordering::Relaxed);
                buf.put_slice(b"x");
                Poll::Ready(Ok(()))
            }
        }

        let reads = Arc::new(AtomicUsize::new(0));
        let mut pipe = Some(AlwaysReady {
            remaining_reads: 10_000,
            reads: reads.clone(),
        });
        let drain = CancellationToken::new();
        drain.cancel();
        let _ = read_capped(&mut pipe, 1024, drain).await;

        assert_eq!(
            reads.load(Ordering::Relaxed),
            0,
            "the fired deadline must be checked before a perpetually ready pipe"
        );
    }

    #[tokio::test]
    async fn large_output_is_bounded_in_memory() {
        let mut req = ProcessRequest::new(
            "sh",
            vec!["-c".into(), "yes x | head -c 1000000".into()],
            std::env::temp_dir(),
        );
        req.max_output_bytes = 64 * 1024;
        let out = CommandRunner::new()
            .run(req, CancellationToken::new())
            .await
            .unwrap();
        assert!(out.success(), "the process itself must finish normally");
        assert!(
            out.stdout.len() <= 64 * 1024 + 128,
            "captured stdout must stay near the cap, got {} bytes",
            out.stdout.len()
        );
        assert!(out.truncated);
        assert!(
            out.dropped_bytes > 900_000,
            "the dropped middle must be accounted for, got {}",
            out.dropped_bytes
        );
    }

    #[tokio::test]
    async fn bounded_output_keeps_head_and_tail() {
        let mut req = ProcessRequest::new(
            "sh",
            vec![
                "-c".into(),
                "echo START; yes filler | head -c 100000; echo; echo END".into(),
            ],
            std::env::temp_dir(),
        );
        req.max_output_bytes = 8 * 1024;
        let out = CommandRunner::new()
            .run(req, CancellationToken::new())
            .await
            .unwrap();
        assert!(out.stdout.starts_with("START"), "head must be preserved");
        assert!(
            out.stdout.trim_end().ends_with("END"),
            "tail must be preserved: …{:?}",
            &out.stdout[out.stdout.len().saturating_sub(40)..]
        );
        assert!(out.truncated);
    }

    #[tokio::test]
    async fn small_output_is_not_truncated() {
        let out = CommandRunner::new()
            .run(
                ProcessRequest::new("echo", vec!["hi".into()], std::env::temp_dir()),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!out.truncated);
        assert_eq!(out.dropped_bytes, 0);
        assert_eq!(out.stdout, "hi\n");
    }

    #[tokio::test]
    async fn nonzero_exit_is_reported() {
        let out = CommandRunner::new()
            .run(
                ProcessRequest::new(
                    "sh",
                    vec!["-c".into(), "exit 3".into()],
                    std::env::temp_dir(),
                ),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(out.exit_code, Some(3));
        assert!(!out.success());
    }

    #[tokio::test]
    async fn times_out() {
        let mut req = ProcessRequest::new("sleep", vec!["5".into()], std::env::temp_dir());
        req.timeout = Duration::from_millis(100);
        let out = CommandRunner::new()
            .run(req, CancellationToken::new())
            .await
            .unwrap();
        assert!(out.timed_out);
    }

    #[tokio::test]
    async fn cancellation_stops_it() {
        let token = CancellationToken::new();
        token.cancel();
        let err = CommandRunner::new()
            .run(
                ProcessRequest::new("sleep", vec!["5".into()], std::env::temp_dir()),
                token,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ProcessError::Cancelled));
    }

    /// Cancel must free the runner within a hard wall-clock bound (the product
    /// hang: cancel/force-cancel stayed Busy for minutes). Allow SIGTERM grace
    /// + POST_KILL_WAIT (2s) + margin.
    #[tokio::test]
    async fn cancellation_of_long_sleep_returns_within_bound() {
        let token = CancellationToken::new();
        let run_token = token.clone();
        let handle = tokio::spawn(async move {
            CommandRunner::new()
                .run(
                    ProcessRequest::new("sleep", vec!["120".into()], std::env::temp_dir()),
                    run_token,
                )
                .await
        });
        // Let the child actually start.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let start = std::time::Instant::now();
        token.cancel();
        let result = handle.await.unwrap();
        let elapsed = start.elapsed();
        assert!(
            matches!(result, Err(ProcessError::Cancelled)),
            "expected Cancelled, got {result:?}"
        );
        assert!(
            elapsed < Duration::from_secs(4),
            "cancel must complete within 4s (term + post-kill wait), took {elapsed:?}"
        );
    }

    #[test]
    fn bwrap_confines_writes_to_roots() {
        let roots = vec![PathBuf::from("/ws"), PathBuf::from("/tmp")];
        let a = bwrap_args("go", &["build".into()], true, &roots, &[]);
        assert!(
            a.windows(3)
                .any(|w| w[0] == "--ro-bind" && w[1] == "/" && w[2] == "/"),
            "binds / read-only: {a:?}"
        );
        assert!(
            a.windows(3)
                .any(|w| w[0] == "--bind" && w[1] == "/ws" && w[2] == "/ws"),
            "workspace re-bound writable: {a:?}"
        );
        assert!(
            a.windows(3)
                .any(|w| w[0] == "--bind" && w[1] == "/tmp" && w[2] == "/tmp"),
            "tmp re-bound writable: {a:?}"
        );
        assert!(
            a.iter().any(|s| s == "--unshare-net"),
            "network denied: {a:?}"
        );
        let go = a.iter().position(|s| s == "go").expect("command present");
        assert_eq!(a[go + 1], "build", "command args follow the program");
    }

    #[test]
    fn bwrap_shares_network_when_allowed() {
        let a = bwrap_args(
            "curl",
            &["x".into()],
            false,
            &[PathBuf::from("/ws")],
            &[PathBuf::from("/ws/.git")],
        );
        assert!(
            a.windows(3)
                .any(|w| { w[0] == "--ro-bind" && w[1] == "/ws/.git" && w[2] == "/ws/.git" })
                || !PathBuf::from("/ws/.git").exists(),
            "protected .git re-bound ro when present: {a:?}"
        );
        let a = bwrap_args("curl", &["x".into()], false, &[PathBuf::from("/ws")], &[]);
        assert!(
            !a.iter().any(|s| s == "--unshare-net"),
            "network shared: {a:?}"
        );
    }

    #[test]
    fn writable_roots_exclude_shared_temp_and_host_tool_directories() {
        let workspace = tempfile::tempdir().expect("workspace");
        let scratch = tempfile::tempdir().expect("scratch");
        let tool_cache = tempfile::tempdir().expect("tool cache");
        let cache_roots = vec![tool_cache.path().to_path_buf()];
        let roots = writable_roots(workspace.path(), Some(scratch.path()), &cache_roots);

        assert_eq!(
            roots.len(),
            3,
            "only workspace, private scratch, and Leveler tool cache: {roots:?}"
        );
        assert!(roots.contains(&workspace.path().canonicalize().unwrap()));
        assert!(roots.contains(&scratch.path().canonicalize().unwrap()));
        assert!(roots.contains(&tool_cache.path().canonicalize().unwrap()));
        for forbidden in [
            std::env::temp_dir(),
            PathBuf::from("/tmp"),
            PathBuf::from("/var/tmp"),
        ] {
            let forbidden = forbidden.canonicalize().unwrap_or(forbidden);
            assert!(
                !roots.contains(&forbidden),
                "shared temp root must remain read-only: {forbidden:?}"
            );
        }
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            for relative in [".cargo", ".npm", ".local", "Library/Application Support"] {
                let path = home.join(relative);
                let path = path.canonicalize().unwrap_or(path);
                assert!(
                    !roots.contains(&path),
                    "host tool/config directory must remain read-only: {path:?}"
                );
            }
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn private_paths_separate_ephemeral_temp_and_persistent_build_caches() {
        let base = tempfile::tempdir().expect("base");
        let leveler_home = base.path().join("leveler-home");
        let workspace = base.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let environment = leveler_core::EnvSnapshot::new(
            [(
                std::ffi::OsString::from("LEVELER_HOME"),
                leveler_home.as_os_str().to_os_string(),
            )],
            PathBuf::new(),
            base.path().to_path_buf(),
        );
        let paths = prepare_sandbox_paths(&environment, &workspace, false).expect("private paths");
        assert!(paths.scratch_path().join("tmp").is_dir());
        for relative in [
            "cargo",
            "go/build",
            "go/mod",
            "go/path",
            "npm",
            "pip",
            "xdg-cache",
        ] {
            assert!(
                paths.tool_cache_path().join(relative).is_dir(),
                "missing private cache directory {relative}"
            );
        }
        assert_eq!(
            paths.scratch_path().parent(),
            Some(leveler_home.canonicalize().unwrap().as_path())
        );
        assert!(
            paths
                .tool_cache_path()
                .starts_with(leveler_home.canonicalize().unwrap())
        );

        let second = prepare_sandbox_paths(&environment, &workspace, false).expect("second paths");
        assert_ne!(paths.scratch_path(), second.scratch_path());
        assert_eq!(paths.tool_cache_path(), second.tool_cache_path());
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn private_paths_fall_back_to_captured_temp_without_home_authority() {
        use std::os::unix::fs::PermissionsExt as _;

        let base = tempfile::tempdir().unwrap();
        let workspace = base.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let environment = leveler_core::EnvSnapshot::new(
            std::iter::empty::<(std::ffi::OsString, std::ffi::OsString)>(),
            workspace.clone(),
            base.path().to_path_buf(),
        );

        let paths = prepare_sandbox_paths(&environment, &workspace, false).unwrap();
        let owner = paths.scratch_path().parent().unwrap();
        assert_eq!(
            owner.parent(),
            Some(base.path().canonicalize().unwrap().as_path())
        );
        assert!(
            owner
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("codeleveler-private-")
        );
        assert_eq!(
            std::fs::metadata(owner).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert!(paths.tool_cache_path().starts_with(owner));
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn private_paths_reject_poisoned_temp_without_home_authority() {
        let base = tempfile::tempdir().unwrap();
        let workspace = base.path().join("workspace");
        let outside = base.path().join("outside");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let poisoned_temp = workspace.join("temp-link");
        std::os::unix::fs::symlink(&outside, &poisoned_temp).unwrap();
        let environment = leveler_core::EnvSnapshot::new(
            std::iter::empty::<(std::ffi::OsString, std::ffi::OsString)>(),
            workspace.clone(),
            poisoned_temp,
        );

        let error = prepare_sandbox_paths(&environment, &workspace, false)
            .err()
            .expect("poisoned temp must fail closed");
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(std::fs::read_dir(&outside).unwrap().next().is_none());
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn concurrent_private_cache_repair_is_race_safe() {
        let base = tempfile::tempdir().unwrap();
        let workspace = base.path().join("workspace");
        let home = base.path().join("home");
        let outside = base.path().join("outside");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("sentinel"), "unchanged").unwrap();
        let environment = std::sync::Arc::new(leveler_core::EnvSnapshot::new(
            [
                ("HOME".into(), home.clone().into_os_string()),
                ("LEVELER_HOME".into(), home.join("leveler").into_os_string()),
            ],
            base.path().to_path_buf(),
            home.join("tmp"),
        ));
        let initialized = prepare_sandbox_paths(&environment, &workspace, false).unwrap();
        let registry = initialized.tool_cache_path().join("cargo/registry");
        drop(initialized);
        std::fs::remove_dir(&registry).unwrap();
        std::os::unix::fs::symlink(&outside, &registry).unwrap();

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(12));
        let mut threads = Vec::new();
        for _ in 0..12 {
            let barrier = barrier.clone();
            let environment = environment.clone();
            let workspace = workspace.clone();
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                prepare_sandbox_paths(&environment, &workspace, false).map(drop)
            }));
        }
        for thread in threads {
            thread.join().unwrap().unwrap();
        }
        assert!(registry.is_dir());
        assert!(
            !std::fs::symlink_metadata(&registry)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_to_string(outside.join("sentinel")).unwrap(),
            "unchanged"
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[tokio::test]
    async fn cache_symlink_poisoning_cannot_escape_host_initialization() {
        #[cfg(target_os = "linux")]
        if std::process::Command::new("bwrap")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipping: bubblewrap is not installed");
            return;
        }

        let base = tempfile::tempdir().expect("base");
        let workspace = base.path().join("workspace");
        let safe_home = base.path().join("safe-home");
        let outside = base.path().join("outside");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&safe_home).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let sentinel = outside.join("sentinel");
        std::fs::write(&sentinel, "unchanged").unwrap();

        // Both configured state/temp paths are attacker-controlled workspace
        // links. Cache and scratch selection must ignore them and use HOME.
        let poisoned_leveler_home = workspace.join("poisoned-leveler-home");
        let poisoned_tmp = workspace.join("poisoned-tmp");
        let poisoned_cargo_home = workspace.join("poisoned-cargo-home");
        std::os::unix::fs::symlink(&outside, &poisoned_leveler_home).unwrap();
        std::os::unix::fs::symlink(&outside, &poisoned_tmp).unwrap();
        std::os::unix::fs::symlink(&outside, &poisoned_cargo_home).unwrap();
        let mut variables: Vec<_> = std::env::vars_os().collect();
        variables.push(("HOME".into(), safe_home.clone().into_os_string()));
        variables.push((
            "LEVELER_HOME".into(),
            poisoned_leveler_home.into_os_string(),
        ));
        variables.push(("CARGO_HOME".into(), poisoned_cargo_home.into_os_string()));
        let environment = std::sync::Arc::new(leveler_core::EnvSnapshot::new(
            variables,
            std::env::current_dir().unwrap(),
            poisoned_tmp,
        ));

        let paths = prepare_sandbox_paths(&environment, &workspace, false).unwrap();
        let safe_home = safe_home.canonicalize().unwrap();
        assert!(paths.tool_cache_path().starts_with(&safe_home));
        assert!(paths.scratch_path().starts_with(&safe_home));
        let registry_root = paths.tool_cache_path().join("cargo/registry");
        drop(paths);

        // Simulate a leaf symlink left by an older vulnerable process. The
        // capability-relative initializer must unlink only the poisoned entry,
        // recreate a real leaf, and leave the target untouched.
        std::fs::remove_dir(&registry_root).unwrap();
        std::os::unix::fs::symlink(&outside, &registry_root).unwrap();
        let repaired = prepare_sandbox_paths(&environment, &workspace, false).unwrap();
        assert!(registry_root.is_dir());
        assert!(
            !std::fs::symlink_metadata(&registry_root)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "unchanged");
        drop(repaired);

        // A confined command may modify cache contents, but cannot unlink a
        // trusted leaf mount and replace it with a link to an arbitrary target.
        let runner = CommandRunner::with_environment(environment);
        let script = "target=$(readlink \"$CARGO_HOME/registry\")\nrm -rf \"$target\" || exit 91\nln -s \"$1\" \"$target\"";
        let mut request = ProcessRequest::new(
            "sh",
            vec![
                "-c".into(),
                script.into(),
                "sh".into(),
                outside.display().to_string(),
            ],
            workspace.clone(),
        );
        request.write_root = Some(workspace.clone());
        let output = runner
            .run(request, CancellationToken::new())
            .await
            .expect("run cache poisoning attempt");
        assert!(
            !output.success(),
            "cache-root replacement must fail: {output:?}"
        );
        assert!(registry_root.is_dir());
        assert!(
            !std::fs::symlink_metadata(&registry_root)
                .unwrap()
                .file_type()
                .is_symlink()
        );

        // The next trusted initialization is safe even after the attack.
        let _next = prepare_sandbox_paths(runner.environment.as_ref(), &workspace, false).unwrap();
        assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "unchanged");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[tokio::test]
    async fn confined_common_builds_use_private_temp_and_persistent_cache() {
        #[cfg(target_os = "linux")]
        if std::process::Command::new("bwrap")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipping: bubblewrap is not installed");
            return;
        }

        let base = tempfile::tempdir().expect("base");
        let workspace = base.path().join("workspace");
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        std::fs::write(
            workspace.join("Cargo.toml"),
            "[package]\nname = \"sandbox-smoke\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        std::fs::write(workspace.join("src/main.rs"), "fn main() {}\n").unwrap();

        let mut variables: Vec<_> = std::env::vars_os()
            .filter(|(name, _)| name != "CARGO_TARGET_DIR")
            .collect();
        variables.push((
            "LEVELER_HOME".into(),
            base.path().join("home").into_os_string(),
        ));
        let environment = std::sync::Arc::new(leveler_core::EnvSnapshot::new(
            variables,
            std::env::current_dir().unwrap(),
            std::env::temp_dir(),
        ));
        let runner = CommandRunner::with_environment(environment);

        let mut private_tmp = ProcessRequest::new(
            "sh",
            vec![
                "-c".into(),
                "test -n \"$TMPDIR\" && touch \"$TMPDIR/allowed\"".into(),
            ],
            workspace.clone(),
        );
        private_tmp.write_root = Some(workspace.clone());
        let output = runner
            .run(private_tmp, CancellationToken::new())
            .await
            .expect("write private TMPDIR");
        assert!(
            output.success(),
            "private TMPDIR must be writable: {output:?}"
        );

        let global_tmp_target = base.path().parent().unwrap().join(format!(
            "codeleveler-global-temp-canary-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&global_tmp_target);
        let mut shared_tmp = ProcessRequest::new(
            "sh",
            vec![
                "-c".into(),
                "touch \"$1\"".into(),
                "sh".into(),
                global_tmp_target.display().to_string(),
            ],
            workspace.clone(),
        );
        shared_tmp.write_root = Some(workspace.clone());
        let output = runner
            .run(shared_tmp, CancellationToken::new())
            .await
            .expect("try shared temp write");
        assert!(
            !output.success() && !global_tmp_target.exists(),
            "shared temp tree must stay read-only: {output:?}"
        );

        for _ in 0..2 {
            let mut request = ProcessRequest::new(
                "cargo",
                vec!["check".into(), "--offline".into(), "--quiet".into()],
                workspace.clone(),
            );
            request.write_root = Some(workspace.clone());
            let output = runner
                .run(request, CancellationToken::new())
                .await
                .expect("run confined cargo");
            assert!(
                output.success(),
                "confined cargo check failed: stdout={} stderr={}",
                output.stdout,
                output.stderr
            );
        }

        if std::process::Command::new("go")
            .arg("version")
            .output()
            .is_ok()
        {
            let go_workspace = base.path().join("go-workspace");
            std::fs::create_dir(&go_workspace).unwrap();
            std::fs::write(
                go_workspace.join("go.mod"),
                "module sandbox-smoke\n\ngo 1.22\n",
            )
            .unwrap();
            std::fs::write(
                go_workspace.join("main.go"),
                "package main\nfunc main() {}\n",
            )
            .unwrap();
            let mut request = ProcessRequest::new(
                "go",
                vec!["build".into(), "./...".into()],
                go_workspace.clone(),
            );
            request.write_root = Some(go_workspace);
            let output = runner
                .run(request, CancellationToken::new())
                .await
                .expect("run confined go");
            assert!(output.success(), "confined go build failed: {output:?}");
        }

        if std::process::Command::new("npm")
            .arg("--version")
            .output()
            .is_ok()
        {
            let npm_workspace = base.path().join("npm-workspace");
            std::fs::create_dir(&npm_workspace).unwrap();
            std::fs::write(
                npm_workspace.join("package.json"),
                r#"{"name":"sandbox-smoke","version":"0.0.0","scripts":{"build":"node -e \"require('fs').writeFileSync('built.txt','ok')\""}}"#,
            )
            .unwrap();
            let mut request = ProcessRequest::new(
                "npm",
                vec!["run".into(), "build".into(), "--silent".into()],
                npm_workspace.clone(),
            );
            request.write_root = Some(npm_workspace.clone());
            let output = runner
                .run(request, CancellationToken::new())
                .await
                .expect("run confined npm");
            assert!(output.success(), "confined npm build failed: {output:?}");
            assert!(npm_workspace.join("built.txt").is_file());
        }
        assert!(base.path().join("home/tool-cache").is_dir());
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[tokio::test]
    async fn confined_offline_cargo_reuses_readonly_host_cache_and_config() {
        #[cfg(target_os = "linux")]
        if std::process::Command::new("bwrap")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipping: bubblewrap is not installed");
            return;
        }

        let base = tempfile::tempdir().expect("base");
        let dependency = base.path().join("dependency");
        std::fs::create_dir_all(dependency.join("src")).unwrap();
        std::fs::write(
            dependency.join("Cargo.toml"),
            "[package]\nname = \"host-cached-dep\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        std::fs::write(
            dependency.join("src/lib.rs"),
            "pub fn answer() -> u8 { 42 }\n",
        )
        .unwrap();
        let git = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&dependency)
            .status()
            .unwrap();
        assert!(git.success());
        for args in [
            ["config", "user.email", "test@example.invalid"].as_slice(),
            ["config", "user.name", "CodeLeveler Test"].as_slice(),
            ["add", "."].as_slice(),
            ["commit", "-qm", "initial"].as_slice(),
        ] {
            assert!(
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(&dependency)
                    .status()
                    .unwrap()
                    .success()
            );
        }

        let workspace = base.path().join("workspace");
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        let dependency_url = format!("file://{}", dependency.canonicalize().unwrap().display());
        std::fs::write(
            workspace.join("Cargo.toml"),
            format!(
                "[package]\nname = \"offline-consumer\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[dependencies]\nhost-cached-dep = {{ git = {dependency_url:?} }}\n"
            ),
        )
        .unwrap();
        std::fs::write(
            workspace.join("src/main.rs"),
            "fn main() { assert_eq!(host_cached_dep::answer(), 42); }\n",
        )
        .unwrap();

        // Warm only the host Cargo cache, then remove both the original git
        // source and build output. The confined build below can succeed only by
        // reading the host cache through the read-only overlay.
        let host_cargo = base.path().join("host-cargo");
        let warm_target = base.path().join("warm-target");
        let warm = std::process::Command::new("cargo")
            .args(["check", "--quiet"])
            .env("CARGO_HOME", &host_cargo)
            .env("CARGO_TARGET_DIR", &warm_target)
            .current_dir(&workspace)
            .status()
            .expect("warm host cargo cache");
        assert!(warm.success());
        assert!(host_cargo.join("git").is_dir());
        std::fs::remove_dir_all(&dependency).unwrap();
        std::fs::remove_dir_all(&warm_target).unwrap();

        let configured_target = workspace.join("configured-target");
        let config = format!(
            "[build]\ntarget-dir = {target:?}\n\n[env]\nLEVELER_CARGO_CONFIG_CANARY = \"from-host-config\"\n\n[registries.company]\nindex = \"https://example.invalid/index\"\n",
            target = configured_target.display().to_string()
        );
        std::fs::write(host_cargo.join("config.toml"), &config).unwrap();
        std::fs::write(
            workspace.join("src/main.rs"),
            "const _: &str = env!(\"LEVELER_CARGO_CONFIG_CANARY\");\nfn main() { assert_eq!(host_cached_dep::answer(), 42); }\n",
        )
        .unwrap();

        let leveler_home = base.path().join("leveler-home");
        assert!(!leveler_home.exists(), "private cache starts empty");
        let mut variables: Vec<_> = std::env::vars_os()
            .filter(|(name, _)| name != "CARGO_TARGET_DIR")
            .collect();
        variables.push(("CARGO_HOME".into(), host_cargo.clone().into_os_string()));
        variables.push(("LEVELER_HOME".into(), leveler_home.clone().into_os_string()));
        let environment = std::sync::Arc::new(leveler_core::EnvSnapshot::new(
            variables,
            std::env::current_dir().unwrap(),
            std::env::temp_dir(),
        ));
        let runner = CommandRunner::with_environment(environment);
        let mut request = ProcessRequest::new(
            "cargo",
            vec!["check".into(), "--offline".into(), "--quiet".into()],
            workspace.clone(),
        );
        request.write_root = Some(workspace.clone());
        request.deny_network = true;
        let output = runner
            .run(request, CancellationToken::new())
            .await
            .expect("run deny-network offline cargo");
        assert!(
            output.success(),
            "offline host-cache build failed: stdout={} stderr={}",
            output.stdout,
            output.stderr
        );
        assert!(
            configured_target.is_dir(),
            "host config.toml target-dir must be applied"
        );
        assert_eq!(
            std::fs::read_to_string(host_cargo.join("config.toml")).unwrap(),
            config
        );

        let host_write_canary = host_cargo.join("git/host-write-canary");
        let mut write_host_cache = ProcessRequest::new(
            "sh",
            vec![
                "-c".into(),
                "printf tampered > \"$CARGO_HOME/git/host-write-canary\"".into(),
            ],
            workspace.clone(),
        );
        write_host_cache.write_root = Some(workspace.clone());
        write_host_cache.deny_network = true;
        let output = runner
            .run(write_host_cache, CancellationToken::new())
            .await
            .expect("try host cache write through overlay");
        assert!(
            !output.success() && !host_write_canary.exists(),
            "host cache symlink target must remain read-only: {output:?}"
        );

        let workspace_cache = std::fs::read_dir(leveler_home.join("tool-cache"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert!(
            std::fs::read_dir(workspace_cache.join("cargo/git"))
                .unwrap()
                .next()
                .is_none(),
            "private cache began empty; offline build must not copy or mutate host git cache"
        );
    }

    #[test]
    fn absolute_arg_outside_roots_is_detected() {
        // Absolute-path spelling differs by platform.
        #[cfg(not(windows))]
        let (allowed, outside) = (PathBuf::from("/tmp"), "/etc/hosts");
        #[cfg(windows)]
        let (allowed, outside) = (PathBuf::from(r"C:\ws"), r"C:\Windows\System32\etc\hosts");
        let allowed = vec![allowed];
        assert!(
            first_absolute_arg_outside_roots(&["hello".into(), outside.into()], &allowed).is_some()
        );
        assert!(first_absolute_arg_outside_roots(&["-n".into(), "hi".into()], &allowed).is_none());
    }

    #[test]
    fn windows_style_absolute_args_are_recognized_on_all_hosts() {
        // Cross-compile docs / model transcripts often use Windows paths even
        // when the agent runs on Unix; detect them so preflight stays useful.
        assert!(looks_like_absolute_path_arg(r"C:\Users\me\other\AGENTS.md"));
        assert!(looks_like_absolute_path_arg(r"D:/work/repo/file.rs"));
        assert!(looks_like_absolute_path_arg(r"\\server\share\file.txt"));
        assert!(!looks_like_absolute_path_arg(r"relative\path.txt"));
        assert!(!looks_like_absolute_path_arg("-LiteralPath"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_path_under_is_case_insensitive() {
        let root = PathBuf::from(r"C:\Users\Me\proj");
        let child = PathBuf::from(r"c:\users\me\proj\src\main.rs");
        assert!(path_is_under(&child, &root));
        assert!(!path_is_under(
            Path::new(r"C:\Users\Me\other\secret.txt"),
            &root
        ));
    }

    #[test]
    fn sandbox_passthrough_when_unconfined() {
        // No network deny, no write confinement → run the command as-is.
        let (p, a) = sandbox_command("cargo", &["test".into()], false, None, &[], None, &[]);
        assert_eq!(p, "cargo");
        assert_eq!(a, vec!["test".to_string()]);
    }

    #[test]
    fn sandbox_wraps_when_denying_network() {
        let (program, args) =
            sandbox_command("cargo", &["build".into()], true, None, &[], None, &[]);
        #[cfg(target_os = "macos")]
        {
            assert_eq!(program, "/usr/bin/sandbox-exec");
            assert!(args.iter().any(|a| a.contains("deny network")));
            assert!(args.contains(&"cargo".to_string()));
            assert!(args.contains(&"build".to_string()));
        }
        #[cfg(target_os = "linux")]
        {
            assert_eq!(program, "unshare");
            assert!(args.contains(&"--net".to_string()));
            assert!(args.contains(&"cargo".to_string()));
            assert!(args.contains(&"build".to_string()));
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            assert_eq!(program, "cargo");
            let _ = args;
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_confines_writes_via_params() {
        let root = std::path::Path::new("/tmp");
        let scratch = tempfile::tempdir().expect("scratch");
        let cache_roots = vec![scratch.path().to_path_buf()];
        let (program, args) = sandbox_command(
            "touch",
            &["x".into()],
            true,
            Some(root),
            &[],
            Some(scratch.path()),
            &cache_roots,
        );
        assert_eq!(program, "/usr/bin/sandbox-exec");
        let policy = &args[1];
        assert!(
            policy.contains("(deny default)"),
            "closed by default: {policy}"
        );
        assert!(
            policy.contains("(allow file-read*)"),
            "broad-read workspace policy: {policy}"
        );
        assert!(
            !policy.contains("USER_READ_ROOT") && !policy.contains("(deny file-read*"),
            "home trees must not be read-gated: {policy}"
        );
        assert!(
            policy.contains("(allow file-write*") && policy.contains("(param \"WRITABLE_ROOT_0\")"),
            "writes go through a param, not an interpolated path: {policy}"
        );
        assert!(
            policy.contains("PROTECTED_WRITE") && policy.contains("(deny file-write*"),
            "workspace .git must be write-denied: {policy}"
        );
        // The path is a -D param, not inlined into the policy body.
        assert!(
            args.iter().any(|a| a.starts_with("-DWRITABLE_ROOT_0=")),
            "writable root passed as -D param: {args:?}"
        );
        // Command is placed after the `--` separator.
        let sep = args
            .iter()
            .position(|a| a == "--")
            .expect("has -- separator");
        assert_eq!(args[sep + 1], "touch");
    }

    // The real proof: a sandboxed process may write inside the workspace but not
    // outside it. Runs actual `sandbox-exec`. The "outside" target lives under
    // $HOME (not a writable root and not a temp dir, which IS writable).
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn seatbelt_blocks_writes_outside_the_workspace() {
        let home = PathBuf::from(std::env::var("HOME").expect("HOME set"));
        let base = home.join(format!(".leveler-sbtest-{}", std::process::id()));
        let ws = base.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let ws = ws.canonicalize().unwrap();
        let runner = unix_host_runner();

        // Write inside the workspace: allowed.
        let mut inside = ProcessRequest::new(
            "sh",
            vec!["-c".into(), "echo hi > ok.txt".into()],
            ws.clone(),
        );
        inside.write_root = Some(ws.clone());
        let out = runner.run(inside, CancellationToken::new()).await.unwrap();
        assert!(
            out.success(),
            "write inside workspace should succeed: {out:?}"
        );
        assert!(ws.join("ok.txt").exists());

        // Write to a sibling under $HOME (outside every writable root): blocked.
        let escape = base.join("escape.txt");
        let _ = std::fs::remove_file(&escape);
        let mut out_req = ProcessRequest::new(
            "sh",
            vec!["-c".into(), format!("echo x > {}", escape.display())],
            ws.clone(),
        );
        out_req.write_root = Some(ws.clone());
        let out = runner.run(out_req, CancellationToken::new()).await.unwrap();
        assert!(
            !out.success(),
            "write outside workspace must be blocked: {out:?}"
        );

        // Reads outside the workspace succeed (e.g. ~/.gitconfig).
        // Host-side run_command arg preflight still blocks model-supplied absolute
        // paths; this canary is OS seatbelt only.
        let secret = base.join("secret.txt");
        std::fs::write(&secret, "classified\n").unwrap();
        let mut read_req =
            ProcessRequest::new("cat", vec![secret.display().to_string()], ws.clone());
        read_req.write_root = Some(ws.clone());
        let out = runner
            .run(read_req, CancellationToken::new())
            .await
            .unwrap();
        assert!(
            out.success(),
            "read outside workspace should be allowed under the broad-read policy: {out:?}"
        );
        assert!(
            out.stdout.contains("classified"),
            "stdout: {:?}",
            out.stdout
        );
        let _ = std::fs::remove_file(&secret);
        assert!(
            !escape.exists(),
            "the outside file must not have been created"
        );

        // Workspace .git must not be writable under confinement.
        let git = ws.join(".git");
        std::fs::create_dir_all(&git).unwrap();
        let mut git_write = ProcessRequest::new(
            "sh",
            vec![
                "-c".into(),
                "echo pwned > .git/evil && cat .git/evil".into(),
            ],
            ws.clone(),
        );
        git_write.write_root = Some(ws.clone());
        let out = runner
            .run(git_write, CancellationToken::new())
            .await
            .unwrap();
        assert!(
            !out.success() || !git.join("evil").exists(),
            "write into .git must be blocked: {out:?}"
        );
        // Normal workspace write still ok.
        let mut ok_write = ProcessRequest::new(
            "sh",
            vec!["-c".into(), "echo fine > normal.txt".into()],
            ws.clone(),
        );
        ok_write.write_root = Some(ws.clone());
        let out = runner
            .run(ok_write, CancellationToken::new())
            .await
            .unwrap();
        assert!(out.success(), "normal workspace write: {out:?}");
        assert!(ws.join("normal.txt").exists());

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn git_write_protected_paths_point_at_dot_git() {
        let root = PathBuf::from("/tmp/proj");
        let paths = git_write_protected_paths(&root);
        assert_eq!(paths.len(), 1);
        assert!(paths[0].ends_with(".git"));
    }

    /// D4 canary (macOS): confined write_root blocks writes under `.git`;
    /// dropping write_root (A7 unrestricted) allows them again.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn confined_git_write_fails_unrestricted_git_write_ok() {
        let home = PathBuf::from(std::env::var("HOME").expect("HOME"));
        let base = home.join(format!(
            ".leveler-git-canary-{}",
            std::process::id() as u64 * 17 + 3
        ));
        let ws = base.join("ws");
        std::fs::create_dir_all(ws.join(".git")).unwrap();
        let runner = unix_host_runner();

        let marker = ws.join(".git/index.lock");
        let _ = std::fs::remove_file(&marker);

        let mut confined = ProcessRequest::new(
            "sh",
            vec!["-c".into(), "echo lock > .git/index.lock".into()],
            ws.clone(),
        );
        confined.write_root = Some(ws.clone());
        let out = runner
            .run(confined, CancellationToken::new())
            .await
            .unwrap();
        assert!(
            !out.success() || !marker.exists(),
            "confined must block .git/index.lock write: {out:?}"
        );
        let _ = std::fs::remove_file(&marker);

        // No write_root → same as turn_unrestricted_fs / full-access path.
        let free = ProcessRequest::new(
            "sh",
            vec!["-c".into(), "echo lock > .git/index.lock".into()],
            ws.clone(),
        );
        let out = runner.run(free, CancellationToken::new()).await.unwrap();
        assert!(out.success(), "unrestricted must allow .git write: {out:?}");
        assert!(marker.exists(), "index.lock should exist after elevation");
        assert!(
            std::fs::read_to_string(&marker).unwrap().contains("lock"),
            "contents written"
        );

        std::fs::remove_dir_all(&base).ok();
    }

    // Real Linux canary. CI installs bubblewrap; local machines without it skip
    // this platform proof while the pure argv tests still run everywhere.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn bubblewrap_blocks_writes_outside_the_workspace() {
        if std::process::Command::new("bwrap")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipping: bubblewrap is not installed");
            return;
        }

        let home = PathBuf::from(std::env::var("HOME").expect("HOME set"));
        let base = home.join(format!(".leveler-bwrap-test-{}", std::process::id()));
        let ws = base.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let ws = ws.canonicalize().unwrap();
        let runner = unix_host_runner();

        let mut inside = ProcessRequest::new(
            "sh",
            vec!["-c".into(), "echo hi > ok.txt".into()],
            ws.clone(),
        );
        inside.write_root = Some(ws.clone());
        let output = runner.run(inside, CancellationToken::new()).await.unwrap();
        assert!(
            output.success(),
            "write inside workspace should succeed: {output:?}"
        );
        assert!(ws.join("ok.txt").exists());

        let escape = base.join("escape.txt");
        let mut outside = ProcessRequest::new(
            "sh",
            vec!["-c".into(), format!("echo x > {}", escape.display())],
            ws.clone(),
        );
        outside.write_root = Some(ws.clone());
        let output = runner.run(outside, CancellationToken::new()).await.unwrap();
        assert!(
            !output.success(),
            "write outside workspace must be blocked: {output:?}"
        );
        assert!(!escape.exists());

        std::fs::remove_dir_all(&base).ok();
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn sandboxed_non_network_command_still_runs() {
        let mut req = ProcessRequest::new("echo", vec!["hi".into()], std::env::temp_dir());
        req.deny_network = true;
        let out = unix_host_runner()
            .run(req, CancellationToken::new())
            .await
            .unwrap();
        assert!(out.success());
        assert_eq!(out.stdout.trim(), "hi");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_kills_grandchildren() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "leveler-tree-{}",
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let pidfile = dir.join("gc.pid");

        // sh (group leader) spawns a background sleep (grandchild), records its
        // pid, then waits. Killing the group must take the sleep down too.
        let script = format!("sleep 30 & echo $! > {} ; wait", pidfile.display());
        let request = ProcessRequest::new("sh", vec!["-c".into(), script], dir.clone());

        let token = CancellationToken::new();
        let run_token = token.clone();
        let handle =
            tokio::spawn(async move { CommandRunner::new().run(request, run_token).await });

        // Wait for the grandchild pid to appear.
        let mut gc_pid = None;
        for _ in 0..200 {
            if let Ok(s) = std::fs::read_to_string(&pidfile)
                && let Ok(pid) = s.trim().parse::<i32>()
            {
                gc_pid = Some(pid);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let gc_pid = gc_pid.expect("grandchild pid file");

        token.cancel();
        let result = handle.await.unwrap();
        assert!(matches!(result, Err(ProcessError::Cancelled)));

        // Give signals a moment to propagate, then confirm the grandchild is gone.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let alive = nix::sys::signal::kill(nix::unistd::Pid::from_raw(gc_pid), None).is_ok();
        assert!(!alive, "grandchild sleep {gc_pid} should have been killed");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// WS1: Windows Job Object must kill cmd-spawned grandchildren on cancel.
    #[cfg(windows)]
    #[tokio::test]
    async fn windows_job_cancellation_kills_grandchildren() {
        let (dir, request, pidfile) = windows_grandchild_request("cancel");
        let token = CancellationToken::new();
        let run_token = token.clone();
        let runner = windows_host_runner();
        let handle = tokio::spawn(async move { runner.run(request, run_token).await });

        let gc_pid = wait_windows_pidfile(&pidfile).await;
        token.cancel();
        let result = handle.await.unwrap();
        assert!(
            matches!(result, Err(ProcessError::Cancelled)),
            "expected Cancelled, got {result:?}"
        );
        assert_windows_grandchild_dead(gc_pid).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// WS1: timeout path must also terminate the Job tree (not only cancel).
    #[cfg(windows)]
    #[tokio::test]
    async fn windows_job_timeout_kills_grandchildren() {
        let (dir, mut request, pidfile) = windows_grandchild_request("timeout");
        // Leave enough time for PowerShell cold-start on hosted runners and to
        // prove the grandchild exists before exercising the timeout kill path.
        request.timeout = Duration::from_secs(8);
        let runner = windows_host_runner();
        let handle =
            tokio::spawn(async move { runner.run(request, CancellationToken::new()).await });

        let gc_pid = wait_windows_pidfile(&pidfile).await;
        let result = handle.await.unwrap().expect("timeout returns Ok timed_out");
        assert!(
            result.timed_out,
            "expected timed_out=true, got exit={:?}",
            result.exit_code
        );
        assert_windows_grandchild_dead(gc_pid).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Real `CommandRunner::run` path on Windows maps Job/wrap spawn failures
    /// to typed errors (never a successful plain-child run).
    #[cfg(windows)]
    #[tokio::test]
    async fn windows_command_runner_maps_job_spawn_failure() {
        // Missing program: CreateProcess fails inside process-wrap Job path.
        let req = ProcessRequest::new(
            "definitely-no-such-leveler-ws1-program-xyz",
            vec![],
            std::env::temp_dir(),
        );
        let err = CommandRunner::new()
            .run(req, CancellationToken::new())
            .await
            .expect_err("missing program must fail");
        // NotFound → Spawn; other wrap failures → ProcessTreeSetup. Either way
        // the real runner returned a typed error (not Ok / silent plain child).
        match &err {
            ProcessError::Spawn { program, source } => {
                assert!(program.contains("definitely-no-such-leveler-ws1-program-xyz"));
                assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
            }
            ProcessError::ProcessTreeSetup(msg) => {
                assert!(
                    msg.contains("Job Object") || msg.contains("process-tree"),
                    "{msg}"
                );
            }
            other => panic!("expected Spawn or ProcessTreeSetup, got {other:?}"),
        }
    }

    #[cfg(windows)]
    fn windows_host_runner() -> CommandRunner {
        CommandRunner::with_environment(std::sync::Arc::new(leveler_core::EnvSnapshot::new(
            std::env::vars_os(),
            std::env::current_dir().unwrap_or_default(),
            std::env::temp_dir(),
        )))
    }

    #[cfg(windows)]
    fn windows_grandchild_request(
        tag: &str,
    ) -> (std::path::PathBuf, ProcessRequest, std::path::PathBuf) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "leveler-win-tree-{tag}-{}",
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let pidfile = dir.join("gc.pid");
        let system_root = std::env::var_os("SystemRoot")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows"));
        let powershell = system_root.join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
        let ping = system_root.join(r"System32\PING.EXE");
        let pidfile_str = pidfile.display().to_string().replace('\'', "''");
        let ping_str = ping.display().to_string().replace('\'', "''");
        let script = format!(
            "$p = Start-Process -PassThru -WindowStyle Hidden -FilePath '{ping_str}' -ArgumentList @('-n','60','127.0.0.1'); Set-Content -Encoding Ascii -Path '{pidfile_str}' -Value $p.Id; Wait-Process -Id $p.Id"
        );
        let request = ProcessRequest::new(
            powershell.display().to_string(),
            vec![
                "-NoLogo".into(),
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-Command".into(),
                script,
            ],
            dir.clone(),
        );
        (dir, request, pidfile)
    }

    #[cfg(windows)]
    async fn wait_windows_pidfile(pidfile: &std::path::Path) -> u32 {
        for _ in 0..400 {
            if let Ok(s) = std::fs::read_to_string(pidfile)
                && let Ok(pid) = s.trim().parse::<u32>()
                && pid > 0
            {
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("grandchild pid file missing: {}", pidfile.display());
    }

    #[cfg(windows)]
    async fn assert_windows_grandchild_dead(gc_pid: u32) {
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            !windows_pid_alive(gc_pid),
            "grandchild pid {gc_pid} should have been killed by Job Object"
        );
    }

    #[cfg(windows)]
    fn windows_pid_alive(pid: u32) -> bool {
        let out = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output();
        match out {
            Ok(o) => {
                let text = String::from_utf8_lossy(&o.stdout);
                text.contains(&pid.to_string()) && !text.to_uppercase().contains("NO TASKS")
            }
            Err(_) => false,
        }
    }

    #[test]
    fn process_tree_capability_is_job() {
        assert!(crate::process_tree_backend_available());
        assert_eq!(
            crate::probe_sandbox_capabilities().process_tree,
            crate::ProcessTreeCapability::Job
        );
    }

    #[test]
    fn map_windows_job_spawn_error_types_not_found_as_spawn() {
        let err = map_windows_job_spawn_error(
            "missing.exe",
            std::io::Error::new(std::io::ErrorKind::NotFound, "not found"),
        );
        assert!(matches!(err, ProcessError::Spawn { .. }), "{err:?}");
    }

    #[test]
    fn map_windows_job_spawn_error_types_other_as_process_tree_setup() {
        let err = map_windows_job_spawn_error(
            "tool.exe",
            std::io::Error::new(std::io::ErrorKind::PermissionDenied, "job assign denied"),
        );
        match err {
            ProcessError::ProcessTreeSetup(msg) => {
                assert!(msg.contains("Job Object"), "{msg}");
                assert!(msg.contains("tool.exe"), "{msg}");
            }
            other => panic!("expected ProcessTreeSetup, got {other:?}"),
        }
    }

    /// Prove the same mapping the Windows `CommandRunner::run` path uses is
    /// what constructs `ProcessTreeSetup` for non-NotFound wrap failures.
    #[test]
    fn process_tree_setup_comes_from_job_spawn_mapping() {
        let err = map_windows_job_spawn_error(
            "cmd",
            std::io::Error::other("AssignProcessToJobObject failed"),
        );
        assert!(
            matches!(err, ProcessError::ProcessTreeSetup(_)),
            "CommandRunner Windows path uses this mapping: {err:?}"
        );
        let msg = err.to_string();
        assert!(msg.contains("process-tree") || msg.contains("Job"), "{msg}");
    }

    #[test]
    fn process_tree_setup_error_display_is_stable() {
        let err = ProcessError::ProcessTreeSetup("job create failed".into());
        let msg = err.to_string();
        assert!(msg.contains("process-tree") || msg.contains("Job"), "{msg}");
        assert!(matches!(err, ProcessError::ProcessTreeSetup(_)));
    }
}
