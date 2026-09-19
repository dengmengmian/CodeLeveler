//! Skill authoring through the conversation.
//!
//! The model proposes a skill with `save_skill` / `delete_skill`; the proposal
//! is validated before anyone is asked, a human decides whether it is written,
//! and the store writes it atomically. Nothing the model says can skip either
//! step, in any permission profile.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use leveler_agent::{AgentEvent, Executor, NoopSink};
use leveler_core::{RequestId, ToolCallId};
use leveler_execution::{
    ApprovalDecision, ApprovalRequest, Approver, PermissionProfile, Workspace,
};
use leveler_model::{
    ContentPart, FinishReason, Message, ModelError, ModelEventStream, ModelProfile, ModelRef,
    ModelRequest, ModelResponse, ModelRuntime, Role, TokenUsage, ToolCall,
};
use leveler_tools::ToolContext;

fn registry() -> leveler_tools::ToolRegistry {
    let mut registry = leveler_tools::default_registry();
    leveler_agent::register_harness_controls(&mut registry);
    registry
}

struct Script {
    replies: Mutex<VecDeque<ModelResponse>>,
    tools_seen: Mutex<Vec<Vec<String>>>,
    system_seen: Mutex<Vec<Vec<String>>>,
}

#[async_trait]
impl ModelRuntime for Script {
    async fn generate(
        &self,
        _r: ModelRequest,
        _c: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        unimplemented!()
    }

    async fn stream(
        &self,
        request: ModelRequest,
        _c: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        self.tools_seen
            .lock()
            .unwrap()
            .push(request.tools.iter().map(|t| t.name.clone()).collect());
        self.system_seen.lock().unwrap().push(
            request
                .messages
                .iter()
                .filter(|m| m.role == Role::System)
                .map(|m| m.text_content())
                .collect(),
        );
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| ModelResponse {
                request_id: RequestId::generate(),
                message: Message::text(Role::Assistant, "done"),
                finish_reason: FinishReason::Stop,
                usage: TokenUsage::default(),
            });
        Ok(leveler_model::stream_from_response(reply))
    }

    async fn profile(&self, _m: &ModelRef) -> Result<ModelProfile, ModelError> {
        unimplemented!()
    }
}

fn call(name: &str, args: serde_json::Value) -> ModelResponse {
    ModelResponse {
        request_id: RequestId::generate(),
        message: Message {
            role: Role::Assistant,
            content: vec![ContentPart::ToolCall {
                call: ToolCall {
                    id: ToolCallId::new(format!("c-{name}-{}", RequestId::generate())),
                    name: name.to_string(),
                    arguments: args,
                },
            }],
        },
        finish_reason: FinishReason::ToolCalls,
        usage: TokenUsage::default(),
    }
}

struct Human {
    decision: ApprovalDecision,
    has_human: bool,
    asked: Mutex<Vec<ApprovalRequest>>,
}

impl Human {
    fn new(decision: ApprovalDecision) -> Arc<Self> {
        Arc::new(Self {
            decision,
            has_human: true,
            asked: Mutex::new(Vec::new()),
        })
    }

    fn headless() -> Arc<Self> {
        Arc::new(Self {
            decision: ApprovalDecision::Deny,
            has_human: false,
            asked: Mutex::new(Vec::new()),
        })
    }

    fn asked(&self) -> Vec<ApprovalRequest> {
        self.asked.lock().unwrap().clone()
    }
}

#[async_trait]
impl Approver for Human {
    async fn decide(&self, request: &ApprovalRequest) -> ApprovalDecision {
        self.asked.lock().unwrap().push(request.clone());
        self.decision
    }

    fn has_human(&self) -> bool {
        self.has_human
    }
}

struct Env {
    _tmp: tempfile::TempDir,
    repo: PathBuf,
    home: PathBuf,
    systems: Mutex<Vec<Vec<String>>>,
}

impl Env {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let home = tmp.path().join("leveler-home");
        std::fs::create_dir_all(&repo).unwrap();
        Self {
            _tmp: tmp,
            repo,
            home,
            systems: Mutex::new(Vec::new()),
        }
    }

    fn project_skill(&self, name: &str) -> PathBuf {
        self.repo.join(".leveler/skills").join(name)
    }

    async fn run(
        &self,
        profile: PermissionProfile,
        approver: Arc<Human>,
        replies: Vec<ModelResponse>,
    ) -> (Vec<(String, bool, String)>, Vec<Vec<String>>) {
        let env = Arc::new(leveler_core::EnvSnapshot::new(
            [(
                std::ffi::OsString::from("LEVELER_HOME"),
                self.home.clone().into_os_string(),
            )],
            self.repo.clone(),
            std::env::temp_dir(),
        ));
        let context =
            ToolContext::with_environment(Workspace::new(&self.repo).unwrap(), profile, env);
        let runtime = Arc::new(Script {
            replies: Mutex::new(replies.into()),
            tools_seen: Mutex::new(Vec::new()),
            system_seen: Mutex::new(Vec::new()),
        });
        let mut events = Vec::new();
        Executor::new(
            runtime.clone(),
            Arc::new(registry()),
            context,
            ModelRef::new("mock", "m"),
            10,
        )
        .with_approver(approver)
        .run(
            "save a skill",
            &mut |e| events.push(e),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let results = events
            .into_iter()
            .filter_map(|e| match e {
                AgentEvent::ToolResult {
                    name,
                    is_error,
                    preview,
                    ..
                } => Some((name, is_error, preview)),
                _ => None,
            })
            .collect();
        let tools = runtime.tools_seen.lock().unwrap().clone();
        *self.systems.lock().unwrap() = runtime.system_seen.lock().unwrap().clone();
        (results, tools)
    }
}

fn proposal(scope: &str, action: &str, name: &str) -> serde_json::Value {
    serde_json::json!({
        "scope": scope,
        "action": action,
        "name": name,
        "description": "Diagnose intermittent Windows CI failures.",
        "body": "1. Reproduce the flake.\n2. Bisect the change set.",
        "files": [{ "path": "references/checklist.md", "content": "checklist\n" }],
    })
}

fn result_of<'a>(results: &'a [(String, bool, String)], tool: &str) -> &'a (String, bool, String) {
    results
        .iter()
        .find(|(n, _, _)| n == tool)
        .unwrap_or_else(|| panic!("no {tool} result in {results:?}"))
}

#[tokio::test]
async fn a_confirmed_skill_is_written_and_previewed() {
    let env = Env::new();
    let human = Human::new(ApprovalDecision::ApproveOnce);
    let (results, tools) = env
        .run(
            PermissionProfile::FullAccess,
            human.clone(),
            vec![call(
                "save_skill",
                proposal("project", "create", "windows-ci-debug"),
            )],
        )
        .await;
    for t in ["load_skill", "save_skill", "delete_skill"] {
        assert!(
            tools[0].iter().any(|x| x == t),
            "{t} offered to the main agent: {:?}",
            tools[0]
        );
    }
    let (_, is_error, preview) = result_of(&results, "save_skill");
    assert!(!is_error, "{preview}");
    let asked = human.asked();
    assert_eq!(asked.len(), 1, "a human confirms even under full access");
    let description = &asked[0].description;
    for needle in [
        "Create Skill",
        "windows-ci-debug",
        "Diagnose intermittent Windows CI failures.",
        "project",
        "references/checklist.md",
    ] {
        assert!(
            description.contains(needle),
            "preview lacks `{needle}`: {description}"
        );
    }
    let dir = env.project_skill("windows-ci-debug");
    assert!(dir.join("SKILL.md").is_file());
    assert!(dir.join("references/checklist.md").is_file());
    let content = std::fs::read_to_string(dir.join("SKILL.md")).unwrap();
    assert!(
        content.starts_with("---\nname: windows-ci-debug\n"),
        "{content}"
    );
}

#[tokio::test]
async fn nothing_is_written_without_a_human_yes() {
    for (label, approver) in [
        ("denied", Human::new(ApprovalDecision::Deny)),
        ("headless", Human::headless()),
    ] {
        let env = Env::new();
        let (results, _) = env
            .run(
                PermissionProfile::FullAccess,
                approver,
                vec![call(
                    "save_skill",
                    proposal("project", "create", "windows-ci-debug"),
                )],
            )
            .await;
        let (_, is_error, _) = result_of(&results, "save_skill");
        assert!(*is_error, "{label}");
        assert!(!env.project_skill("windows-ci-debug").exists(), "{label}");
    }
}

#[tokio::test]
async fn an_invalid_proposal_is_refused_before_anyone_is_asked() {
    let env = Env::new();
    let human = Human::new(ApprovalDecision::ApproveOnce);
    let (results, _) = env
        .run(
            PermissionProfile::FullAccess,
            human.clone(),
            vec![call(
                "save_skill",
                proposal("project", "create", "Bad Name"),
            )],
        )
        .await;
    let (_, is_error, preview) = result_of(&results, "save_skill");
    assert!(*is_error, "{preview}");
    assert!(preview.contains("invalid"), "{preview}");
    assert!(human.asked().is_empty(), "the validator catches it first");
}

#[tokio::test]
async fn updating_a_native_skill_replaces_it_after_confirmation() {
    let env = Env::new();
    let human = Human::new(ApprovalDecision::ApproveOnce);
    env.run(
        PermissionProfile::FullAccess,
        human.clone(),
        vec![call(
            "save_skill",
            proposal("project", "create", "windows-ci-debug"),
        )],
    )
    .await;
    let human = Human::new(ApprovalDecision::ApproveOnce);
    let mut update = proposal("project", "update", "windows-ci-debug");
    update["body"] = serde_json::json!("1. Reproduce.\n2. Bisect.\n3. Report.");
    update["files"] = serde_json::json!([]);
    let (results, _) = env
        .run(
            PermissionProfile::FullAccess,
            human.clone(),
            vec![call("save_skill", update)],
        )
        .await;
    let (_, is_error, preview) = result_of(&results, "save_skill");
    assert!(!is_error, "{preview}");
    assert_eq!(human.asked().len(), 1);
    let dir = env.project_skill("windows-ci-debug");
    let content = std::fs::read_to_string(dir.join("SKILL.md")).unwrap();
    assert!(content.contains("3. Report."), "{content}");
    assert!(!dir.join("references/checklist.md").exists());
}

#[tokio::test]
async fn delete_requires_confirmation_and_removes_the_skill() {
    let env = Env::new();
    let human = Human::new(ApprovalDecision::ApproveOnce);
    env.run(
        PermissionProfile::FullAccess,
        human.clone(),
        vec![call(
            "save_skill",
            proposal("project", "create", "windows-ci-debug"),
        )],
    )
    .await;
    let human = Human::new(ApprovalDecision::ApproveOnce);
    let (results, _) = env
        .run(
            PermissionProfile::FullAccess,
            human.clone(),
            vec![call(
                "delete_skill",
                serde_json::json!({"scope": "project", "name": "windows-ci-debug"}),
            )],
        )
        .await;
    let (_, is_error, preview) = result_of(&results, "delete_skill");
    assert!(!is_error, "{preview}");
    assert_eq!(human.asked().len(), 1);
    assert!(!env.project_skill("windows-ci-debug").exists());
}

#[tokio::test]
async fn a_builtin_skill_cannot_be_deleted() {
    let env = Env::new();
    let human = Human::new(ApprovalDecision::ApproveOnce);
    let (results, _) = env
        .run(
            PermissionProfile::FullAccess,
            human.clone(),
            vec![call(
                "delete_skill",
                serde_json::json!({"scope": "project", "name": "skill-creator"}),
            )],
        )
        .await;
    let (_, is_error, preview) = result_of(&results, "delete_skill");
    assert!(*is_error, "{preview}");
    assert!(
        human.asked().is_empty(),
        "the validator refuses a builtin first"
    );
}

#[tokio::test]
async fn the_skill_index_is_injected_metadata_only() {
    let env = Env::new();
    let human = Human::new(ApprovalDecision::ApproveOnce);
    env.run(
        PermissionProfile::FullAccess,
        human,
        vec![call(
            "save_skill",
            proposal("project", "create", "windows-ci-debug"),
        )],
    )
    .await;

    // A trivial turn: nothing but the seeded system messages.
    env.run(
        PermissionProfile::FullAccess,
        Human::new(ApprovalDecision::ApproveOnce),
        Vec::new(),
    )
    .await;
    let systems = env.systems.lock().unwrap().clone();
    let first = systems.first().expect("the model was asked once");
    let joined = first.join("\n");
    // The index reaches the model, so it can load a skill it was not told to.
    assert!(joined.contains("Available skills"), "{joined}");
    assert!(joined.contains("skill-creator"), "{joined}");
    assert!(joined.contains("windows-ci-debug"), "{joined}");
    // Progressive disclosure: the body is not in the index.
    assert!(
        !joined.contains("1. Reproduce the flake."),
        "the full body must not be injected with the index: {joined}"
    );
}
