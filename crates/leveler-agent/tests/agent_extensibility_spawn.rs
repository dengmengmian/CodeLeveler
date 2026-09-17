//! Agent Extensibility — runtime integration of declarative agents.
//!
//! A `.leveler/agents/<name>/{agent.yaml,instructions.md}` definition spawns
//! through `spawn_agent(agent=...)` under an existing capability class. Every
//! bound it declares is enforced by the runtime — toolset, write scope, model,
//! reasoning effort, rounds — and nothing it or its caller writes can widen the
//! class. The child runs on the definition as resolved at spawn.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use leveler_agent::agent_registry::AgentRoots;
use leveler_agent::{AgentEvent, Executor};
use leveler_core::{RequestId, ToolCallId};
use leveler_execution::{PermissionProfile, Workspace};
use leveler_lifecycle::ChildSpawnSpec;
use leveler_model::{
    ContentPart, FinishReason, Message, ModelError, ModelEventStream, ModelProfile, ModelRef,
    ModelRequest, ModelResponse, ModelRuntime, ReasoningEffort, Role, TokenUsage, ToolCall,
};
use leveler_tools::ToolContext;

const CHILD: &str = "CHILD_TASK_7Q";

fn default_registry() -> leveler_tools::ToolRegistry {
    let mut registry = leveler_tools::default_registry();
    leveler_agent::register_harness_controls(&mut registry);
    registry
}

#[derive(Clone, Debug)]
struct Seen {
    child: bool,
    blob: String,
    tools: Vec<String>,
    effort: Option<ReasoningEffort>,
    model: ModelRef,
}

type Hook = Box<dyn Fn(usize) + Send + Sync>;

/// Parent and child scripts routed by the child marker; every request is
/// recorded. `on_child_request(n)` runs before the n-th child request (0-based).
struct Rt {
    parent: Mutex<VecDeque<ModelResponse>>,
    child: Mutex<VecDeque<ModelResponse>>,
    seen: Mutex<Vec<Seen>>,
    on_child_request: Option<Hook>,
}

impl Rt {
    fn new(parent: Vec<ModelResponse>, child: Vec<ModelResponse>) -> Self {
        Self {
            parent: Mutex::new(parent.into()),
            child: Mutex::new(child.into()),
            seen: Mutex::new(Vec::new()),
            on_child_request: None,
        }
    }
}

#[async_trait]
impl ModelRuntime for Rt {
    async fn generate(
        &self,
        _request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        unimplemented!()
    }

    async fn stream(
        &self,
        request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        let blob: String = request
            .messages
            .iter()
            .map(|m| m.text_content())
            .collect::<Vec<_>>()
            .join("\n");
        let child = blob.contains(CHILD);
        let n_child = {
            let seen = self.seen.lock().unwrap();
            seen.iter().filter(|s| s.child).count()
        };
        if child && let Some(hook) = &self.on_child_request {
            hook(n_child);
        }
        self.seen.lock().unwrap().push(Seen {
            child,
            blob,
            tools: request.tools.iter().map(|t| t.name.clone()).collect(),
            effort: request.reasoning_effort,
            model: request.model.clone(),
        });
        let response = if child {
            self.child.lock().unwrap().pop_front()
        } else {
            self.parent.lock().unwrap().pop_front()
        }
        .unwrap_or_else(|| text(if child { "child done" } else { "parent done" }));
        Ok(leveler_model::stream_from_response(response))
    }

    async fn profile(&self, model: &ModelRef) -> Result<ModelProfile, ModelError> {
        if model.model == "missing" {
            return Err(ModelError::new(
                leveler_model::ModelErrorKind::InvalidRequest,
                format!("model `{model}` is not configured"),
            ));
        }
        Ok(serde_json::from_value(serde_json::json!({
            "id": model.model, "provider": model.provider, "model_id": model.model,
            "protocol": "openai_chat",
            "capabilities": {
                "streaming": true, "tool_calling": true, "parallel_tool_calls": true,
                "structured_output": false, "reasoning": true, "vision": false
            },
            "limits": {
                "context_window": 128000, "reliable_context": 64000,
                "max_output_tokens": 4096, "max_tool_schema_bytes": 65536,
                "max_parallel_tool_calls": 4
            },
            "reasoning": { "supported_efforts": ["low", "high"], "default_effort": "low" }
        }))
        .unwrap())
    }
}

fn text(t: &str) -> ModelResponse {
    ModelResponse {
        request_id: RequestId::generate(),
        message: Message::text(Role::Assistant, t),
        finish_reason: FinishReason::Stop,
        usage: TokenUsage::default(),
    }
}

fn calls(calls: Vec<(&str, &str, serde_json::Value)>) -> ModelResponse {
    ModelResponse {
        request_id: RequestId::generate(),
        message: Message {
            role: Role::Assistant,
            content: calls
                .into_iter()
                .map(|(id, name, arguments)| ContentPart::ToolCall {
                    call: ToolCall {
                        id: ToolCallId::new(id),
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

fn spawn(args: serde_json::Value) -> ModelResponse {
    let mut args = args;
    if args.get("run_in_background").is_none() {
        args["run_in_background"] = serde_json::Value::Bool(false);
    }
    calls(vec![("s1", "spawn_agent", args)])
}

struct Repo {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    user: PathBuf,
}

impl Repo {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        let user = tmp.path().join("home-agents");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&user).unwrap();
        Self {
            _tmp: tmp,
            root,
            user,
        }
    }

    fn agent(&self, name: &str, yaml_body: &str, instructions: &str) -> PathBuf {
        write_agent(
            &self.root.join(".leveler/agents"),
            name,
            yaml_body,
            instructions,
        )
    }

    fn file(&self, rel: &str, content: &str) {
        let p = self.root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    fn read(&self, rel: &str) -> Option<String> {
        std::fs::read_to_string(self.root.join(rel)).ok()
    }
}

fn write_agent(dir: &Path, name: &str, yaml_body: &str, instructions: &str) -> PathBuf {
    let d = dir.join(name);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(
        d.join("agent.yaml"),
        format!("version: 1\nname: {name}\ndescription: The {name} agent.\n{yaml_body}"),
    )
    .unwrap();
    std::fs::write(d.join("instructions.md"), instructions).unwrap();
    d
}

struct Run {
    events: Vec<AgentEvent>,
    seen: Vec<Seen>,
    records: Vec<leveler_agent::ModelRequestRecord>,
}

/// Keeps the model-request records the run hands its durable sink.
#[derive(Default)]
struct Records(Vec<leveler_agent::ModelRequestRecord>);

#[async_trait]
impl leveler_agent::TranscriptSink for Records {
    async fn append(&mut self, _messages: &[Message]) -> Result<(), leveler_engine::PortError> {
        Ok(())
    }

    async fn record_model_request(
        &mut self,
        record: &leveler_agent::ModelRequestRecord,
    ) -> Result<(), leveler_engine::PortError> {
        self.0.push(record.clone());
        Ok(())
    }
}

impl Run {
    fn started(
        &self,
    ) -> Option<(
        String,
        Option<String>,
        Option<String>,
        bool,
        ChildSpawnSpec,
        String,
    )> {
        self.events.iter().find_map(|e| match e {
            AgentEvent::SubAgentStarted {
                role,
                profile_id,
                profile_role,
                read_only,
                spec,
                task,
                ..
            } => Some((
                role.clone(),
                profile_id.clone(),
                profile_role.clone(),
                *read_only,
                spec.clone().unwrap_or_default(),
                task.clone(),
            )),
            _ => None,
        })
    }

    fn spawn_error(&self) -> String {
        self.events
            .iter()
            .find_map(|e| match e {
                AgentEvent::ToolResult {
                    name,
                    is_error: true,
                    preview,
                    ..
                } if name == "spawn_agent" => Some(preview.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no spawn_agent error in {:#?}", self.events))
    }

    fn refused(&self) -> String {
        assert!(
            self.started().is_none(),
            "a refused spawn must not start a child"
        );
        self.spawn_error()
    }

    fn child_requests(&self) -> Vec<&Seen> {
        self.seen.iter().filter(|s| s.child).collect()
    }

    fn parent_requests(&self) -> Vec<&Seen> {
        self.seen.iter().filter(|s| !s.child).collect()
    }
}

async fn run(repo: &Repo, rt: Rt) -> Run {
    let workspace = Workspace::new(&repo.root).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::FullAccess);
    let rt = Arc::new(rt);
    let mut events = Vec::new();
    let mut sink = Records::default();
    Executor::new(
        rt.clone(),
        Arc::new(default_registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .with_agent_roots(AgentRoots {
        project_root: Some(repo.root.clone()),
        user_agents_dir: Some(repo.user.clone()),
    })
    .run(
        "delegate",
        &mut |e| events.push(e),
        &mut sink,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let seen = rt.seen.lock().unwrap().clone();
    Run {
        events,
        seen,
        records: sink.0,
    }
}

async fn spawn_once(repo: &Repo, args: serde_json::Value, child: Vec<ModelResponse>) -> Run {
    run(repo, Rt::new(vec![spawn(args)], child)).await
}

const MUTATING: &[&str] = &[
    "apply_patch",
    "write_file",
    "run_command",
    "shell_command",
    "claim_write_scope",
];

// ── Spawn under a declared identity ─────────────────────────────────────────

#[tokio::test]
async fn a_custom_read_only_agent_spawns_under_its_own_name_with_its_instructions() {
    let repo = Repo::new();
    repo.agent(
        "security-reviewer",
        "capability: read_only\n",
        "INSTR_MARKER_SR1 Report only exploitable issues.",
    );
    let r = spawn_once(
        &repo,
        serde_json::json!({"agent": "security-reviewer", "task": format!("{CHILD} review auth")}),
        vec![],
    )
    .await;
    let (role, profile_id, profile_role, read_only, spec, task) =
        r.started().expect("the custom agent starts");
    assert_eq!(role, "explorer");
    assert_eq!(profile_id.as_deref(), Some("security-reviewer"));
    assert_eq!(profile_role.as_deref(), Some("explorer"));
    assert!(read_only);
    assert!(
        !task.contains("INSTR_MARKER_SR1"),
        "instructions are not the task: {task}"
    );

    let snapshot = spec
        .agent
        .expect("the spawn records the resolved definition");
    assert_eq!(snapshot.name, "security-reviewer");
    assert_eq!(snapshot.source, "project");
    assert_eq!(snapshot.capability, "read_only");
    assert!(snapshot.fingerprint.starts_with("sha256:"));

    let child = r.child_requests();
    assert!(!child.is_empty());
    assert_eq!(
        child[0].blob.matches("INSTR_MARKER_SR1").count(),
        1,
        "the child's initial context carries its instructions exactly once"
    );
    for forbidden in MUTATING {
        assert!(
            !child[0].tools.iter().any(|t| t == forbidden),
            "a read_only agent must not be offered {forbidden}: {:?}",
            child[0].tools
        );
    }
    for parent in r.parent_requests() {
        assert!(
            !parent.blob.contains("INSTR_MARKER_SR1"),
            "full instructions never enter the parent"
        );
    }
}

#[tokio::test]
async fn a_read_only_agent_cannot_write_even_if_it_tries() {
    let repo = Repo::new();
    repo.file("src/lib.rs", "pub fn old() {}\n");
    repo.agent("ro", "capability: read_only\n", "Look only.");
    let r = spawn_once(
        &repo,
        serde_json::json!({"agent": "ro", "task": CHILD}),
        vec![calls(vec![(
            "w1",
            "write_file",
            serde_json::json!({"path": "src/lib.rs", "content": "pwned"}),
        )])],
    )
    .await;
    assert!(r.started().is_some());
    assert_eq!(
        repo.read("src/lib.rs").as_deref(),
        Some("pub fn old() {}\n")
    );
}

#[tokio::test]
async fn a_call_role_or_profile_cannot_change_the_agents_class() {
    let repo = Repo::new();
    repo.agent("ro", "capability: read_only\n", "Look only.");
    for args in [
        serde_json::json!({"agent": "ro", "role": "default", "task": CHILD}),
        serde_json::json!({"agent": "ro", "profile": "default", "task": CHILD}),
        serde_json::json!({"agent": "ro", "profile": "worker", "files": ["a.rs"], "task": CHILD}),
    ] {
        let r = spawn_once(&repo, args.clone(), vec![]).await;
        let err = r.refused();
        assert!(
            err.contains("ro") && err.contains("read_only"),
            "{args}: {err}"
        );
    }
    // Repeating the agent's own class is fine.
    let r = spawn_once(
        &repo,
        serde_json::json!({"agent": "ro", "role": "explorer", "task": CHILD}),
        vec![],
    )
    .await;
    assert!(r.started().is_some());
}

#[tokio::test]
async fn a_read_only_agent_refuses_a_files_scope() {
    let repo = Repo::new();
    repo.agent("ro", "capability: read_only\n", "Look only.");
    let r = spawn_once(
        &repo,
        serde_json::json!({"agent": "ro", "files": ["src/lib.rs"], "task": CHILD}),
        vec![],
    )
    .await;
    assert!(r.refused().contains("read-only"));
}

#[tokio::test]
async fn an_unknown_invalid_or_harness_only_agent_is_refused_never_defaulted() {
    let repo = Repo::new();
    repo.agent("security-reviewer", "capability: read_only\n", "x");
    let r = spawn_once(
        &repo,
        serde_json::json!({"agent": "security", "task": CHILD}),
        vec![],
    )
    .await;
    let err = r.refused();
    assert!(err.contains("Agent \"security\" not found."), "{err}");
    assert!(err.contains("security-reviewer"), "{err}");

    // A broken project override of a built-in persona does not fall back to it.
    repo.agent("code-reviewer", "capability: read_only\nwirte: true\n", "x");
    let r = spawn_once(
        &repo,
        serde_json::json!({"agent": "code-reviewer", "task": CHILD}),
        vec![],
    )
    .await;
    let err = r.refused();
    assert!(err.contains("invalid") && err.contains("wirte"), "{err}");

    let r = spawn_once(
        &repo,
        serde_json::json!({"agent": "reviewer", "task": CHILD}),
        vec![],
    )
    .await;
    assert!(r.refused().contains("harness"));
}

#[tokio::test]
async fn a_user_agent_is_found_and_a_project_agent_of_the_same_name_wins() {
    let repo = Repo::new();
    write_agent(
        &repo.user,
        "rust-explorer",
        "capability: read_only\n",
        "USER_RUST_MARKER",
    );
    let r = spawn_once(
        &repo,
        serde_json::json!({"agent": "rust-explorer", "task": CHILD}),
        vec![],
    )
    .await;
    assert_eq!(r.started().unwrap().4.agent.unwrap().source, "user");
    assert!(r.child_requests()[0].blob.contains("USER_RUST_MARKER"));

    repo.agent(
        "rust-explorer",
        "capability: read_only\n",
        "PROJECT_RUST_MARKER",
    );
    let r = spawn_once(
        &repo,
        serde_json::json!({"agent": "rust-explorer", "task": CHILD}),
        vec![],
    )
    .await;
    assert_eq!(r.started().unwrap().4.agent.unwrap().source, "project");
    let blob = &r.child_requests()[0].blob;
    assert!(blob.contains("PROJECT_RUST_MARKER") && !blob.contains("USER_RUST_MARKER"));
}

// ── Declared bounds are enforced ────────────────────────────────────────────

#[tokio::test]
async fn declared_tools_narrow_the_toolset_and_keep_harness_controls() {
    let repo = Repo::new();
    repo.agent(
        "narrow",
        "capability: read_only\ntools: [read_file, grep]\n",
        "x",
    );
    let r = spawn_once(
        &repo,
        serde_json::json!({"agent": "narrow", "task": CHILD}),
        vec![],
    )
    .await;
    let tools = &r.child_requests()[0].tools;
    for present in ["read_file", "grep", "update_plan", "report_finding"] {
        assert!(
            tools.iter().any(|t| t == present),
            "{present} missing: {tools:?}"
        );
    }
    for absent in ["list_files", "find_files", "git_diff"] {
        assert!(
            !tools.iter().any(|t| t == absent),
            "{absent} not declared: {tools:?}"
        );
    }
    assert_eq!(r.started().unwrap().4.tools, vec!["read_file", "grep"]);
}

#[tokio::test]
async fn a_declared_tool_this_session_lacks_makes_the_agent_unavailable() {
    let repo = Repo::new();
    // A structurally valid writer whose declared web_search this session
    // does not compose (no search key).
    repo.agent(
        "searcher",
        "capability: writer\ntools: [read_file, web_search]\n",
        "x",
    );
    let r = spawn_once(
        &repo,
        serde_json::json!({"agent": "searcher", "task": CHILD}),
        vec![],
    )
    .await;
    let err = r.refused();
    assert!(
        err.contains("web_search") && err.contains("not available"),
        "{err}"
    );
}

#[tokio::test]
async fn a_scoped_writer_spawn_scope_must_lie_inside_the_definitions_write_roots() {
    let repo = Repo::new();
    repo.file("src/lib.rs", "rust\n");
    repo.file("web/app.ts", "ts\n");
    repo.agent(
        "frontend-worker",
        "capability: scoped_writer\nworkspace:\n  write_roots: [web]\n",
        "Frontend only.",
    );
    let r = spawn_once(
        &repo,
        serde_json::json!({"agent": "frontend-worker", "files": ["web/app.ts", "src/lib.rs"], "task": CHILD}),
        vec![],
    )
    .await;
    let err = r.refused();
    assert!(err.contains("src/lib.rs") && err.contains("web"), "{err}");

    let r = spawn_once(
        &repo,
        serde_json::json!({"agent": "frontend-worker", "files": ["web/app.ts"], "task": CHILD}),
        vec![
            calls(vec![(
                "w1",
                "write_file",
                serde_json::json!({"path": "web/app.ts", "content": "new ts\n"}),
            )]),
            calls(vec![(
                "w2",
                "write_file",
                serde_json::json!({"path": "src/lib.rs", "content": "pwned\n"}),
            )]),
        ],
    )
    .await;
    let (role, _, _, read_only, spec, _) = r.started().expect("an in-bounds scope is admitted");
    assert_eq!(role, "worker");
    assert!(!read_only);
    assert_eq!(spec.agent.unwrap().write_roots, vec!["web"]);
    assert_eq!(repo.read("web/app.ts").as_deref(), Some("new ts\n"));
    assert_eq!(
        repo.read("src/lib.rs").as_deref(),
        Some("rust\n"),
        "outside the scope"
    );
}

#[tokio::test]
async fn a_writer_cannot_claim_outside_its_write_roots() {
    let repo = Repo::new();
    repo.file("src/lib.rs", "rust\n");
    repo.agent(
        "web-writer",
        "capability: writer\nworkspace:\n  write_roots: [web]\n",
        "Frontend only.",
    );
    let r = spawn_once(
        &repo,
        serde_json::json!({"agent": "web-writer", "task": CHILD}),
        vec![
            calls(vec![(
                "c1",
                "claim_write_scope",
                serde_json::json!({"paths": ["src/lib.rs"]}),
            )]),
            calls(vec![(
                "w1",
                "write_file",
                serde_json::json!({"path": "src/lib.rs", "content": "pwned\n"}),
            )]),
            calls(vec![(
                "c2",
                "claim_write_scope",
                serde_json::json!({"paths": ["web/new.ts"]}),
            )]),
            calls(vec![(
                "w2",
                "write_file",
                serde_json::json!({"path": "web/new.ts", "content": "ok\n"}),
            )]),
        ],
    )
    .await;
    assert!(r.started().is_some());
    assert_eq!(repo.read("src/lib.rs").as_deref(), Some("rust\n"));
    assert_eq!(repo.read("web/new.ts").as_deref(), Some("ok\n"));
    let refusal = r.events.iter().find_map(|e| match e {
        AgentEvent::SubAgentActivity {
            tool,
            phase,
            preview,
            ..
        } if tool == "claim_write_scope"
            && phase == "tool_finished"
            && preview.contains("not granted") =>
        {
            Some(preview.clone())
        }
        _ => None,
    });
    let refusal = refusal.expect("the child is told why its claim was refused");
    assert!(
        refusal.contains("outside this agent's write roots"),
        "{refusal}"
    );
}

#[tokio::test]
async fn bound_skills_reach_the_child_and_nothing_else_does() {
    let repo = Repo::new();
    repo.file(
        ".leveler/skills/sec-audit/SKILL.md",
        "---\nname: sec-audit\ndescription: audit\n---\nSKILL_BODY_MARKER_Z9\n",
    );
    repo.file(
        ".leveler/skills/unrelated/SKILL.md",
        "---\nname: unrelated\ndescription: other\n---\nUNBOUND_SKILL_MARKER\n",
    );
    repo.agent(
        "auditor",
        "capability: read_only\nskills: [sec-audit]\n",
        "x",
    );
    let r = spawn_once(
        &repo,
        serde_json::json!({"agent": "auditor", "task": CHILD}),
        vec![],
    )
    .await;
    let blob = &r.child_requests()[0].blob;
    assert_eq!(blob.matches("SKILL_BODY_MARKER_Z9").count(), 1, "{blob}");
    assert!(!blob.contains("UNBOUND_SKILL_MARKER"));
    assert_eq!(
        r.started().unwrap().4.agent.unwrap().skills,
        vec!["sec-audit"]
    );
    for parent in r.parent_requests() {
        assert!(!parent.blob.contains("SKILL_BODY_MARKER_Z9"));
    }

    repo.agent(
        "broken-auditor",
        "capability: read_only\nskills: [no-such-skill]\n",
        "x",
    );
    let r = spawn_once(
        &repo,
        serde_json::json!({"agent": "broken-auditor", "task": CHILD}),
        vec![],
    )
    .await;
    assert!(r.refused().contains("no-such-skill"));
}

#[tokio::test]
async fn the_declared_model_and_reasoning_effort_are_what_the_child_requests() {
    let repo = Repo::new();
    repo.agent(
        "deep",
        "capability: read_only\nmodel: mock/cheap\nreasoning_effort: high\n",
        "x",
    );
    let r = spawn_once(
        &repo,
        serde_json::json!({"agent": "deep", "task": CHILD}),
        vec![],
    )
    .await;
    let child = r.child_requests()[0].clone();
    assert_eq!(child.model, ModelRef::new("mock", "cheap"));
    assert_eq!(child.effort, Some(ReasoningEffort::High));
    let parent = r.parent_requests()[0].clone();
    assert_eq!(parent.model, ModelRef::new("mock", "m"));
    let child_records: Vec<Option<String>> = r
        .records
        .iter()
        .filter(|record| record.agent_id.is_some())
        .map(|record| record.reasoning_effort.clone())
        .collect();
    assert!(!child_records.is_empty());
    assert!(
        child_records.iter().all(|e| e.as_deref() == Some("high")),
        "the durable record carries the effort actually requested: {child_records:?}"
    );
    let spec = r.started().unwrap().4;
    assert_eq!(spec.model.as_deref(), Some("mock/cheap"));
    assert_eq!(
        spec.agent.unwrap().reasoning_effort.as_deref(),
        Some("high")
    );

    // No silent substitution: an unconfigured model or an effort the model
    // does not offer exactly makes the agent unavailable.
    repo.agent(
        "nomodel",
        "capability: read_only\nmodel: mock/missing\n",
        "x",
    );
    let r = spawn_once(
        &repo,
        serde_json::json!({"agent": "nomodel", "task": CHILD}),
        vec![],
    )
    .await;
    assert!(r.refused().contains("mock/missing"));

    repo.agent(
        "tiny-effort",
        "capability: read_only\nreasoning_effort: minimal\n",
        "x",
    );
    let r = spawn_once(
        &repo,
        serde_json::json!({"agent": "tiny-effort", "task": CHILD}),
        vec![],
    )
    .await;
    let err = r.refused();
    assert!(err.contains("minimal"), "{err}");
}

#[tokio::test]
async fn the_declared_round_budget_bounds_the_child() {
    let repo = Repo::new();
    repo.file("a.txt", "a\n");
    repo.agent(
        "brief",
        "capability: read_only\nbudget:\n  max_rounds: 2\n",
        "x",
    );
    let loops: Vec<ModelResponse> = (0..6)
        .map(|i| {
            calls(vec![(
                Box::leak(format!("r{i}").into_boxed_str()),
                "read_file",
                serde_json::json!({"path": "a.txt"}),
            )])
        })
        .collect();
    let r = spawn_once(
        &repo,
        serde_json::json!({"agent": "brief", "task": CHILD}),
        loops,
    )
    .await;
    assert!(
        r.child_requests().len() <= 2,
        "{} child requests",
        r.child_requests().len()
    );
    assert_eq!(r.started().unwrap().4.max_rounds, 2);
}

// ── The running child keeps its spawn-time definition ───────────────────────

#[tokio::test]
async fn editing_the_definition_mid_run_does_not_change_the_running_child() {
    let repo = Repo::new();
    repo.file("src/lib.rs", "rust\n");
    let dir = repo.agent("mutable", "capability: read_only\n", "V1_INSTRUCTIONS");
    let edit_dir = dir.clone();
    let mut rt = Rt::new(
        vec![spawn(
            serde_json::json!({"agent": "mutable", "task": CHILD}),
        )],
        vec![
            calls(vec![(
                "r1",
                "read_file",
                serde_json::json!({"path": "src/lib.rs"}),
            )]),
            calls(vec![(
                "w1",
                "write_file",
                serde_json::json!({"path": "src/lib.rs", "content": "pwned\n"}),
            )]),
        ],
    );
    rt.on_child_request = Some(Box::new(move |n| {
        if n == 1 {
            std::fs::write(
                edit_dir.join("agent.yaml"),
                "version: 1\nname: mutable\ndescription: now writes\ncapability: writer\n",
            )
            .unwrap();
            std::fs::write(edit_dir.join("instructions.md"), "V2_INSTRUCTIONS").unwrap();
        }
    }));
    let r = run(&repo, rt).await;
    let child = r.child_requests();
    assert!(child.len() >= 2);
    for req in &child {
        assert!(req.blob.contains("V1_INSTRUCTIONS") && !req.blob.contains("V2_INSTRUCTIONS"));
        assert!(
            !req.tools.iter().any(|t| t == "write_file"),
            "{:?}",
            req.tools
        );
    }
    assert_eq!(repo.read("src/lib.rs").as_deref(), Some("rust\n"));

    // A new spawn sees the edit.
    let r = spawn_once(
        &repo,
        serde_json::json!({"agent": "mutable", "task": CHILD}),
        vec![],
    )
    .await;
    assert_eq!(r.started().unwrap().4.agent.unwrap().capability, "writer");
    assert!(r.child_requests()[0].blob.contains("V2_INSTRUCTIONS"));
}

// ── The parent's catalog is bounded ─────────────────────────────────────────

#[tokio::test]
async fn the_parent_sees_a_bounded_catalog_of_names_and_descriptions_only() {
    let repo = Repo::new();
    repo.agent(
        "security-reviewer",
        "capability: read_only\n",
        "FULL_INSTRUCTIONS_NOT_FOR_PARENT",
    );
    let r = run(&repo, Rt::new(vec![text("parent done")], vec![])).await;
    let parent = &r.parent_requests()[0].blob;
    assert!(
        parent.contains("security-reviewer — The security-reviewer agent. (read_only, project)"),
        "{parent}"
    );
    assert!(
        parent.contains("code-explorer"),
        "built-in personas are listed"
    );
    assert!(!parent.contains("FULL_INSTRUCTIONS_NOT_FOR_PARENT"));
}

#[tokio::test]
async fn a_large_registry_does_not_flood_the_parent_context() {
    let repo = Repo::new();
    for i in 0..300 {
        repo.agent(&format!("agent-{i:03}"), "capability: read_only\n", "x");
    }
    let r = run(&repo, Rt::new(vec![text("parent done")], vec![])).await;
    let parent = &r.parent_requests()[0].blob;
    let start = parent.find("## Available agents").expect("catalog present");
    let more = parent[start..]
        .find("more agents not listed")
        .expect("the overflow is announced");
    let end = start + more + parent[start + more..].find('\n').unwrap();
    let catalog = &parent[start..end];
    assert!(catalog.len() <= 4096, "catalog is {} bytes", catalog.len());
    assert!(catalog.contains("more"), "{catalog}");
    assert!(
        catalog.contains("list_agents"),
        "the overflow says how to see the rest: {catalog}"
    );
}
