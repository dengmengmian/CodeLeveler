//! Shell write fence over persistent policy files.
//!
//! `.leveler/agents`, `.leveler/skills` and `AGENTS.md` are loaded as authority
//! by a later session. A confined shell must not be able to author or modify
//! them: doing so would create instructions nobody approved. The file tools
//! check these paths; this canary proves the OS boundary covers the same
//! resources for an arbitrary shell, PowerShell or cmd child.
//!
//! Only an action that is allowed to write the workspace runs here. The
//! control at the end proves the fence is not simply "all writes fail".
//!
//! The missing-path cases run on macOS (Seatbelt path rules) and Linux (the
//! `seccomp` supervisor in `leveler-linux-confine`). Windows is covered by the
//! integrity-label canaries and its own CI job.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use leveler_execution::{CommandRunner, ProcessOutput, ProcessRequest, WriteScope};
use tokio_util::sync::CancellationToken;

fn runner(home: &Path) -> CommandRunner {
    let mut values: Vec<(std::ffi::OsString, std::ffi::OsString)> = std::env::vars_os().collect();
    values.push((
        std::ffi::OsString::from("LEVELER_HOME"),
        home.as_os_str().to_os_string(),
    ));
    CommandRunner::with_environment(Arc::new(leveler_core::EnvSnapshot::new(
        values,
        std::env::current_dir().unwrap_or_default(),
        std::env::temp_dir(),
    )))
}

async fn confined(runner: &CommandRunner, workspace: &Path, script: &str) -> ProcessOutput {
    let mut request = ProcessRequest::new(
        "sh",
        vec!["-c".to_string(), script.to_string()],
        workspace.to_path_buf(),
    );
    request.timeout = Duration::from_secs(60);
    request.write_scope = WriteScope::Workspace {
        root: workspace.to_path_buf(),
    };
    request.unrestricted_execution = false;
    runner
        .run(request, CancellationToken::new())
        .await
        .unwrap_or_else(|error| panic!("sh -c {script:?} failed to run: {error:?}"))
}

fn detail(out: &ProcessOutput) -> String {
    format!(
        "exit={:?} stdout={:?} stderr={:?}",
        out.exit_code, out.stdout, out.stderr
    )
}

fn workspace() -> (tempfile::TempDir, tempfile::TempDir, PathBuf) {
    let home = tempfile::tempdir().expect("home");
    let root = tempfile::tempdir().expect("workspace");
    let path = root.path().canonicalize().expect("canonical workspace");
    (home, root, path)
}

/// A definition that already exists is not overwritten and a new one is not
/// created inside an existing protected directory.
#[tokio::test]
async fn a_confined_shell_cannot_write_agent_or_skill_definitions() {
    let (home, _root, ws) = workspace();
    std::fs::create_dir_all(ws.join(".leveler")).expect("leveler dir");
    let agent = ws.join(".leveler/agents/evil/agent.yaml");
    let skill = ws.join(".leveler/skills/evil/SKILL.md");
    std::fs::create_dir_all(agent.parent().unwrap()).unwrap();
    std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
    std::fs::write(&agent, "ORIGINAL-AGENT").unwrap();
    std::fs::write(&skill, "ORIGINAL-SKILL").unwrap();
    let runner = runner(home.path());

    let out = confined(
        &runner,
        &ws,
        "printf PWNED > .leveler/agents/evil/agent.yaml; \
         printf PWNED > .leveler/skills/evil/SKILL.md; \
         mkdir -p .leveler/agents/new && printf PWNED > .leveler/agents/new/agent.yaml",
    )
    .await;
    assert_eq!(
        std::fs::read_to_string(&agent).unwrap(),
        "ORIGINAL-AGENT",
        "an existing agent definition must stay write-denied: {}",
        detail(&out)
    );
    assert_eq!(
        std::fs::read_to_string(&skill).unwrap(),
        "ORIGINAL-SKILL",
        "an existing skill definition must stay write-denied: {}",
        detail(&out)
    );
    assert!(
        !ws.join(".leveler/agents/new/agent.yaml").exists(),
        "a new definition below an existing protected directory must not be created: {}",
        detail(&out)
    );
}

/// The protected directory itself does not exist yet. macOS denies it with
/// the seatbelt path rule; Linux denies it with the syscall supervisor.
#[tokio::test]
async fn a_confined_shell_cannot_create_the_definition_directory() {
    let (home, _root, ws) = workspace();
    std::fs::create_dir_all(ws.join(".leveler")).expect("leveler dir");
    let runner = runner(home.path());

    let out = confined(
        &runner,
        &ws,
        "mkdir -p .leveler/agents/evil && printf PWNED > .leveler/agents/evil/agent.yaml",
    )
    .await;
    assert!(
        !ws.join(".leveler/agents/evil/agent.yaml").exists(),
        "a definition directory created after admission must not become writable: {}",
        detail(&out)
    );
}

/// `AGENTS.md` is policy text the next session loads. A command may not rewrite
/// it behind the file tool's path check.
#[tokio::test]
async fn a_confined_shell_cannot_modify_an_existing_agents_file() {
    let (home, _root, ws) = workspace();
    let agents = ws.join("AGENTS.md");
    std::fs::write(&agents, "ORIGINAL-RULES").unwrap();
    let runner = runner(home.path());

    let out = confined(&runner, &ws, "printf 'PWNED-RULES' > AGENTS.md").await;
    assert_eq!(
        std::fs::read_to_string(&agents).unwrap(),
        "ORIGINAL-RULES",
        "AGENTS.md is policy text and must stay write-denied: {}",
        detail(&out)
    );
}

/// Creating the policy file from nothing is the same authority as modifying
/// it: the next session would load the new text.
#[tokio::test]
async fn a_confined_shell_cannot_create_an_agents_file() {
    let (home, _root, ws) = workspace();
    let agents = ws.join("AGENTS.md");
    let runner = runner(home.path());

    let out = confined(&runner, &ws, "printf 'PWNED-RULES' > AGENTS.md").await;
    assert!(
        !agents.exists(),
        "a policy file created by a confined command would be loaded later: {}",
        detail(&out)
    );
}

/// Control: the same runner, scope and workspace still allows an ordinary
/// project write. A canary that also blocked this would prove nothing about
/// policy-file protection.
#[tokio::test]
async fn an_ordinary_workspace_write_still_succeeds() {
    let (home, _root, ws) = workspace();
    let runner = runner(home.path());

    let out = confined(&runner, &ws, "printf fine > ordinary.txt").await;
    assert!(
        out.success() && ws.join("ordinary.txt").is_file(),
        "ordinary workspace write must remain allowed: {}",
        detail(&out)
    );
}

/// A workspace with no `.leveler` at all must not gain a definition directory.
#[tokio::test]
async fn a_confined_shell_cannot_create_a_missing_leveler_definition_directory() {
    let (home, _root, ws) = workspace();
    let runner = runner(home.path());

    let out = confined(
        &runner,
        &ws,
        "mkdir -p .leveler/skills/evil && printf PWNED > .leveler/skills/evil/SKILL.md",
    )
    .await;
    assert!(
        !ws.join(".leveler/skills/evil/SKILL.md").exists(),
        "a skills directory created after admission must not become writable: {}",
        detail(&out)
    );
    assert!(
        !ws.join(".leveler/skills").exists(),
        "the protected directory itself must not be created: {}",
        detail(&out)
    );
}

/// Renaming a temporary file onto a protected name is the same authority as
/// writing it directly.
#[tokio::test]
async fn a_confined_shell_cannot_rename_into_a_protected_path() {
    let (home, _root, ws) = workspace();
    std::fs::create_dir_all(ws.join(".leveler")).expect("leveler dir");
    let runner = runner(home.path());

    let out = confined(
        &runner,
        &ws,
        "printf PWNED > tmp && mv tmp AGENTS.md; \
         printf PWNED > tmp2 && mv tmp2 .leveler/agents",
    )
    .await;
    assert!(
        !ws.join("AGENTS.md").exists(),
        "rename must not create the policy file: {}",
        detail(&out)
    );
    assert!(
        !ws.join(".leveler/agents").exists(),
        "rename must not create the definition directory: {}",
        detail(&out)
    );
    let _ = std::fs::remove_file(ws.join("tmp"));
    let _ = std::fs::remove_file(ws.join("tmp2"));
}

/// A directory alias is resolved before the decision: `alias/AGENTS.md` names
/// the same policy file the workspace root would.
#[tokio::test]
async fn a_confined_shell_cannot_reach_the_policy_file_through_an_alias() {
    let (home, _root, ws) = workspace();
    let runner = runner(home.path());

    let out = confined(
        &runner,
        &ws,
        "ln -s . alias && printf PWNED > alias/AGENTS.md",
    )
    .await;
    assert!(
        !ws.join("AGENTS.md").exists(),
        "a directory alias must not bypass the policy fence: {}",
        detail(&out)
    );
}

/// `AGENTS.md` is loaded from subdirectories too, so a nested one is the same
/// authority as the root file.
#[tokio::test]
async fn a_confined_shell_cannot_create_a_nested_agents_file() {
    let (home, _root, ws) = workspace();
    let runner = runner(home.path());

    let out = confined(&runner, &ws, "mkdir -p src && printf PWNED > src/AGENTS.md").await;
    assert!(
        !ws.join("src/AGENTS.md").exists(),
        "nested project instructions must stay write-denied: {}",
        detail(&out)
    );

    // An existing nested file is protected too, and a rename is the same
    // authority as a write.
    std::fs::write(ws.join("src/AGENTS.md"), "ORIGINAL-NESTED").expect("seed nested");
    let out = confined(
        &runner,
        &ws,
        "printf PWNED > src/AGENTS.md; printf PWNED > tmp && mv tmp src/AGENTS.md",
    )
    .await;
    assert_eq!(
        std::fs::read_to_string(ws.join("src/AGENTS.md")).unwrap(),
        "ORIGINAL-NESTED",
        "existing nested instructions and rename targets stay write-denied: {}",
        detail(&out)
    );
    let _ = std::fs::remove_file(ws.join("tmp"));
}

/// The fence is a write boundary, not a command ban: the blocked attempts must
/// leave the rest of the tree exactly as it was, including the ordinary file a
/// control write creates.
#[tokio::test]
async fn the_fence_leaves_the_workspace_tree_unchanged() {
    let (home, _root, ws) = workspace();
    std::fs::create_dir_all(ws.join(".leveler")).expect("leveler dir");
    std::fs::write(ws.join("keep.txt"), "KEEP").expect("seed file");
    let before = tree(&ws);
    let runner = runner(home.path());

    let out = confined(
        &runner,
        &ws,
        "printf PWNED > AGENTS.md; mkdir -p .leveler/agents; \
         printf PWNED > .leveler/agents/x; printf fine > ordinary.txt",
    )
    .await;
    assert!(
        out.success(),
        "the control write must still run: {}",
        detail(&out)
    );
    let after = tree(&ws);
    let expected = before
        .iter()
        .cloned()
        .chain(std::iter::once((
            "ordinary.txt".to_string(),
            b"fine".to_vec(),
        )))
        .collect::<Vec<_>>();
    assert_eq!(
        after,
        expected,
        "a denied attempt must not materialize placeholders: {}",
        detail(&out)
    );
}

/// Unrestricted execution is the product's FullAccess equivalent and keeps
/// its documented freedom: the same shell may write the policy file.
#[tokio::test]
async fn unrestricted_execution_can_write_the_policy_file() {
    let (home, _root, ws) = workspace();
    let runner = runner(home.path());
    let mut request = ProcessRequest::new(
        "sh",
        vec!["-c".to_string(), "printf FULL > AGENTS.md".to_string()],
        ws.clone(),
    );
    request.timeout = Duration::from_secs(60);
    request.write_scope = leveler_execution::WriteScope::Unrestricted;
    request.unrestricted_execution = true;
    let out = runner
        .run(request, CancellationToken::new())
        .await
        .expect("unrestricted run");
    assert!(
        out.success(),
        "unrestricted execution must not be fenced: {}",
        detail(&out)
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("AGENTS.md")).unwrap(),
        "FULL"
    );
}

/// The fence covers Git metadata as well: a missing `.git` is not creatable
/// from a confined shell, matching the read-only bind of an existing one.
#[tokio::test]
async fn a_confined_shell_cannot_create_git_metadata() {
    let (home, _root, ws) = workspace();
    let runner = runner(home.path());

    let out = confined(&runner, &ws, "mkdir .git && printf PWNED > .git/config").await;
    assert!(
        !ws.join(".git").exists(),
        "Git metadata must stay non-creatable under confinement: {}",
        detail(&out)
    );
}

fn tree(root: &Path) -> Vec<(String, Vec<u8>)> {
    fn walk(root: &Path, directory: &Path, out: &mut Vec<(String, Vec<u8>)>) {
        let mut entries = std::fs::read_dir(directory)
            .expect("read workspace")
            .collect::<Result<Vec<_>, _>>()
            .expect("read workspace entry");
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let relative = path.strip_prefix(root).expect("relative");
            out.push((relative.to_string_lossy().into_owned(), Vec::new()));
            if entry.file_type().expect("file type").is_dir() {
                walk(root, &path, out);
            } else {
                out.last_mut().expect("pushed").1 = std::fs::read(&path).expect("read file");
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out
}

/// Deleting a protected path and recreating it is two attacks on the same
/// authority: the delete must fail and the recreate must not produce a file.
#[tokio::test]
async fn a_confined_shell_cannot_delete_and_recreate_a_protected_path() {
    let (home, _root, ws) = workspace();
    let agents = ws.join("AGENTS.md");
    std::fs::write(&agents, "ORIGINAL-RULES").unwrap();
    let definition = ws.join(".leveler/agents/evil/agent.yaml");
    std::fs::create_dir_all(definition.parent().unwrap()).unwrap();
    std::fs::write(&definition, "ORIGINAL-AGENT").unwrap();
    let runner = runner(home.path());

    let out = confined(
        &runner,
        &ws,
        "rm -f AGENTS.md; printf PWNED > AGENTS.md; \
         rm -f .leveler/agents/evil/agent.yaml; printf PWNED > .leveler/agents/evil/agent.yaml",
    )
    .await;
    assert_eq!(
        std::fs::read_to_string(&agents).unwrap(),
        "ORIGINAL-RULES",
        "delete+recreate must leave the policy file intact: {}",
        detail(&out)
    );
    assert_eq!(
        std::fs::read_to_string(&definition).unwrap(),
        "ORIGINAL-AGENT",
        "delete+recreate must leave the definition intact: {}",
        detail(&out)
    );
}

/// The other ways to give a protected name a new inode: a hard link, a
/// symbolic link, and a FIFO. All three are creation syscalls with a
/// destination the fence must refuse.
#[tokio::test]
async fn a_confined_shell_cannot_link_or_mkfifo_a_protected_path() {
    let (home, _root, ws) = workspace();
    std::fs::create_dir_all(ws.join(".leveler")).expect("leveler dir");
    let runner = runner(home.path());

    let out = confined(
        &runner,
        &ws,
        "printf PWNED > outside.txt; \
         ln outside.txt AGENTS.md; \
         ln -s outside.txt .leveler/agents; \
         mkfifo .leveler/skills",
    )
    .await;
    assert!(
        !ws.join("AGENTS.md").exists(),
        "a hard link must not create the policy file: {}",
        detail(&out)
    );
    assert!(
        std::fs::symlink_metadata(ws.join(".leveler/agents")).is_err(),
        "a symlink must not create the definition directory: {}",
        detail(&out)
    );
    assert!(
        std::fs::symlink_metadata(ws.join(".leveler/skills")).is_err(),
        "a FIFO must not create the definition directory: {}",
        detail(&out)
    );
    let _ = std::fs::remove_file(ws.join("outside.txt"));
}

/// The fence sits in front of every creation syscall, so an ordinary
/// build-like burst must stay fast: 500 file creations go through the
/// supervisor and all succeed. The bound only catches pathological overhead.
#[tokio::test]
async fn a_build_like_write_burst_stays_fast_under_the_fence() {
    let (home, _root, ws) = workspace();
    let runner = runner(home.path());
    let started = std::time::Instant::now();
    let out = confined(
        &runner,
        &ws,
        "i=0; while [ $i -lt 500 ]; do printf x > \"f$i\"; i=$((i+1)); done",
    )
    .await;
    assert!(out.success(), "the burst must run: {}", detail(&out));
    let files = std::fs::read_dir(&ws)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with('f'))
        .count();
    assert_eq!(files, 500, "every ordinary write must land");
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "500 creations took {:?}; the fence must not dominate an ordinary build",
        started.elapsed()
    );
}
