//! Shell writes and Git formatting flags cannot borrow metadata authority.
use leveler_execution::{
    CommandClass, CommandRunner, CommandView, classify_command, executed_commands,
};
// Only the two Unix-gated tests below build a request with a write scope, so on
// Windows these would be unused imports under `-D warnings`.
#[cfg(any(target_os = "macos", target_os = "linux"))]
use leveler_execution::{ProcessRequest, WriteScope};
use leveler_test_support::git;

fn effects(script: &str) -> leveler_execution::CallGitEffects {
    let args = vec!["-c".into(), script.into()];
    let executed = executed_commands("sh", &args);
    executed.git_effects()
}

fn classify(script: &str) -> CommandClass {
    let args = vec!["-c".into(), script.into()];
    classify_command(&CommandView {
        program: "sh",
        args: &args,
    })
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn runner() -> CommandRunner {
    CommandRunner::with_environment(std::sync::Arc::new(leveler_core::EnvSnapshot::new(
        std::env::vars_os(),
        std::env::current_dir().unwrap(),
        std::env::temp_dir(),
    )))
}

#[test]
fn config_display_modifiers_do_not_turn_writes_into_reads() {
    let repo = git::scratch_repo();
    // Git accepts this formatting modifier on a SET action; it is not a query.
    git::run(
        repo.path(),
        &[
            "config",
            "--show-scope",
            "remote.origin.url",
            "https://chosen.example/repo",
        ],
    );
    assert!(
        std::fs::read_to_string(repo.path().join(".git/config"))
            .unwrap()
            .contains("https://chosen.example/repo")
    );
    let script =
        "git add . && git config --show-scope remote.origin.url https://chosen.example/repo";
    let effects = effects(script);
    assert!(
        effects.effects.config_write,
        "format flags do not erase the config write: {effects:?}"
    );
    assert!(
        effects.gated(),
        "repository identity change needs authorization"
    );
    assert_eq!(classify(script), CommandClass::Dangerous);
}

#[test]
fn shell_write_redirections_do_not_receive_git_metadata_scope() {
    for script in [
        "git fetch origin > .git/config",
        "git fetch origin && git status >> .git/config",
        "git fetch origin > out.txt",
        "git fetch origin 2> .git/config",
        "git fetch origin &> .git/config",
        "git fetch origin <> .git/config",
        "git fetch origin > $OUTPUT",
        "bash -c 'git fetch origin > .git/config'",
        "git fetch origin > .git/config; git status",
    ] {
        assert!(
            !effects(script).needs_repository_write_scope(),
            "shell file write cannot inherit Git's scope: {script}"
        );
    }
    // An input redirect and fd duplication do not add a filesystem write.
    for script in ["git fetch origin < input.txt", "git fetch origin 1>&2"] {
        assert!(effects(script).needs_repository_write_scope(), "{script}");
    }
}

#[test]
fn nested_shells_retain_environment_and_redirect_authority_constraints() {
    for script in [
        "GIT_DIR=other sh -c 'git add file'",
        "GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=core.hooksPath GIT_CONFIG_VALUE_0=other bash -c 'git add file'",
        "sh -c \"GIT_DIR=other bash -c 'git add file'\"",
    ] {
        let executed = executed_commands("sh", &["-c".into(), script.into()]);
        let git = executed.git_effects();
        assert!(git.any_git, "the nested Git invocation must remain visible");
        assert!(
            !executed.complete,
            "wrapper environment cannot be dropped: {script}"
        );
        assert!(
            git.gated() && !git.needs_repository_write_scope(),
            "{script}: {git:?}"
        );
    }
    let executed = executed_commands(
        "sh",
        &[
            "-c".into(),
            "sh -c \"bash -c 'git fetch origin > .git/config'\"".into(),
        ],
    );
    assert!(
        executed.shell_writes,
        "nested shell must retain its file writes"
    );
    assert!(!executed.git_effects().needs_repository_write_scope());
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test]
async fn git_shell_redirect_cannot_truncate_the_real_repository_config() {
    let repo = git::scratch_repo();
    let root = repo.path().canonicalize().unwrap();
    let config = root.join(".git/config");
    let before = std::fs::read(&config).unwrap();
    let script = "git fetch origin > .git/config";
    let scope = if effects(script).needs_repository_write_scope() {
        WriteScope::WorkspaceWithGit { root: root.clone() }
    } else {
        WriteScope::Workspace { root: root.clone() }
    };
    let mut request = ProcessRequest::new("sh", vec!["-c".into(), script.into()], root);
    request.write_scope = scope;
    let output = runner().run(request, Default::default()).await.unwrap();
    assert!(
        !output.success(),
        "the sandbox must reject the shell's config write: {output:?}"
    );
    assert_eq!(
        std::fs::read(config).unwrap(),
        before,
        "a failed fetch must not have truncated .git/config through a widened shell"
    );
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test]
async fn ordinary_git_status_output_redirect_still_runs_without_approval() {
    let repo = git::scratch_repo();
    let root = repo.path().canonicalize().unwrap();
    let script = "git status --porcelain > out.txt";
    assert_eq!(classify(script), CommandClass::Safe);
    assert!(!effects(script).gated());
    let mut request = ProcessRequest::new("sh", vec!["-c".into(), script.into()], root.clone());
    request.write_scope = WriteScope::Workspace { root: root.clone() };
    let output = runner().run(request, Default::default()).await.unwrap();
    assert!(
        output.success(),
        "ordinary workspace output remains permitted: {output:?}"
    );
    assert!(root.join("out.txt").is_file());
}
