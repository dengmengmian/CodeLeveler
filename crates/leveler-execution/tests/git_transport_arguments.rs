//! Transport option values must never be mistaken for repository targets.
use leveler_execution::{
    CommandClass, CommandView, classify_command, executed_commands, git_command_effects,
};

fn effects(words: &[&str]) -> leveler_execution::GitCommandEffects {
    let argv: Vec<String> = std::iter::once("git")
        .chain(words.iter().copied())
        .map(str::to_string)
        .collect();
    git_command_effects(&argv).unwrap()
}

#[test]
fn transport_option_values_cannot_hide_arbitrary_targets() {
    for argv in [
        vec!["fetch", "--depth", "1", "https://unknown.example/repo"],
        vec!["fetch", "-j", "2", "https://unknown.example/repo"],
        vec!["fetch", "--shallow-since", "yesterday", "host:path"],
        vec!["fetch", "--depth=1", "https://unknown.example/repo"],
        vec![
            "ls-remote",
            "--sort",
            "version:refname",
            "https://unknown.example/repo",
        ],
        vec![
            "ls-remote",
            "--server-option",
            "test",
            "https://unknown.example/repo",
        ],
        vec!["pull", "--depth", "1", "https://unknown.example/repo"],
        vec!["pull", "-X", "ours", "https://unknown.example/repo"],
        vec![
            "fetch",
            "--multiple",
            "origin",
            "https://unknown.example/repo",
        ],
        vec!["fetch", "--", "https://unknown.example/repo"],
    ] {
        let git = effects(&argv);
        assert!(
            git.explicit_remote,
            "target must remain visible: {argv:?}: {git:?}"
        );
        let args: Vec<String> = argv.iter().map(|v| v.to_string()).collect();
        assert_eq!(
            classify_command(&CommandView {
                program: "git",
                args: &args
            }),
            CommandClass::Dangerous,
            "{argv:?}"
        );
    }
}

#[test]
fn known_option_values_preserve_named_remote_fetches() {
    for argv in [
        vec!["fetch", "--depth", "1", "origin"],
        vec!["fetch", "--depth=1", "origin"],
        vec!["fetch", "-j2", "origin"],
        vec!["fetch", "--shallow-exclude", "main", "origin"],
        vec!["fetch", "--recurse-submodules=on-demand", "origin"],
        vec!["fetch", "--", "origin", "refs/heads/main"],
        vec!["ls-remote", "--sort", "version:refname", "origin"],
    ] {
        let git = effects(&argv);
        assert!(git.resolved && !git.explicit_remote, "{argv:?}: {git:?}");
        let args: Vec<String> = argv.iter().map(|v| v.to_string()).collect();
        assert_eq!(
            classify_command(&CommandView {
                program: "git",
                args: &args
            }),
            CommandClass::Safe,
            "{argv:?}"
        );
    }
}

#[test]
fn unknown_or_incomplete_transport_options_fail_closed() {
    for argv in [
        vec!["fetch", "--unknown", "1", "origin"],
        vec!["fetch", "--depth"],
        vec!["fetch", "--depth="],
        vec!["ls-remote", "--sort"],
        vec!["pull", "--strategy"],
        vec!["fetch", "--upload-pack", "payload", "origin"],
        vec!["fetch", "--upload-pa=/tmp/helper", "origin"],
        vec!["fetch", "--upload-p=/tmp/helper", "origin"],
        vec!["fetch", "-é", "origin"],
    ] {
        let git = effects(&argv);
        assert!(!git.resolved, "must not guess: {argv:?}: {git:?}");
        let args: Vec<String> = argv.iter().map(|v| v.to_string()).collect();
        let executed = executed_commands("git", &args);
        let call = executed.git_effects();
        assert!(
            call.gated() && !call.needs_repository_write_scope(),
            "{argv:?}"
        );
    }
}

#[test]
fn archive_uses_the_last_remote_option_and_honors_the_sentinel() {
    assert!(
        effects(&[
            "archive",
            "--remote=origin",
            "--remote=https://unknown.example/repo",
            "HEAD"
        ])
        .explicit_remote
    );
    assert!(
        !effects(&[
            "archive",
            "--remote=https://unknown.example/repo",
            "--remote=origin",
            "HEAD"
        ])
        .explicit_remote
    );
    assert!(
        !effects(&[
            "archive",
            "--",
            "HEAD",
            "--remote=https://unknown.example/repo"
        ])
        .effects
        .remote_read
    );
    assert!(!effects(&["archive", "--remote"]).resolved);
}
