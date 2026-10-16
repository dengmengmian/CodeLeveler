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
//!
//! One table, two shells. `sh -c` and `cmd /C` are different languages and the
//! classifier decides each on its own terms — the Bash AST for the POSIX
//! wrapper, the string classifier for `cmd` — so an entry is asserted for the
//! shell it is written in, and the two spellings of the same intent live side
//! by side (a variable remote is `$REMOTE` under `sh` and `%REMOTE%` under
//! `cmd`). Entries that mean the same thing to both shells are asserted for
//! both, on every platform: a verdict that only holds on the runner whose
//! platform happens to execute it is not a contract.

use leveler_execution::{
    ApprovalPolicy, CommandView, PermissionProfile, Requirement, RiskLevel, classify_command,
};

/// The shell a table entry is written in — and therefore the shell the entry
/// only means anything about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shell {
    /// `sh -c <line>`: the POSIX wrapper, classified on the Bash grammar.
    Bash,
    /// `cmd /C <line>`: what the product executes on Windows, classified as a
    /// cmd script.
    Cmd,
}

impl Shell {
    fn invocation(self, command_line: &str) -> (&'static str, Vec<String>) {
        let (program, flag) = match self {
            Shell::Bash => ("sh", "-c"),
            Shell::Cmd => ("cmd", "/C"),
        };
        (program, vec![flag.to_string(), command_line.to_string()])
    }
}

/// The requirement one shell produces for one command line.
fn requirement(shell: Shell, profile: PermissionProfile, command_line: &str) -> Requirement {
    let (program, args) = shell.invocation(command_line);
    ApprovalPolicy::default().evaluate(
        profile,
        "shell_command",
        RiskLevel::WorkspaceWrite,
        Some(CommandView {
            program,
            args: &args,
        }),
    )
}

fn assert_allow(shell: Shell, command_line: &str) {
    assert_eq!(
        requirement(shell, PermissionProfile::Assisted, command_line),
        Requirement::Auto,
        "Auto must allow under {shell:?}: {command_line}"
    );
    assert_eq!(
        requirement(shell, PermissionProfile::FullAccess, command_line),
        Requirement::Auto,
        "Full must allow under {shell:?}: {command_line}"
    );
}

fn assert_ask(shell: Shell, command_line: &str) {
    assert_eq!(
        requirement(shell, PermissionProfile::Assisted, command_line),
        Requirement::NeedApproval,
        "Auto must ask for a dangerous operation under {shell:?}: {command_line}"
    );
    assert_eq!(
        requirement(shell, PermissionProfile::FullAccess, command_line),
        Requirement::Auto,
        "Full bypasses the gate entirely"
    );
}

/// The line means the same thing to both shells, so every platform asserts both.
fn assert_both_allow(command_line: &str) {
    assert_allow(Shell::Bash, command_line);
    assert_allow(Shell::Cmd, command_line);
}

/// The line means the same thing to both shells, so every platform asserts both.
fn assert_both_ask(command_line: &str) {
    assert_ask(Shell::Bash, command_line);
    assert_ask(Shell::Cmd, command_line);
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
        assert_both_allow(command_line);
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
        // A variable path or revision does not change a read into a write.
        "git show \"$rev\"",
        "git log -- \"*$f*\"",
        "git diff HEAD -- \"$f\"",
        "for f in 0016_model_config 0017_performance; do git show \"HEAD:$f\"; done",
        "git status --porcelain && git show \"$f\"",
        "git -C \"$dir\" status --porcelain",
        // Reversible branch movement and merge are ordinary development.
        "git switch main",
        "git switch -c feat",
        "git checkout main",
        "git checkout -b feat",
        "git merge origin/main",
        // Configured-remote fetch inside a shell matches the direct command.
        "git fetch origin",
        "git ls-remote origin",
        "git fetch origin && git status --porcelain",
        // Git operations are ordinary development, including ones that rewrite
        // history or publish. A stored credential still asks at admission,
        // which this table does not see.
        "git reset --hard",
        "git clean -fd",
        "git clean $FLAGS",
        "git checkout -- .",
        "git checkout -f main",
        "git checkout \"$branch\"",
        "git switch --force main",
        "git restore --worktree .",
        "git rebase main",
        "git branch -D feature",
        "git pull origin",
        "git fetch https://unknown.example/repo.git",
        "git push",
        "git push --force origin main",
        "git push origin main",
        "git filter-branch --all",
        "git remote set-url origin https://evil.example/x",
        "git update-ref -d refs/heads/main",
        "git -c user.email=a@b.com commit -m x",
        "git fetch origin && echo extra",
        "git status; git clean $FLAGS",
    ] {
        assert_both_allow(command_line);
    }
    // A backup listing is a read plus an ordinary directory create.
    //
    // The POSIX spelling is written for `sh`, which walks the command
    // substitution on the Bash grammar; `cmd` has no such construct, so the
    // Windows spelling states the same intent in the language Windows runs —
    // and both are ordinary development whichever platform executes them.
    assert_allow(
        Shell::Bash,
        "mkdir -p tmp/backup && ts=$(date +%Y%m%d-%H%M%S) && echo \"backup $ts\" && for db in a b; do echo \"$db\"; done",
    );
    assert_both_allow("mkdir tmp\\backup && set ts=1 && for %d in (a b) do echo %d");
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
        // A literal relative path is inside the working directory. The sandbox
        // keeps that directory inside the workspace.
        "rm -r build",
        "rm -f workspace-file.txt",
        "rm -rf -- build/obj",
        "rm \"my file.txt\"",
        "rmdir build",
        "rmdir a/b",
        "rm -rf build && echo done",
        "cargo test",
        "cargo build",
        "npm run build",
    ] {
        assert_both_allow(command_line);
    }
}

/// P6: relaxing ordinary capabilities must not relax the dangerous ones. Every
/// entry here is an operation the product deliberately gates.
#[test]
fn auto_still_asks_for_dangerous_operations() {
    for command_line in [
        // Deletion whose target is not a literal path inside the working
        // directory. Shared temp is writable, and still outside the workspace.
        "rm -rf /tmp/anything",
        "rm -f /tmp/codeleveler-auto-test.txt",
        "rm -rf /",
        "rm -rf ../outside",
        "rm -rf a/../../etc",
        "rm -rf ~/secret",
        "rm -rf \"$dir\"",
        "rm -rf *",
        "rm build /etc/passwd",
        "rm -rf",
        "rmdir ../outside",
        "dd if=/dev/zero of=/dev/disk2",
        "mkfs /dev/disk3",
        "shutdown -h now",
        // A write outside the workspace that is not an ordinary temp file
        // stays gated: only the shared temp root is exempt.
        "echo x > /etc/hosts",
        "echo x > /Users/someone/elsewhere/file",
        "echo x > /tmp/../etc/passwd",
        "echo x > ../../outside.txt",
        // A remote we cannot name would use whatever credential the helper
        // has. An unreadable config key might be hooksPath.
        "git fetch --all",
        "git config core.hooksPath /tmp/evil",
        // Git that executes a local program.
        "git -c core.hooksPath=/tmp/evil status",
        "git -c core.fsmonitor=/tmp/evil diff",
        "git -c alias.status='!echo pwnd' status",
        "git fetch --upload-pack=/tmp/evil origin",
        "git --exec-path=/tmp/evil status",
        "GIT_SSH_COMMAND=/tmp/evil git fetch origin",
        "GIT_CONFIG_GLOBAL=/tmp/evil git status",
        // Privilege escalation.
        "sudo rm -rf /",
        "su root",
        // Host escape.
        "open index.html",
    ] {
        assert_both_ask(command_line);
    }
    // The unreadable word itself: `sh` reads `$REMOTE` as an expansion, `cmd`
    // reads `%REMOTE%` as one. Whichever shell expands it, the destination is
    // not one remote anyone can bind a credential to, so it asks — and only
    // the shell that would expand the word asks for it. `$REMOTE` under `cmd`
    // is the literal remote name `$REMOTE`, exactly as `%REMOTE%` under `sh`
    // is the literal remote named `%REMOTE%`.
    assert_ask(Shell::Bash, "git fetch $REMOTE");
    assert_ask(Shell::Bash, "git push $REMOTE");
    assert_ask(Shell::Bash, "git config $KEY value");
    assert_ask(Shell::Cmd, "git fetch %REMOTE%");
    assert_ask(Shell::Cmd, "git push %REMOTE%");
    assert_ask(Shell::Cmd, "git config %KEY% value");
    // Same rule for a deletion: an unreadable target cannot be proven to stay
    // in the workspace.
    assert_ask(Shell::Cmd, "rm -rf %dir%");
    // And for the program word itself: `sh` reads `$CMD` as the program to
    // run, `cmd` reads `%CMD%` as one, and neither can be vetted.
    assert_ask(Shell::Bash, "$CMD --version");
    assert_ask(Shell::Cmd, "%CMD% --version");
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
/// have quietly started calling a read-only Git command dangerous. This one
/// runs through `shell_invocation`, so it stays tied to the wrapper the product
/// really executes on this platform rather than to the table's own shapes.
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
