//! Agent Extensibility — authoring through the conversation.
//!
//! The model proposes a definition with `save_agent`; the validator decides
//! whether it is well-formed, a human decides whether it is written, and the
//! store writes it atomically. Nothing the model says can skip either step,
//! in any permission profile.

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
    batch(vec![(name, args)])
}

/// Several calls in one assistant message: one round. The runtime stops a turn
/// after consecutive rounds in which every call was refused, so refusals that
/// belong together are issued together.
fn batch(calls: Vec<(&str, serde_json::Value)>) -> ModelResponse {
    ModelResponse {
        request_id: RequestId::generate(),
        message: Message {
            role: Role::Assistant,
            content: calls
                .into_iter()
                .map(|(name, arguments)| ContentPart::ToolCall {
                    call: ToolCall {
                        id: ToolCallId::new(format!("c-{name}-{}", RequestId::generate())),
                        name: name.to_string(),
                        arguments,
                    },
                })
                .collect(),
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
        }
    }

    fn project_agent(&self, name: &str) -> PathBuf {
        self.repo.join(".leveler/agents").join(name)
    }

    fn user_agent(&self, name: &str) -> PathBuf {
        self.home.join("agents").join(name)
    }

    /// Run a scripted conversation; returns the tool results and the tools the
    /// model was offered on its first request.
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
            "make an agent",
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
        (results, tools)
    }
}

fn reviewer_proposal(scope: &str) -> serde_json::Value {
    serde_json::json!({
        "scope": scope,
        "action": "create",
        "name": "security-reviewer",
        "description": "Reviews Rust changes for exploitable security issues.",
        "capability": "read_only",
        "tools": ["read_file", "grep", "git_diff"],
        "reasoning_effort": "high",
        "instructions": "Report only high-confidence, exploitable issues with file:line.\n"
    })
}

fn result_of<'a>(results: &'a [(String, bool, String)], tool: &str) -> &'a (String, bool, String) {
    results
        .iter()
        .find(|(n, _, _)| n == tool)
        .unwrap_or_else(|| panic!("no {tool} result in {results:?}"))
}

#[tokio::test]
async fn a_confirmed_proposal_is_written_validated_and_previewed() {
    let env = Env::new();
    let human = Human::new(ApprovalDecision::ApproveOnce);
    let (results, tools) = env
        .run(
            PermissionProfile::FullAccess,
            human.clone(),
            vec![call("save_agent", reviewer_proposal("project"))],
        )
        .await;
    for t in ["list_agents", "save_agent", "delete_agent"] {
        assert!(
            tools[0].iter().any(|x| x == t),
            "{t} offered to the main agent: {:?}",
            tools[0]
        );
    }
    let (_, is_error, preview) = result_of(&results, "save_agent");
    assert!(!is_error, "{preview}");

    let asked = human.asked();
    assert_eq!(asked.len(), 1, "a human confirms even under full access");
    let d = &asked[0].description;
    for needle in [
        "security-reviewer",
        "project",
        "read_only",
        "no writes",
        "read_file, grep, git_diff",
        "high",
        "Report only high-confidence",
    ] {
        assert!(d.contains(needle), "preview lacks `{needle}`: {d}");
    }

    let dir = env.project_agent("security-reviewer");
    let yaml = std::fs::read_to_string(dir.join("agent.yaml")).unwrap();
    assert!(
        yaml.starts_with("version: 1\nname: security-reviewer\n"),
        "{yaml}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("instructions.md")).unwrap(),
        "Report only high-confidence, exploitable issues with file:line.\n"
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
                vec![call("save_agent", reviewer_proposal("project"))],
            )
            .await;
        let (_, is_error, _) = result_of(&results, "save_agent");
        assert!(*is_error, "{label}");
        assert!(!env.project_agent("security-reviewer").exists(), "{label}");
    }
}

#[tokio::test]
async fn an_inconsistent_proposal_is_refused_before_anyone_is_asked() {
    let env = Env::new();
    let human = Human::new(ApprovalDecision::ApproveOnce);
    let mut proposal = reviewer_proposal("project");
    // "read-only reviewer" that asks for a write tool and a write scope.
    proposal["tools"] = serde_json::json!(["read_file", "apply_patch"]);
    let mut roots = reviewer_proposal("project");
    roots["name"] = serde_json::json!("roots-reviewer");
    roots["write_roots"] = serde_json::json!(["src"]);
    let mut missing_skill = reviewer_proposal("project");
    missing_skill["name"] = serde_json::json!("skilled");
    missing_skill["skills"] = serde_json::json!(["no-such-skill"]);
    let (results, _) = env
        .run(
            PermissionProfile::FullAccess,
            human.clone(),
            vec![batch(vec![
                ("save_agent", proposal),
                ("save_agent", roots),
                ("save_agent", missing_skill),
            ])],
        )
        .await;
    let errors: Vec<_> = results
        .iter()
        .filter(|(n, _, _)| n == "save_agent")
        .collect();
    assert_eq!(errors.len(), 3);
    assert!(errors.iter().all(|(_, e, _)| *e), "{errors:?}");
    assert!(errors[0].2.contains("apply_patch"), "{}", errors[0].2);
    assert!(errors[1].2.contains("read_only"), "{}", errors[1].2);
    assert!(
        errors[2].2.contains("no-such-skill") && errors[2].2.contains("create the skill"),
        "{}",
        errors[2].2
    );
    assert!(
        human.asked().is_empty(),
        "the validator, not the human, catches contradictions"
    );
    assert!(!env.repo.join(".leveler/agents").exists());
}

#[tokio::test]
async fn a_session_approval_does_not_carry_over_to_a_different_proposal() {
    let env = Env::new();
    let human = Human::new(ApprovalDecision::ApproveSession);
    let mut second = reviewer_proposal("user");
    second["name"] = serde_json::json!("global-writer");
    second["capability"] = serde_json::json!("writer");
    second["tools"] = serde_json::json!(["read_file", "apply_patch", "run_command"]);
    env.run(
        PermissionProfile::FullAccess,
        human.clone(),
        vec![
            call("save_agent", reviewer_proposal("project")),
            call("save_agent", second),
        ],
    )
    .await;
    assert_eq!(
        human.asked().len(),
        2,
        "each distinct proposal is confirmed"
    );
    assert!(env.user_agent("global-writer").join("agent.yaml").is_file());
    assert!(human.asked()[1].description.contains("user"));
}

#[tokio::test]
async fn built_ins_are_copied_not_edited_and_list_agents_shows_their_definition() {
    let env = Env::new();
    let human = Human::new(ApprovalDecision::ApproveOnce);
    let mut edit_builtin = reviewer_proposal("project");
    edit_builtin["action"] = serde_json::json!("update");
    edit_builtin["name"] = serde_json::json!("code-reviewer");
    let mut reserved = reviewer_proposal("project");
    reserved["name"] = serde_json::json!("worker");
    let (results, _) = env
        .run(
            PermissionProfile::FullAccess,
            human.clone(),
            vec![
                call("list_agents", serde_json::json!({"name": "code-reviewer"})),
                batch(vec![("save_agent", edit_builtin), ("save_agent", reserved)]),
                call("list_agents", serde_json::json!({})),
            ],
        )
        .await;
    let lists: Vec<_> = results
        .iter()
        .filter(|(n, _, _)| n == "list_agents")
        .collect();
    assert!(
        lists[0].2.contains("capability: read_only") && lists[0].2.contains("builtin"),
        "{}",
        lists[0].2
    );
    let saves: Vec<_> = results
        .iter()
        .filter(|(n, _, _)| n == "save_agent")
        .collect();
    assert!(
        saves[0].1 && saves[0].2.contains("built-in"),
        "{}",
        saves[0].2
    );
    assert!(
        saves[1].1 && saves[1].2.contains("built-in"),
        "{}",
        saves[1].2
    );
    assert!(
        lists[1].2.contains("code-explorer") && lists[1].2.contains("explorer"),
        "{}",
        lists[1].2
    );
    assert!(human.asked().is_empty());
}

#[tokio::test]
async fn delete_needs_confirmation_and_then_removes_the_definition() {
    let env = Env::new();
    let human = Human::new(ApprovalDecision::ApproveOnce);
    env.run(
        PermissionProfile::FullAccess,
        human.clone(),
        vec![
            call("save_agent", reviewer_proposal("project")),
            call(
                "delete_agent",
                serde_json::json!({"scope": "project", "name": "security-reviewer"}),
            ),
        ],
    )
    .await;
    assert_eq!(human.asked().len(), 2);
    assert!(human.asked()[1].description.contains("security-reviewer"));
    assert!(!env.project_agent("security-reviewer").exists());
}

#[tokio::test]
async fn delegated_children_are_never_offered_the_authoring_tools() {
    let env = Env::new();
    let human = Human::new(ApprovalDecision::ApproveOnce);
    let (_, tools) = env
        .run(
            PermissionProfile::FullAccess,
            human,
            vec![call(
                "spawn_agent",
                serde_json::json!({"task": "look around", "run_in_background": false}),
            )],
        )
        .await;
    let child: Vec<&Vec<String>> = tools
        .iter()
        .filter(|t| t.iter().any(|n| n == "report_finding"))
        .collect();
    assert!(!child.is_empty(), "the child ran");
    for t in child {
        for authoring in [
            "list_agents",
            "save_agent",
            "delete_agent",
            "save_skill",
            "delete_skill",
        ] {
            assert!(
                !t.iter().any(|n| n == authoring),
                "{authoring} reached a child: {t:?}"
            );
        }
    }
}
