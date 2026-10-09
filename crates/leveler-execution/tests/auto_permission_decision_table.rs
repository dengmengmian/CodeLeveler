//! The Auto Permission Contract, as a decision table.
//!
//! Auto ("替我审批") allows ordinary development operations and asks only for
//! dangerous, destructive, or boundary-crossing ones. This file pins that
//! contract at the policy layer — the layer that decides ALLOW vs ASK — so a
//! classification regression fails here, cheaply and deterministically, before
//! anyone reaches for a real sandbox.
//!
//! The sandbox half of the same contract is proven by
//! `auto_permission_sandbox.rs`, which runs the SAME operations through a real
//! `sandbox-exec` on macOS. A policy table that passes while the sandbox still
//! answers EPERM is exactly the double-truth-source failure this pair exists
//! to make impossible.

use leveler_execution::{
    ApprovalPolicy, CommandView, PermissionProfile, Requirement, RiskLevel, classify_command,
};

/// The requirement Auto (Assisted) produces for one shell command.
fn auto(command_line: &str) -> Requirement {
    let (program, args) = leveler_execution::shell_invocation(command_line);
    ApprovalPolicy::default().evaluate(
        PermissionProfile::Assisted,
        "shell_command",
        RiskLevel::WorkspaceWrite,
        Some(CommandView {
            program: &program,
            args: &args,
        }),
    )
}

/// The requirement Full produces for one shell command.
fn full(command_line: &str) -> Requirement {
    let (program, args) = leveler_execution::shell_invocation(command_line);
    ApprovalPolicy::default().evaluate(
        PermissionProfile::FullAccess,
        "shell_command",
        RiskLevel::WorkspaceWrite,
        Some(CommandView {
            program: &program,
            args: &args,
        }),
    )
}

fn assert_allowed(command_line: &str) {
    assert_eq!(
        auto(command_line),
        Requirement::Auto,
        "Auto must allow: {command_line}"
    );
    assert_eq!(
        full(command_line),
        Requirement::Auto,
        "Full must allow: {command_line}"
    );
}

fn assert_asked(command_line: &str) {
    assert_eq!(
        auto(command_line),
        Requirement::NeedApproval,
        "Auto must ask for a dangerous operation: {command_line}"
    );
    assert_eq!(
        full(command_line),
        Requirement::Auto,
        "Full bypasses the gate entirely"
    );
}

#[test]
fn auto_allows_process_and_system_inspection() {
    for command_line in [
        "ps ax -o pid,comm",
        "ps aux",
        "ps -ef",
        "top -l 1 -n 1",
        "sysctl -n kern.argmax",
        "sysctl -n hw.ncpu",
        "lsof -nP -iTCP -sTCP:LISTEN",
        "uname -a",
        "df -h",
        "vm_stat",
    ] {
        assert_allowed(command_line);
    }
}

#[test]
fn auto_allows_read_only_git_in_every_ordinary_spelling() {
    for command_line in [
        "git status --porcelain",
        "git status --porcelain=v1 --branch",
        "git log --oneline -3",
        "git log --oneline -3 | head -5",
        "git diff --stat",
        "git diff",
        "git diff HEAD~1",
        "git rev-parse HEAD",
        "git rev-parse --abbrev-ref HEAD",
        "git show --stat HEAD",
        "git branch --show-current",
        "git ls-files",
        "git remote -v",
        "git stash list",
        "git --no-pager log --oneline -3",
        // Compound read-only Git stays read-only Git: a `;` between two reads
        // must not turn the call into an unknown quantity.
        "git log --oneline -3; git status --porcelain",
        "git status && git log --oneline -1",
        // A relocated read is still a read. `-C`/`--git-dir` move the target,
        // they do not change what the subcommand does.
        "git -C . status --porcelain",
        "git -C . log --oneline -3",
        "git -C /tmp/elsewhere status",
        "git --git-dir=/tmp/elsewhere log --oneline -1",
    ] {
        assert_allowed(command_line);
    }
}

#[test]
fn auto_allows_temporary_and_workspace_file_operations() {
    // The policy layer allows these; the sandbox must too (see the companion
    // execution test).
    for command_line in [
        "echo hi > /tmp/codeleveler-auto-test.txt",
        "echo hi >> /tmp/codeleveler-auto-test.txt",
        "cat /tmp/codeleveler-auto-test.txt",
        "mktemp",
        "echo hi > \"$TMPDIR/codeleveler-auto-test.txt\"",
        "echo hi > workspace-file.txt",
        "cat workspace-file.txt",
        "cargo test",
        "cargo build",
        "npm run build",
    ] {
        // `shell_invocation` executes cmd on Windows: exercise native temporary
        // destinations there instead of POSIX /tmp and $TMPDIR spellings.
        #[cfg(windows)]
        let native_command_line = command_line
            .replace("/tmp/", "%TEMP%\\")
            .replace("$TMPDIR/", "%TEMP%\\");
        #[cfg(windows)]
        let command_line = native_command_line.as_str();
        assert_allowed(command_line);
    }
}

/// P6: relaxing ordinary capabilities must not relax the dangerous ones. Every
/// entry here is an operation the product deliberately gates.
#[test]
fn auto_still_asks_for_dangerous_operations() {
    for command_line in [
        // Destructive local filesystem.
        "rm -rf /tmp/anything",
        "rm -r build",
        // `rm` is a destructive PROGRAM, so it is gated whatever its target —
        // including cleanup of a file this task created in `/tmp`. The sandbox
        // now permits that write (see the companion execution test); the
        // decision to delete is still a human's.
        "rm -f /tmp/codeleveler-auto-test.txt",
        "rm -f workspace-file.txt",
        "rmdir build",
        "dd if=/dev/zero of=/dev/disk2",
        "mkfs /dev/disk3",
        "shutdown -h now",
        // A write outside the workspace that is not an ordinary temp file
        // stays gated: only the shared temp root is exempt.
        "echo x > /etc/hosts",
        "echo x > /Users/someone/elsewhere/file",
        "echo x > /tmp/../etc/passwd",
        "echo x > ../../outside.txt",
        // Destructive or history-rewriting Git.
        "git reset --hard",
        "git clean -fd",
        "git checkout -- .",
        "git restore --worktree .",
        "git rebase main",
        "git merge origin/main",
        "git branch -D feature",
        "git push",
        "git push --force origin main",
        "git filter-branch --all",
        "git remote set-url origin https://evil.example/x",
        "git config core.hooksPath /tmp/evil",
        "git update-ref -d refs/heads/main",
        // Git configuration injection is not read-only Git whatever the
        // subcommand is.
        "git -c core.hooksPath=/tmp/evil status",
        "git -c core.fsmonitor=/tmp/evil diff",
        // Privilege escalation.
        "sudo rm -rf /",
        "su root",
        // Host escape.
        "open index.html",
    ] {
        assert_asked(command_line);
    }
}

/// The declared-risk half of the same table: a tool that is destructive or
/// privileged by nature needs a decision under Auto and never under Full.
#[test]
fn non_command_tools_follow_the_same_risk_table() {
    let policy = ApprovalPolicy::default();
    for risk in [
        RiskLevel::Safe,
        RiskLevel::WorkspaceWrite,
        RiskLevel::Network,
    ] {
        assert_eq!(
            policy.evaluate(PermissionProfile::Assisted, "write_file", risk, None),
            Requirement::Auto,
            "{risk:?} is ordinary development under Auto"
        );
    }
    for risk in [RiskLevel::Destructive, RiskLevel::Privileged] {
        assert_eq!(
            policy.evaluate(PermissionProfile::Assisted, "write_file", risk, None),
            Requirement::NeedApproval,
            "{risk:?} needs a decision under Auto"
        );
    }
    for risk in [
        RiskLevel::Safe,
        RiskLevel::WorkspaceWrite,
        RiskLevel::Network,
        RiskLevel::Destructive,
        RiskLevel::Privileged,
    ] {
        assert_eq!(
            policy.evaluate(PermissionProfile::FullAccess, "write_file", risk, None),
            Requirement::Auto,
            "Full is never gated"
        );
    }
}

/// A sanity check on the table itself: the classifier that feeds Auto must not
/// have quietly started calling a read-only Git command dangerous.
#[test]
fn the_classifier_agrees_that_read_only_git_is_not_dangerous() {
    for command_line in [
        "git status --porcelain",
        "git log --oneline -3",
        "git diff",
        "git rev-parse HEAD",
        "git -C . status",
    ] {
        let (program, args) = leveler_execution::shell_invocation(command_line);
        assert_eq!(
            classify_command(&CommandView {
                program: &program,
                args: &args,
            }),
            leveler_execution::CommandClass::Safe,
            "{command_line} is a read"
        );
    }
}
