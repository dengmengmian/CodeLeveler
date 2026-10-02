//! Git permission behaviour, end to end: the verdict comes from a Git
//! invocation's EFFECTS, and the repository-metadata capability that verdict
//! implies is what the OS sandbox actually grants.
//!
//! These are not argv-shape unit tests. Each capability claim is checked
//! against a real repository in a real sandbox (`sandbox-exec` / `bwrap`):
//!
//! - a sealed workspace really does fail `git fetch` on `.git/FETCH_HEAD`,
//!   which is the reported failure this whole change exists for;
//! - the capability the effect grants really does let the same fetch write
//!   that file;
//! - a credential helper that the sandbox denies really does print `fatal:`
//!   while the Git process still reports success, so a caller must read the
//!   EXIT CODE and never the stderr text.

use std::path::{Path, PathBuf};
use std::time::Duration;

use leveler_execution::{
    CommandClass, CommandRunner, CommandView, ProcessRequest, WriteScope, classify_command,
    executed_commands, git_command_effects, shell_invocation,
};
use leveler_test_support::git;

/// An environment prefix does not remove the command, so it must not remove the
/// verdict. `GIT_DIR=/somewhere git push` still pushes.
#[test]
fn an_environment_prefix_does_not_hide_a_gated_command() {
    let classify = |script: &str| {
        let (program, args) = shell_invocation(script);
        classify_command(&CommandView {
            program: &program,
            args: &args,
        })
    };
    for script in [
        "GIT_DIR=/tmp/other git push origin main",
        "GIT_SSH_COMMAND=/tmp/evil git fetch origin",
        "GIT_CONFIG_GLOBAL=/tmp/evil git reset --hard",
    ] {
        assert_eq!(classify(script), CommandClass::Dangerous, "{script}");
    }

    // The prefix also blocks the capability, so the same invocation cannot be
    // handed the repository's metadata scope from its effects alone.
    let (program, args) = shell_invocation("GIT_DIR=/tmp/other git fetch origin");
    let executed = executed_commands(&program, &args);
    let effects = executed.git_effects();
    assert!(effects.any_git, "the fetch is still visible: {effects:?}");
    assert!(
        !effects.grants_metadata_write_by_effect(),
        "an environment prefix must not earn the metadata capability: {effects:?}"
    );
}

/// A Git verdict may only ever ADD a reason to ask. `Safe` is not permission to
/// skip the rest of the call: the danger and the Git command routinely share one
/// script, and each has its own reason to be gated.
#[test]
fn a_git_verdict_never_masks_the_rest_of_the_call() {
    let classify = |program: &str, words: &[&str]| {
        let args: Vec<String> = words.iter().map(|w| w.to_string()).collect();
        classify_command(&CommandView {
            program,
            args: &args,
        })
    };

    // A harmless Git read shares the script with a command that is not
    // harmless: the strictest verdict is the one that asks.
    for script in [
        "git status; sudo true",
        "git status; open /tmp/file",
        "git status; rm -rf /tmp/other",
        "git fetch origin && sudo true",
        "git status; git clean $FLAGS",
        "git status; git fetch $REMOTE",
        "git status > /etc/git-status",
    ] {
        assert_eq!(
            classify("sh", &["-c", script]),
            CommandClass::Dangerous,
            "{script}"
        );
    }

    // The converse: a gated Git command is not laundered by harmless siblings.
    assert_eq!(
        classify("sh", &["-c", "ls && git push origin main"]),
        CommandClass::Dangerous
    );

    // And an all-harmless script stays harmless.
    for script in [
        "git status; echo done",
        "git status && printf done",
        "git status; curl https://example.com",
    ] {
        assert_eq!(
            classify("sh", &["-c", script]),
            CommandClass::Safe,
            "{script}"
        );
    }
}

/// A local bare repository to fetch from, plus a working clone. A local
/// remote keeps the test hermetic: no network, no credentials, and the
/// metadata write is the only thing being measured.
struct Fixture {
    _root: tempfile::TempDir,
    origin: PathBuf,
    work: PathBuf,
}

fn fixture() -> Option<Fixture> {
    if !git::try_run(Path::new("."), &["--version"]) {
        eprintln!("skipping: git is unavailable");
        return None;
    }
    if !sandbox_available() {
        eprintln!("skipping: no OS write-confinement wrapper on this host");
        return None;
    }
    let root = tempfile::tempdir().unwrap();
    let origin = root.path().join("origin.git");
    let work = root.path().join("work");
    std::fs::create_dir_all(&origin).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    git::run(&origin, &["init", "-q", "--bare", "-b", "main"]);
    git::run(&work, &["init", "-q", "-b", "main"]);
    git::run(&work, &["config", "user.email", "t@t"]);
    git::run(&work, &["config", "user.name", "t"]);
    std::fs::write(work.join("a.txt"), "one\n").unwrap();
    git::run(&work, &["add", "a.txt"]);
    git::run(&work, &["commit", "-qm", "init"]);
    git::run(
        &work,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    // Local configuration beats the developer's global one, so the pull under
    // test is the shape the policy describes and not `--rebase`.
    git::run(&work, &["config", "pull.rebase", "false"]);
    git::run(&work, &["config", "pull.ff", "only"]);
    // Publish that commit so the clone below shares its history, then let a
    // second clone advance the remote by one commit: the working repository is
    // now genuinely behind, can fast-forward, and has a fetch to record.
    git::run(&work, &["push", "-q", "origin", "main"]);
    git::run(&work, &["fetch", "-q", "origin"]);
    let seed = root.path().join("seed");
    std::fs::create_dir_all(&seed).unwrap();
    git::run(&seed, &["clone", "-q", origin.to_str().unwrap(), "."]);
    git::run(&seed, &["config", "user.email", "t@t"]);
    git::run(&seed, &["config", "user.name", "t"]);
    std::fs::write(seed.join("b.txt"), "two\n").unwrap();
    git::run(&seed, &["add", "b.txt"]);
    git::run(&seed, &["commit", "-qm", "second"]);
    git::run(&seed, &["push", "-q", "origin", "main"]);
    Some(Fixture {
        _root: root,
        origin,
        work,
    })
}

/// The wrapper [`CommandRunner`] actually invokes for a confined request.
fn sandbox_available() -> bool {
    #[cfg(target_os = "macos")]
    {
        Path::new("/usr/bin/sandbox-exec").exists()
    }
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("bwrap")
            .arg("--version")
            .output()
            .is_ok_and(|out| out.status.success())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        false
    }
}

/// A runner over the REAL process environment: the confined child needs an
/// absolute home and temp dir for its private scratch and tool caches.
fn runner() -> CommandRunner {
    CommandRunner::with_environment(std::sync::Arc::new(leveler_core::EnvSnapshot::new(
        std::env::vars_os(),
        std::env::current_dir().unwrap_or_default(),
        std::env::temp_dir(),
    )))
}

async fn run_git(
    work: &Path,
    args: &[&str],
    scope: WriteScope,
) -> leveler_execution::ProcessOutput {
    let mut request = ProcessRequest::new(
        "git",
        args.iter().map(|a| a.to_string()).collect(),
        work.into(),
    );
    request.write_scope = scope;
    request.timeout = Duration::from_secs(60);
    request.allow_env.push("GIT_CONFIG_GLOBAL".to_string());
    runner().run(request, Default::default()).await.unwrap()
}

async fn fetch_with_scope(work: &Path, scope: WriteScope) -> leveler_execution::ProcessOutput {
    run_git(work, &["fetch", "origin"], scope).await
}

/// BEFORE: the reason `git pull`/`git fetch` failed on `.git/FETCH_HEAD`.
///
/// The workspace write fence seals `.git` wholesale, so a Git command's
/// metadata write — the mechanical precondition of a fetch — is denied even
/// though nothing outside the repository was attempted.
#[tokio::test]
async fn a_sealed_workspace_denies_the_fetch_metadata_write() {
    let Some(fx) = fixture() else { return };
    // The fixture's own setup fetch wrote FETCH_HEAD; removing the derived file
    // is what makes "the denied fetch did not write it" observable.
    let _ = std::fs::remove_file(fx.work.join(".git/FETCH_HEAD"));
    let out = fetch_with_scope(
        &fx.work,
        WriteScope::Workspace {
            root: fx.work.clone(),
        },
    )
    .await;
    assert!(
        !out.success(),
        "a sealed .git must deny the fetch write: {out:?}"
    );
    assert!(
        !fx.work.join(".git/FETCH_HEAD").exists(),
        "no FETCH_HEAD may be written under the seal"
    );
    assert!(
        out.stderr.contains("Operation not permitted")
            || out.stderr.contains("Permission denied")
            || out.stderr.contains("FETCH_HEAD"),
        "the failure must name the denied metadata write: {out:?}"
    );
    assert!(
        fx.origin.exists(),
        "the remote must be untouched: nothing reached it"
    );
}

/// AFTER: the capability the fetches's EFFECTS grant — repository metadata
/// write, with the workspace boundary and the credential store still closed —
/// is what makes the same command work.
#[tokio::test]
async fn the_granted_metadata_capability_completes_the_fetch() {
    let Some(fx) = fixture() else { return };
    let out = fetch_with_scope(
        &fx.work,
        WriteScope::WorkspaceWithGit {
            root: fx.work.clone(),
        },
    )
    .await;
    assert!(out.success(), "the granted fetch must succeed: {out:?}");
    assert!(
        fx.work.join(".git/FETCH_HEAD").is_file(),
        "the fetch records its result in FETCH_HEAD: {out:?}"
    );
}

/// The verdict and the capability are two readings of ONE parse, so the call
/// that is allowed to run without asking and the call that is allowed to write
/// repository metadata can never disagree.
#[test]
fn fetch_is_granted_by_effect_while_mutations_are_gated() {
    let caps = |program: &str, args: &[&str]| {
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        let executed = executed_commands(program, &args);
        let effects = executed.git_effects();
        (
            effects.grants_metadata_write_by_effect(),
            effects.gated(),
            effects.capabilities(),
        )
    };

    // Read-only: no metadata write is needed at all, so no capability is
    // granted and none is asked for.
    for args in [
        vec!["status"],
        vec!["diff", "HEAD"],
        vec!["log", "--oneline"],
        vec!["rev-parse", "HEAD"],
        vec!["ls-files"],
        vec!["branch", "--show-current"],
    ] {
        let (granted, gated, caps) = caps("git", &args);
        assert!(!granted, "git {args:?} writes no metadata");
        assert!(!gated, "git {args:?} must not prompt");
        assert_eq!(caps, vec!["git_metadata.read"], "git {args:?}");
    }

    // The reported case: a sync against the repository's OWN remote writes
    // only fetched metadata.
    for args in [vec!["fetch", "origin"], vec!["ls-remote", "origin"]] {
        let (_, gated, _) = caps("git", &args);
        assert!(!gated, "git {args:?} must not prompt");
    }
    assert!(caps("git", &["fetch", "origin"]).0, "fetch writes metadata");
    assert!(
        !caps("git", &["ls-remote", "origin"]).0,
        "ls-remote writes nothing locally"
    );

    // A remote the repository does not configure is a target the caller
    // picked, so it is asked for.
    for args in [
        vec!["fetch", "https://unknown-host.example/x.git"],
        vec!["fetch", "git@unknown-host.example:x.git"],
        vec!["fetch", "/tmp/somewhere-else"],
        vec!["ls-remote", "https://unknown-host.example/x.git"],
    ] {
        assert!(caps("git", &args).1, "git {args:?} must be asked for");
    }

    // Local history recording writes metadata and changes neither the
    // checkout's state nor a remote, so it rides the same decision as a fetch.
    for args in [vec!["add", "-A"], vec!["commit", "-m", "x"]] {
        let (granted, gated, caps) = caps("git", &args);
        assert!(granted, "git {args:?} writes metadata: {caps:?}");
        assert!(!gated, "git {args:?} must not prompt: {caps:?}");
    }

    // Workspace mutation is a different permission from a sync.
    for args in [
        vec!["pull"],
        vec!["merge", "origin/main"],
        vec!["rebase", "main"],
        vec!["checkout", "main"],
        vec!["switch", "-c", "feat"],
        vec!["stash"],
        vec!["stash", "pop"],
        vec!["clone", "https://example.com/x.git"],
    ] {
        let (_, gated, _) = caps("git", &args);
        assert!(gated, "git {args:?} must be asked for");
    }

    // Remote writes, identity rewrites, and irreversible history loss are
    // never Git's decision to make silently.
    for args in [
        vec!["push"],
        vec!["push", "--force", "origin", "main"],
        vec!["reset", "--hard"],
        vec!["clean", "-fd"],
        vec!["clean", "-fdx"],
        vec!["remote", "add", "evil", "https://evil.example/x"],
        vec!["remote", "set-url", "origin", "https://evil.example/x"],
        vec!["config", "remote.origin.url", "https://evil.example/x"],
        vec!["rm", "tracked.txt"],
        vec!["branch", "-D", "main"],
        vec!["update-ref", "-d", "refs/heads/main"],
        vec!["stash", "drop"],
        vec!["send-email", "HEAD~1"],
        vec!["filter-branch", "--all"],
    ] {
        assert!(caps("git", &args).1, "git {args:?} must be asked for");
    }

    // An invocation we cannot read is asked for, never guessed at.
    assert!(caps("git", &["some-future-subcommand"]).1);
    assert!(
        caps(
            "git",
            &["-c", "core.hooksPath=/tmp/evil", "fetch", "origin"]
        )
        .1
    );
    assert!(caps("git", &["--git-dir=/tmp/elsewhere", "fetch", "origin"]).1);

    // Arguments that name a program Git executes turn a transport command into
    // arbitrary code, so they are asked for however harmless the subcommand is.
    for args in [
        vec!["fetch", "--upload-pack=/tmp/helper", "origin"],
        vec!["fetch", "--upload-pack", "/tmp/helper", "origin"],
        vec!["ls-remote", "--upload-pack=/tmp/helper", "origin"],
        vec!["push", "--receive-pack=/tmp/helper", "origin", "main"],
        vec![
            "clone",
            "--upload-pack=/tmp/helper",
            "https://example.com/x.git",
        ],
    ] {
        assert!(caps("git", &args).1, "git {args:?} must be asked for");
    }
}

/// Case B. `git pull` is a fetch PLUS a mutation of the checked-out tree, so
/// it is a different permission from `git fetch` — and once approved it needs
/// the same repository-metadata capability, because the refs and HEAD it
/// advances have to be writable for the mutation to land.
#[tokio::test]
async fn a_pull_needs_both_the_metadata_capability_and_the_workspace_mutation() {
    let Some(fx) = fixture() else { return };
    let sealed = WriteScope::Workspace {
        root: fx.work.clone(),
    };
    let out = run_git(&fx.work, &["pull", "origin", "main"], sealed).await;
    assert!(!out.success(), "a sealed .git must deny the pull: {out:?}");
    assert!(
        !fx.work.join("b.txt").exists(),
        "the pull must not have advanced the working tree: {out:?}"
    );

    let unsealed = WriteScope::WorkspaceWithGit {
        root: fx.work.clone(),
    };
    let out = run_git(&fx.work, &["pull", "origin", "main"], unsealed).await;
    assert!(out.success(), "the granted pull must succeed: {out:?}");
    assert!(
        fx.work.join("b.txt").is_file(),
        "the pull advances the working tree: {out:?}"
    );
}

/// A script that mixes Git with anything else does not inherit Git's narrow
/// metadata allowance, however harmless the other command looks.
#[test]
fn a_mixed_script_never_inherits_the_git_capability() {
    let (program, args) = shell_invocation("git fetch origin && ls");
    let executed = executed_commands(&program, &args);
    let effects = executed.git_effects();
    assert!(effects.any_git, "the fetch is visible: {effects:?}");
    assert!(!effects.fully_git, "the script also runs a non-Git command");
    assert!(
        !effects.grants_metadata_write_by_effect(),
        "a mixed script must not widen .git: {effects:?}"
    );

    // A shell wrapper around ONLY Git commands keeps the capability, so the
    // ordinary `shell_command` path is not needlessly blocked.
    let (program, args) = shell_invocation("git fetch origin && git status");
    let executed = executed_commands(&program, &args);
    let effects = executed.git_effects();
    assert!(effects.fully_git, "{effects:?}");
    assert!(
        effects.grants_metadata_write_by_effect(),
        "an all-Git script keeps the fetch capability: {effects:?}"
    );

    // A destructive inner command still gates the whole script.
    let (program, args) = shell_invocation("git fetch origin && git reset --hard");
    let executed = executed_commands(&program, &args);
    let effects = executed.git_effects();
    assert!(effects.gated(), "{effects:?}");
}

/// Case E. Git's own credential helper can fail inside the sandbox — it is
/// trying to lock a store outside every writable root — and print `fatal:`
/// while the Git process still exits 0.
///
/// The verdict must therefore come from the EXIT CODE. Nothing may downgrade
/// a successful operation because its stderr mentioned a failure; nothing may
/// upgrade one because stderr was quiet.
#[tokio::test]
async fn a_denied_credential_store_is_not_a_failed_git_operation() {
    if !git::try_run(Path::new("."), &["--version"]) {
        eprintln!("skipping: git is unavailable");
        return;
    }
    if !sandbox_available() {
        eprintln!("skipping: no OS write-confinement wrapper on this host");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let work = root.path().join("work");
    std::fs::create_dir_all(&work).unwrap();

    // The store Git is asked to write is outside every writable root, which is
    // exactly the situation the sandbox creates for `~/.git-credentials`.
    let denied = root.path().join("denied");
    std::fs::create_dir_all(&denied).unwrap();
    let helper = format!("store --file={}/.git-credentials", denied.display());
    std::fs::write(
        work.join("credential"),
        "protocol=https\nhost=h\nusername=u\npassword=p\n\n",
    )
    .unwrap();

    // `git credential approve` is the call Git's transport makes after a
    // successful authentication, so this is the real shape of the report.
    let (program, args) = shell_invocation(&format!(
        "git -c credential.helper='{helper}' credential approve < credential"
    ));
    let mut request = ProcessRequest::new(program, args, work.clone());
    request.write_scope = WriteScope::Workspace { root: work.clone() };
    request.timeout = Duration::from_secs(60);
    let out = runner().run(request, Default::default()).await.unwrap();

    #[cfg(unix)]
    {
        assert!(
            out.stderr.contains("unable to get credential storage lock"),
            "the helper's denial must be visible in stderr, not hidden: {out:?}"
        );
        assert!(
            out.success(),
            "the process exit code is the authority, and Git reported success: {out:?}"
        );
        assert!(
            !denied.join(".git-credentials").exists(),
            "the credential store must not have been written"
        );
    }
    #[cfg(not(unix))]
    {
        let _ = out;
    }
}

/// Case C: the fetch decision is not a push decision. `git push` is a remote
/// side effect and is gated in every profile.
#[test]
fn push_never_rides_on_the_fetch_decision() {
    let fetch = git_command_effects(&["git".into(), "fetch".into(), "origin".into()]).unwrap();
    let push = git_command_effects(&["git".into(), "push".into(), "origin".into()]).unwrap();
    assert!(fetch.effects.remote_read && !fetch.effects.remote_write);
    assert!(push.effects.remote_write && !push.effects.remote_read);
    assert_ne!(
        push.effects.capabilities(),
        fetch.effects.capabilities(),
        "push and fetch must not share a capability set"
    );
}

/// P0-1: a remote Git transfer laundered through a shell wrapper cannot be bound
/// to an approved remote and credential, so it keeps exact-call approval under
/// Assisted instead of inheriting the direct `git fetch` auto-run. Full still
/// never asks.
#[test]
fn assisted_asks_for_a_wrapper_remote_git_transfer() {
    use leveler_execution::{ApprovalPolicy, PermissionProfile, Requirement, RiskLevel};
    let policy = ApprovalPolicy::default();
    for script in [
        "git fetch origin",
        "git ls-remote origin",
        "git status && git fetch origin",
    ] {
        let args = vec!["-c".to_string(), script.to_string()];
        let view = CommandView {
            program: "sh",
            args: &args,
        };
        assert_eq!(
            policy.evaluate(
                PermissionProfile::Assisted,
                "shell_command",
                RiskLevel::WorkspaceWrite,
                Some(view),
            ),
            Requirement::NeedApproval,
            "Assisted must ask: {script}"
        );
        assert_eq!(
            policy.evaluate(
                PermissionProfile::FullAccess,
                "shell_command",
                RiskLevel::WorkspaceWrite,
                Some(view),
            ),
            Requirement::Auto,
            "Full must never ask: {script}"
        );
    }
    // The direct invocation keeps the bindable, grant-eligible auto-run.
    let args = vec!["fetch".to_string(), "origin".to_string()];
    assert_eq!(
        policy.evaluate(
            PermissionProfile::Assisted,
            "run_command",
            RiskLevel::WorkspaceWrite,
            Some(CommandView {
                program: "git",
                args: &args,
            }),
        ),
        Requirement::Auto
    );
}
