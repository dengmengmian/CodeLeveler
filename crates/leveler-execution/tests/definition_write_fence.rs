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

/// The protected directory itself does not exist yet: the shell must not be
/// able to create it and then write a definition inside.
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
