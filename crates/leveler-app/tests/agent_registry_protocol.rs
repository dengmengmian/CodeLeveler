//! AGENT_REGISTRY_REMOTE_READ — the agent registry through the client
//! protocol: clients list, read, create, update and delete definitions
//! without touching the files, and the runtime validates and writes them.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{
    ClientCommand, CommandId, InteractiveRuntimeClient, RuntimeEvent, SessionId, UiAgentCapability,
    UiAgentDraft, UiAgentScope, UiAgentSource, UiAgentStatus,
};
use leveler_execution::PermissionProfile;
use leveler_model::ModelRef;
use leveler_project::Layout;

/// One process-wide home: the installed environment is first-install-wins.
fn home() -> &'static Path {
    static HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    HOME.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        let _ = leveler_core::install_environment(leveler_core::EnvSnapshot::new(
            [(
                std::ffi::OsString::from("LEVELER_HOME"),
                dir.path().as_os_str().to_os_string(),
            )],
            std::env::temp_dir(),
            std::env::temp_dir(),
        ));
        unsafe {
            std::env::set_var("LEVELER_HOME", dir.path());
        }
        dir
    })
    .path()
}

struct Fixture {
    _tmp: tempfile::TempDir,
    repo: PathBuf,
    client: Arc<InProcessRuntimeClient>,
}

impl Fixture {
    fn new() -> Self {
        home();
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let layout =
            Layout::from_parts(repo.clone(), repo.join("configs"), tmp.path().join("state"));
        let app = Arc::new(Application::assemble(layout).expect("assemble"));
        let client = Arc::new(InProcessRuntimeClient::new(
            app,
            ModelRef::parse("deepseek/test-model").unwrap(),
            PermissionProfile::Assisted,
            false,
        ));
        Self {
            _tmp: tmp,
            repo,
            client,
        }
    }

    fn agent_file(&self, name: &str, yaml_body: &str, instructions: &str) {
        let dir = self.repo.join(".leveler/agents").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("agent.yaml"),
            format!("version: 1\nname: {name}\ndescription: The {name} agent.\n{yaml_body}"),
        )
        .unwrap();
        std::fs::write(dir.join("instructions.md"), instructions).unwrap();
    }

    /// Send `command` and return the first event `pick` accepts.
    async fn ask<T>(&self, command: ClientCommand, pick: impl Fn(RuntimeEvent) -> Option<T>) -> T {
        let mut rx = self.client.subscribe();
        self.client.send(command).await.unwrap();
        loop {
            match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
                Ok(Ok(event)) => {
                    if let Some(found) = pick(event) {
                        return found;
                    }
                }
                other => panic!("no answer: {other:?}"),
            }
        }
    }
}

fn sid() -> SessionId {
    SessionId::new("agents-session")
}

fn draft(name: &str, capability: UiAgentCapability) -> UiAgentDraft {
    UiAgentDraft {
        name: name.into(),
        description: format!("The {name} agent."),
        capability,
        model: None,
        reasoning_effort: None,
        skills: Vec::new(),
        tools: None,
        write_roots: Vec::new(),
        max_rounds: None,
        max_duration_secs: None,
        instructions: format!("Instructions for {name}.\n"),
    }
}

async fn list(fx: &Fixture) -> Vec<leveler_client_protocol::UiAgentEntry> {
    let q = CommandId::generate();
    let want = q.clone();
    fx.ask(
        ClientCommand::ListAgents {
            session_id: sid(),
            query_id: Some(q),
        },
        move |e| match e {
            RuntimeEvent::AgentsLoaded {
                query_id, agents, ..
            } if query_id.as_ref() == Some(&want) => Some(agents),
            _ => None,
        },
    )
    .await
}

async fn mutate(fx: &Fixture, command: ClientCommand) -> (bool, Option<String>) {
    fx.ask(command, |e| match e {
        RuntimeEvent::AgentMutated { ok, error, .. } => Some((ok, error)),
        _ => None,
    })
    .await
}

#[tokio::test]
async fn listing_resolves_sources_status_and_shadowing() {
    let fx = Fixture::new();
    fx.agent_file("security-reviewer", "capability: read_only\n", "Review.");
    fx.agent_file("broken", "capability: read_only\nwirte: true\n", "x");
    fx.agent_file(
        "far-model",
        "capability: read_only\nmodel: nowhere/big\n",
        "x",
    );
    fx.agent_file(
        "code-reviewer",
        "capability: read_only\n",
        "Project override.",
    );

    let agents = list(&fx).await;
    let get = |n: &str| {
        agents
            .iter()
            .find(|a| a.name == n)
            .unwrap_or_else(|| panic!("{n}"))
    };

    let sr = get("security-reviewer");
    assert_eq!(sr.source, UiAgentSource::Project);
    assert_eq!(sr.status, UiAgentStatus::Available);
    assert_eq!(sr.capability, Some(UiAgentCapability::ReadOnly));
    assert!(
        sr.location
            .as_deref()
            .unwrap()
            .ends_with("security-reviewer")
    );
    assert!(sr.fingerprint.as_deref().unwrap().starts_with("sha256:"));

    let broken = get("broken");
    assert_eq!(broken.status, UiAgentStatus::Invalid);
    assert!(broken.reason.as_deref().unwrap().contains("wirte"));

    let far = get("far-model");
    assert_eq!(far.status, UiAgentStatus::Unavailable);
    assert!(far.reason.as_deref().unwrap().contains("nowhere/big"));

    let cr = get("code-reviewer");
    assert_eq!(cr.source, UiAgentSource::Project);
    assert_eq!(cr.shadowed[0].source, UiAgentSource::Builtin);

    let explorer = get("explorer");
    assert!(explorer.structural && !explorer.harness_only);
    assert!(get("reviewer").harness_only);
    let names: Vec<&str> = agents.iter().map(|a| a.name.as_str()).collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted, "stable order");
}

#[tokio::test]
async fn get_returns_the_instructions_or_says_why_not() {
    let fx = Fixture::new();
    fx.agent_file(
        "security-reviewer",
        "capability: read_only\n",
        "Only exploitable issues.",
    );
    let detail = fx
        .ask(
            ClientCommand::GetAgent {
                session_id: sid(),
                name: "security-reviewer".into(),
                query_id: None,
            },
            |e| match e {
                RuntimeEvent::AgentLoaded { agent, error, .. } => Some((agent, error)),
                _ => None,
            },
        )
        .await;
    let agent = detail.0.expect("resolves");
    assert_eq!(
        agent.instructions.as_deref(),
        Some("Only exploitable issues.")
    );

    let missing = fx
        .ask(
            ClientCommand::GetAgent {
                session_id: sid(),
                name: "nope".into(),
                query_id: None,
            },
            |e| match e {
                RuntimeEvent::AgentLoaded { agent, error, .. } => Some((agent, error)),
                _ => None,
            },
        )
        .await;
    assert!(missing.0.is_none());
    assert!(missing.1.unwrap().contains("not found"));
}

#[tokio::test]
async fn create_update_delete_write_through_the_runtime() {
    let fx = Fixture::new();
    let (ok, error) = mutate(
        &fx,
        ClientCommand::CreateAgent {
            session_id: sid(),
            scope: UiAgentScope::Project,
            draft: Box::new(draft("frontend-worker", UiAgentCapability::ScopedWriter)),
            query_id: None,
        },
    )
    .await;
    assert!(ok, "{error:?}");
    let dir = fx.repo.join(".leveler/agents/frontend-worker");
    assert!(dir.join("agent.yaml").is_file() && dir.join("instructions.md").is_file());

    let mut updated = draft("frontend-worker", UiAgentCapability::ScopedWriter);
    updated.write_roots = vec!["web".into()];
    updated.instructions = "Frontend only.\n".into();
    let (ok, error) = mutate(
        &fx,
        ClientCommand::UpdateAgent {
            session_id: sid(),
            scope: UiAgentScope::Project,
            draft: Box::new(updated),
            query_id: None,
        },
    )
    .await;
    assert!(ok, "{error:?}");
    let entry = list(&fx)
        .await
        .into_iter()
        .find(|a| a.name == "frontend-worker")
        .unwrap();
    assert_eq!(entry.write_roots, vec!["web"]);
    assert_eq!(
        std::fs::read_to_string(dir.join("instructions.md")).unwrap(),
        "Frontend only.\n"
    );

    let (ok, error) = mutate(
        &fx,
        ClientCommand::DeleteAgent {
            session_id: sid(),
            scope: UiAgentScope::Project,
            name: "frontend-worker".into(),
            query_id: None,
        },
    )
    .await;
    assert!(ok, "{error:?}");
    assert!(!dir.exists());
    assert!(list(&fx).await.iter().all(|a| a.name != "frontend-worker"));
}

#[tokio::test]
async fn a_contradictory_draft_is_refused_and_nothing_is_written() {
    let fx = Fixture::new();
    let mut contradiction = draft("ro-writer", UiAgentCapability::ReadOnly);
    contradiction.tools = Some(vec!["read_file".into(), "apply_patch".into()]);
    let (ok, error) = mutate(
        &fx,
        ClientCommand::CreateAgent {
            session_id: sid(),
            scope: UiAgentScope::Project,
            draft: Box::new(contradiction),
            query_id: None,
        },
    )
    .await;
    assert!(!ok);
    assert!(error.unwrap().contains("apply_patch"));
    assert!(!fx.repo.join(".leveler/agents/ro-writer").exists());

    let (ok, error) = mutate(
        &fx,
        ClientCommand::CreateAgent {
            session_id: sid(),
            scope: UiAgentScope::Project,
            draft: Box::new(draft("worker", UiAgentCapability::ScopedWriter)),
            query_id: None,
        },
    )
    .await;
    assert!(!ok);
    assert!(error.unwrap().contains("built-in"));
}

#[tokio::test]
async fn a_user_scope_agent_lands_in_the_home_and_is_listed_as_user() {
    let fx = Fixture::new();
    let (ok, error) = mutate(
        &fx,
        ClientCommand::CreateAgent {
            session_id: sid(),
            scope: UiAgentScope::User,
            draft: Box::new(draft("rust-explorer-ui", UiAgentCapability::ReadOnly)),
            query_id: None,
        },
    )
    .await;
    assert!(ok, "{error:?}");
    assert!(home().join("agents/rust-explorer-ui/agent.yaml").is_file());
    let entry = list(&fx)
        .await
        .into_iter()
        .find(|a| a.name == "rust-explorer-ui")
        .unwrap();
    assert_eq!(entry.source, UiAgentSource::User);
}
