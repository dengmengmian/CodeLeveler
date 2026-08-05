//! Immutable host capabilities captured by the application composition root.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Host environment as it stood when the composition root captured it.
///
/// Library crates read this instead of `std::env` so that a value cannot
/// change under them mid-run and so tests can supply an exact environment.
/// A snapshot never re-reads the process environment: a variable exported
/// after capture is invisible here, which is what keeps spawned children from
/// inheriting credentials that appeared late.
#[derive(Debug, Clone, Default)]
pub struct EnvSnapshot {
    values: BTreeMap<OsString, OsString>,
    current_dir: PathBuf,
    temp_dir: PathBuf,
}

impl EnvSnapshot {
    /// Capture `values` as the complete environment, with the working and
    /// temporary directories stated explicitly rather than probed.
    pub fn new(
        values: impl IntoIterator<Item = (OsString, OsString)>,
        current_dir: PathBuf,
        temp_dir: PathBuf,
    ) -> Self {
        Self {
            values: values.into_iter().collect(),
            current_dir,
            temp_dir,
        }
    }

    /// Look up a variable by exact name. Case-sensitive on every platform;
    /// see [`Self::var_os_case_insensitive`] for Windows-style lookup.
    pub fn var_os(&self, key: impl AsRef<OsStr>) -> Option<OsString> {
        self.values.get(key.as_ref()).cloned()
    }

    /// [`Self::var_os`] restricted to values that are valid UTF-8. A variable
    /// holding non-UTF-8 bytes reads as absent rather than lossily converted.
    pub fn var(&self, key: impl AsRef<OsStr>) -> Option<String> {
        self.var_os(key).and_then(|v| v.into_string().ok())
    }

    /// Every captured variable, ordered by name.
    pub fn vars_os(&self) -> impl Iterator<Item = (&OsString, &OsString)> {
        self.values.iter()
    }

    /// The working directory at capture time — not the process's current one.
    pub fn current_dir(&self) -> &Path {
        &self.current_dir
    }

    /// The temporary directory at capture time. Sandbox setup treats this as
    /// untrusted: it may itself sit inside the workspace.
    pub fn temp_dir(&self) -> &Path {
        &self.temp_dir
    }

    /// Split a `PATH`-style variable on the platform separator. Absent or
    /// empty yields an empty vector.
    pub fn paths(&self, key: impl AsRef<OsStr>) -> Vec<PathBuf> {
        self.var_os(key)
            .map(|v| std::env::split_paths(&v).collect())
            .unwrap_or_default()
    }

    /// Read a variable using Windows' case-insensitive environment semantics.
    /// Useful for immutable snapshots captured on Windows, where the host may
    /// expose `Path` rather than `PATH`.
    pub fn var_os_case_insensitive(&self, key: &str) -> Option<OsString> {
        self.values.iter().find_map(|(name, value)| {
            name.to_str()
                .is_some_and(|name| name.eq_ignore_ascii_case(key))
                .then(|| value.clone())
        })
    }

    /// [`Self::paths`] using the case-insensitive lookup of
    /// [`Self::var_os_case_insensitive`].
    pub fn paths_case_insensitive(&self, key: &str) -> Vec<PathBuf> {
        self.var_os_case_insensitive(key)
            .map(|value| std::env::split_paths(&value).collect())
            .unwrap_or_default()
    }
}

/// Provider credential variables scrubbed from every spawned process even when
/// the name alone would not look credential-shaped.
pub const SECRET_ENV_DENYLIST: &[&str] = &[
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

impl EnvSnapshot {
    /// [`Self::vars_os`] with credential-named variables
    /// ([`is_credential_env_name`]) removed. Feed this to a child process
    /// after `env_clear()` so it never inherits provider secrets.
    pub fn scrubbed_vars_os(&self) -> impl Iterator<Item = (&OsString, &OsString)> {
        self.vars_os()
            .filter(|(name, _)| !name.to_str().is_some_and(is_credential_env_name))
    }
}

/// The installed [`environment`] snapshot with credential variables removed —
/// the one way to build a child-process environment.
///
/// Use as `cmd.env_clear(); cmd.envs(scrubbed_environment());` (works for both
/// `std` and `tokio` commands). Clearing first and rebuilding from the
/// immutable snapshot matters: removing only known names from the live parent
/// environment is racy — a credential exported after startup would still be
/// inherited by children (git hooks, clean/smudge filters, LSP/MCP servers,
/// user tool hooks) and could be exfiltrated by any code they run.
pub fn scrubbed_environment() -> impl Iterator<Item = (OsString, OsString)> {
    environment()
        .scrubbed_vars_os()
        .map(|(name, value)| (name.clone(), value.clone()))
}

/// Run `git <args>` in `repo` with a credential-scrubbed environment and
/// return its stdout. `None` when git cannot be spawned or exits non-zero;
/// stderr is discarded — callers that need it should run git themselves
/// (still via [`scrubbed_environment`]).
pub fn git_stdout(repo: &Path, args: &[&str]) -> Option<String> {
    let mut command = std::process::Command::new("git");
    command
        .args(args)
        .current_dir(repo)
        .stdin(std::process::Stdio::null())
        .env_clear()
        .envs(scrubbed_environment());
    let out = command.output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

static ENVIRONMENT: OnceLock<EnvSnapshot> = OnceLock::new();

/// Install the process-wide snapshot. The composition root calls this once,
/// before any library code reads [`environment`].
///
/// # Errors
///
/// Returns the rejected `snapshot` unchanged if one was already installed —
/// the first install wins, so a second caller cannot swap the environment out
/// from under code already running against it.
pub fn install_environment(snapshot: EnvSnapshot) -> Result<(), EnvSnapshot> {
    ENVIRONMENT.set(snapshot)
}

/// The installed process capabilities. Library-only callers get an empty,
/// deterministic snapshot and must install/pass capabilities to use host state.
pub fn environment() -> &'static EnvSnapshot {
    static EMPTY: OnceLock<EnvSnapshot> = OnceLock::new();
    ENVIRONMENT
        .get()
        .unwrap_or_else(|| EMPTY.get_or_init(EnvSnapshot::default))
}

/// The global CodeLeveler home directory — `$LEVELER_HOME`, else `$HOME/.leveler`,
/// else `%USERPROFILE%\.leveler` (Windows). `None` when no home is known.
///
/// The single source of this resolution order. Callers pass their own env
/// lookup so each keeps its source (live `std::env` vs the installed snapshot);
/// only the order — including the `USERPROFILE` fallback — lives here, so the
/// surfaces cannot drift apart (which previously dropped `USERPROFILE` on
/// Windows in some places but not others).
pub fn leveler_home_dir_from<F>(var_os: F) -> Option<PathBuf>
where
    F: Fn(&str) -> Option<OsString>,
{
    if let Some(h) = var_os("LEVELER_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(h));
    }
    var_os("HOME")
        .filter(|v| !v.is_empty())
        .or_else(|| var_os("USERPROFILE").filter(|v| !v.is_empty()))
        .map(|h| PathBuf::from(h).join(".leveler"))
}

/// [`leveler_home_dir_from`] resolved against an [`EnvSnapshot`].
pub fn leveler_home_dir(env: &EnvSnapshot) -> Option<PathBuf> {
    leveler_home_dir_from(|k| env.var_os(k))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_can_read_windows_environment_names_case_insensitively() {
        let snapshot = EnvSnapshot::new(
            [(OsString::from("Path"), OsString::from("tool-bin"))],
            PathBuf::new(),
            PathBuf::new(),
        );

        assert_eq!(
            snapshot.var_os_case_insensitive("PATH"),
            Some(OsString::from("tool-bin"))
        );
    }

    fn lookup<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| OsString::from(*v))
        }
    }

    #[test]
    fn credential_names_are_recognized_and_scrubbed_from_snapshots() {
        assert!(is_credential_env_name("OPENAI_API_KEY"));
        assert!(is_credential_env_name("my_service_token"));
        assert!(is_credential_env_name("AWS_SECRET_ACCESS_KEY"));
        assert!(!is_credential_env_name("PATH"));

        let snapshot = EnvSnapshot::new(
            [
                (OsString::from("HOME"), OsString::from("/h")),
                (OsString::from("FOO_API_KEY"), OsString::from("s3cret")),
            ],
            PathBuf::new(),
            PathBuf::new(),
        );
        let kept: Vec<_> = snapshot.scrubbed_vars_os().map(|(n, _)| n.clone()).collect();
        assert_eq!(kept, vec![OsString::from("HOME")]);
    }

    /// `git_stdout` against a real repository: stdout on success, `None` on a
    /// non-zero exit (bad ref) and on a non-repository directory.
    #[test]
    fn git_stdout_returns_stdout_on_success_and_none_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(repo)
                .output()
                .expect("run git");
            assert!(out.status.success(), "git {args:?}: {out:?}");
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(repo.join("f.txt"), "x").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "base"]);

        let head = git_stdout(repo, &["rev-parse", "HEAD"]).expect("HEAD exists");
        assert_eq!(head.trim().len(), 40, "full sha, got {head:?}");

        // Non-zero exit → None.
        assert_eq!(git_stdout(repo, &["rev-parse", "no-such-ref-xyz"]), None);

        // Not a repository → None.
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(git_stdout(empty.path(), &["rev-parse", "HEAD"]), None);
    }

    #[test]
    fn resolution_order_prefers_leveler_home_then_home_then_userprofile() {
        assert_eq!(
            leveler_home_dir_from(lookup(&[("LEVELER_HOME", "/lh"), ("HOME", "/h")])),
            Some(PathBuf::from("/lh"))
        );
        assert_eq!(
            leveler_home_dir_from(lookup(&[("HOME", "/h"), ("USERPROFILE", "/u")])),
            Some(PathBuf::from("/h/.leveler"))
        );
        // USERPROFILE is the Windows fallback that used to be dropped.
        assert_eq!(
            leveler_home_dir_from(lookup(&[("USERPROFILE", "/u")])),
            Some(PathBuf::from("/u/.leveler"))
        );
        assert_eq!(leveler_home_dir_from(lookup(&[])), None);
        // Empty values are skipped, not treated as a set home.
        assert_eq!(
            leveler_home_dir_from(lookup(&[("LEVELER_HOME", ""), ("HOME", "/h")])),
            Some(PathBuf::from("/h/.leveler"))
        );
    }
}
