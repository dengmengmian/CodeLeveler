//! Integration tests for `leveler completions <shell>`.
//!
//! The CLI crate is bin-only, so these drive the real `leveler` binary via
//! `CARGO_BIN_EXE_leveler`. A completion script is only useful if the shell can
//! source it, so the assertions stay on what every shell needs: the binary
//! name, the subcommands, and a clean stdout with nothing else mixed in.

use std::process::Command;

fn leveler() -> Command {
    Command::new(env!("CARGO_BIN_EXE_leveler"))
}

fn completions_for(shell: &str) -> String {
    let out = leveler()
        .args(["completions", shell])
        .output()
        .expect("run leveler completions");
    assert!(
        out.status.success(),
        "`leveler completions {shell}` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("completion script must be UTF-8")
}

#[test]
fn every_supported_shell_emits_a_script_naming_the_binary() {
    for shell in ["bash", "zsh", "fish", "elvish", "powershell"] {
        let script = completions_for(shell);
        assert!(script.len() > 200, "{shell} script looks empty: {script:?}");
        assert!(
            script.contains("leveler"),
            "{shell} script never names the binary"
        );
    }
}

#[test]
fn the_script_covers_the_subcommands_people_actually_type() {
    let script = completions_for("bash");
    for cmd in ["tui", "run", "sessions", "resume", "doctor", "completions"] {
        assert!(
            script.contains(cmd),
            "bash completions omit `{cmd}`, so Tab stays dead on it"
        );
    }
}

#[test]
fn the_script_goes_to_stdout_alone_so_it_can_be_sourced() {
    let out = leveler()
        .args(["completions", "zsh"])
        .output()
        .expect("run leveler completions");
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.trim().is_empty(),
        "anything on stderr ends up in the sourced file when people redirect 2>&1: {stderr}"
    );
    // A shell sources this directly; a human-facing banner would be a syntax error.
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("CodeLeveler ·") && !stdout.contains("✓"),
        "no decorative CLI output may leak into the script: {stdout:?}"
    );
}

#[test]
fn an_unknown_shell_is_rejected_with_the_valid_list() {
    let out = leveler()
        .args(["completions", "cmd.exe"])
        .output()
        .expect("run leveler completions");
    assert!(!out.status.success(), "unknown shell must not exit 0");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("bash") && stderr.contains("zsh"),
        "the error must list what IS supported: {stderr}"
    );
}
