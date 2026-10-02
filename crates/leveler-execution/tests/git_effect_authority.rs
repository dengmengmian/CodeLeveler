//! Git effects must not replace the authority decision for other executed commands.
use leveler_execution::{CommandClass, CommandView, classify_command, executed_commands};

fn classify(program: &str, words: &[&str]) -> CommandClass {
    let args = words
        .iter()
        .map(|word| word.to_string())
        .collect::<Vec<_>>();
    classify_command(&CommandView {
        program,
        args: &args,
    })
}

#[test]
fn git_reads_do_not_authorize_other_shell_effects() {
    for script in [
        "git status; sudo true",
        "git status; open /tmp/file",
        "git status; open /tmp/file; rm -rf /tmp/other",
        "git status | sh -c 'rm -rf /tmp/file'",
        "git status; bash -c 'sudo true'",
        "git status > /etc/git-status",
    ] {
        assert_eq!(
            classify("sh", &["-c", script]),
            CommandClass::Dangerous,
            "{script}"
        );
    }
}

#[test]
fn mixed_safe_commands_keep_their_actual_effect_verdict() {
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
        let args = vec!["-c".into(), script.into()];
        let executed = executed_commands("sh", &args);
        let effects = executed.git_effects();
        assert!(
            !effects.needs_repository_write_scope(),
            "mixed command cannot gain Git metadata scope: {script}"
        );
    }
}

#[test]
fn partially_readable_git_call_does_not_authorize_opaque_effects() {
    for script in [
        "git status; $COMMAND",
        "git status; bash -c \"$SCRIPT\"",
        "git status; git clean $FLAGS",
        "git status; git fetch $REMOTE",
    ] {
        assert_eq!(
            classify("sh", &["-c", script]),
            CommandClass::Dangerous,
            "{script}"
        );
        let args = vec!["-c".into(), script.into()];
        let executed = executed_commands("sh", &args);
        assert!(!executed.complete, "{script}");
        assert!(executed.git_effects().gated(), "{script}");
    }
}

/// A remote Git transfer that a shell wrapper launders cannot be bound to a
/// frozen approved remote and credential: only a directly invoked `git` can.
/// It must keep exact-call approval instead of auto-running against the user's
/// live Git credential store.
#[test]
fn wrapper_remote_git_keeps_exact_call_approval() {
    for script in [
        "git fetch origin",
        "git ls-remote origin",
        "git fetch origin && git status",
        "git remote show origin",
        "git archive --remote=origin HEAD",
    ] {
        assert_eq!(
            classify("sh", &["-c", script]),
            CommandClass::Dangerous,
            "{script}"
        );
    }
    // A direct `git` invocation keeps the bindable, grant-eligible path.
    assert_eq!(classify("git", &["fetch", "origin"]), CommandClass::Safe);
    assert_eq!(
        classify("git", &["ls-remote", "origin"]),
        CommandClass::Safe
    );
    // A local-only wrapper is unaffected: no remote transfer, no credential.
    assert_eq!(
        classify("sh", &["-c", "git status && git log --oneline"]),
        CommandClass::Safe
    );
}

fn git_effects(words: &[&str]) -> leveler_execution::GitCommandEffects {
    let args = words
        .iter()
        .map(|word| word.to_string())
        .collect::<Vec<_>>();
    leveler_execution::git_command_effects(&args).expect("Git invocation")
}

#[test]
fn remote_queries_report_transport_effects_and_targets() {
    for words in [
        vec!["git", "remote", "show", "origin"],
        vec!["git", "remote", "-v", "show", "origin"],
        vec!["git", "archive", "--remote=origin", "HEAD"],
        vec!["git", "archive", "--remote", "origin", "HEAD"],
    ] {
        let effects = git_effects(&words);
        assert!(effects.resolved, "{words:?}");
        assert!(effects.effects.remote_read, "{words:?}");
        assert!(!effects.explicit_remote, "{words:?}");
    }
    for words in [
        vec!["git", "remote", "show", "-n", "origin"],
        vec!["git", "archive", "HEAD"],
    ] {
        assert!(!git_effects(&words).effects.remote_read, "{words:?}");
    }
    for words in [
        vec!["git", "fetch", "host:repo"],
        vec!["git", "ls-remote", "nested/repo"],
        vec!["git", "archive", "--remote=ssh://host/repo", "HEAD"],
        vec!["git", "remote", "show", "https://host/repo"],
    ] {
        assert!(git_effects(&words).explicit_remote, "{words:?}");
        assert_eq!(
            classify("git", &words[1..]),
            CommandClass::Dangerous,
            "{words:?}"
        );
    }
}

#[test]
fn unsupported_git_global_options_cannot_grant_metadata_scope() {
    for words in [
        vec!["git", "--bare", "add", "file"],
        vec!["git", "--unknown-option", "fetch", "origin"],
        vec!["git", "--shallow-file", "/tmp/shallow", "fetch", "origin"],
    ] {
        assert!(!git_effects(&words).resolved, "{words:?}");
        assert_eq!(
            classify("git", &words[1..]),
            CommandClass::Dangerous,
            "{words:?}"
        );
    }
    assert_eq!(
        classify("git", &["--no-pager", "status"]),
        CommandClass::Safe
    );
}

#[test]
fn transport_executable_overrides_do_not_inherit_remote_read_authority() {
    for words in [
        vec!["git", "fetch", "--upload-pack=/tmp/helper", "origin"],
        vec!["git", "ls-remote", "--upload-pack", "/tmp/helper", "origin"],
        vec![
            "git",
            "archive",
            "--remote=origin",
            "--exec=/tmp/helper",
            "HEAD",
        ],
    ] {
        assert!(!git_effects(&words).resolved, "{words:?}");
        assert_eq!(
            classify("git", &words[1..]),
            CommandClass::Dangerous,
            "{words:?}"
        );
    }
}
