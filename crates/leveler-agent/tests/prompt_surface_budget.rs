//! Fixed prompt-surface measurement.
//!
//! Numbers come from one production turn: `Executor::run` assembles control,
//! tools and the request projection. The estimator is
//! [`leveler_model::TokenEstimate`]. This file does not keep a second counter.
//!
//! The workspace is this repository. User-level skills and agents are pointed
//! at an empty home so the figure is the repo's own surface. Search, browser
//! and image tools stay off: this pin is a non-vision model with no search key
//! and no browser handle. Production adds those packs only when the host
//! actually has them.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use leveler_agent::NoopSink;
use leveler_core::{EnvSnapshot, RequestId, ToolCallId};
use leveler_execution::{PermissionProfile, Workspace};
use leveler_model::{
    ContentPart, FinishReason, Message, ModelError, ModelEventStream, ModelProfile, ModelRef,
    ModelRequest, ModelResponse, ModelRuntime, PromptSegment, PromptSource, Role, TokenEstimate,
    TokenUsage, ToolCall, ToolDefinition, estimate_text, estimate_tokens,
    estimate_tool_definitions,
};
use leveler_tools::{CapabilityPacks, ToolContext, model_surface};
use tokio_util::sync::CancellationToken;

struct Script {
    responses: Mutex<VecDeque<ModelResponse>>,
    requests: Mutex<Vec<ModelRequest>>,
}

impl Script {
    fn new(responses: Vec<ModelResponse>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses.into()),
            requests: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl ModelRuntime for Script {
    async fn generate(
        &self,
        _: ModelRequest,
        _: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        Err(ModelError::new(
            leveler_model::ModelErrorKind::Other,
            "inventory uses stream",
        ))
    }

    async fn stream(
        &self,
        request: ModelRequest,
        _: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        self.requests.lock().unwrap().push(request);
        let response = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("script exhausted");
        Ok(leveler_model::stream_from_response(response))
    }

    async fn profile(&self, model: &ModelRef) -> Result<ModelProfile, ModelError> {
        Ok(serde_json::from_value(serde_json::json!({
            "id": model.to_string(),
            "provider": model.provider,
            "model_id": model.model,
            "protocol": "openai_chat",
            "capabilities": {
                "streaming": true,
                "tool_calling": true,
                "parallel_tool_calls": true,
                "structured_output": false,
                "reasoning": false,
                "vision": false
            },
            "limits": {
                "context_window": 131072,
                "reliable_context": 65536,
                "max_output_tokens": 8192,
                "max_tool_schema_bytes": 65536,
                "max_parallel_tool_calls": 4
            },
            "reasoning": {"style": "none"}
        }))
        .unwrap())
    }
}

fn text(value: &str) -> ModelResponse {
    ModelResponse {
        request_id: RequestId::generate(),
        message: Message::text(Role::Assistant, value),
        finish_reason: FinishReason::Stop,
        usage: TokenUsage::default(),
    }
}

fn tool_call(id: &str, name: &str, args: serde_json::Value) -> ModelResponse {
    ModelResponse {
        request_id: RequestId::generate(),
        message: Message {
            origin: None,
            role: Role::Assistant,
            content: vec![ContentPart::ToolCall {
                call: ToolCall {
                    id: ToolCallId::new(id),
                    name: name.to_string(),
                    arguments: args,
                },
            }],
        },
        finish_reason: FinishReason::ToolCalls,
        usage: TokenUsage::default(),
    }
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn isolated_env(root: &Path, home: &Path) -> Arc<EnvSnapshot> {
    Arc::new(EnvSnapshot::new(
        [
            ("HOME".into(), home.as_os_str().to_os_string()),
            ("USERPROFILE".into(), home.as_os_str().to_os_string()),
            ("PATH".into(), std::env::var_os("PATH").unwrap_or_default()),
        ],
        root.to_path_buf(),
        std::env::temp_dir(),
    ))
}

/// Internal minimal baseline enables no optional packs. Full availability is the
/// mechanical intersection for a non-vision model with git, no search key and no browser.
fn packs(minimal_baseline: bool) -> CapabilityPacks {
    if minimal_baseline {
        CapabilityPacks::NONE
    } else {
        CapabilityPacks {
            code_intelligence: true,
            vcs: true,
            web_fetch: true,
            web_search: false,
            media: false,
            memory: true,
            skills: true,
            browser: false,
        }
    }
}

fn registry(minimal_baseline: bool, env: Arc<EnvSnapshot>) -> leveler_tools::ToolRegistry {
    let capabilities = leveler_tools::Capabilities::in_process(env);
    let mut registry = model_surface(packs(minimal_baseline), &capabilities);
    leveler_agent::register_harness_controls(&mut registry);
    registry
}

fn executor(
    root: &Path,
    home: &Path,
    script: Arc<Script>,
    minimal_baseline: bool,
    goal: bool,
) -> leveler_agent::Executor {
    let env = isolated_env(root, home);
    leveler_agent::Executor::new(
        script,
        Arc::new(registry(minimal_baseline, env.clone())),
        ToolContext::with_environment(
            Workspace::new(root).unwrap(),
            PermissionProfile::Assisted,
            env,
        ),
        ModelRef::new("mock", "m"),
        4,
    )
    .with_delegation(true)
    .with_memory_expose(!minimal_baseline)
    .with_goal_mode(goal)
}

fn split_tools(tools: &[ToolDefinition]) -> (u64, u64) {
    let mut description = TokenEstimate::new();
    let mut schema = TokenEstimate::new();
    for tool in tools {
        description.add_text(&tool.description);
        schema.add_tool(&tool.name);
        schema.add_tool(&tool.input_schema.to_string());
    }
    (description.tokens(), schema.tokens())
}

fn column(source: &PromptSource) -> &'static str {
    match source {
        PromptSource::ProjectRules { .. } | PromptSource::ScopedRule { .. } => "project",
        PromptSource::WorkspaceListing => "repo",
        PromptSource::SkillCatalog | PromptSource::AgentCatalog | PromptSource::Skill { .. } => {
            "catalog"
        }
        PromptSource::TurnFacts
        | PromptSource::ExecutionState
        | PromptSource::Resume
        | PromptSource::MemoryRecall { .. }
        | PromptSource::MemoryCatalog => "runtime",
        PromptSource::BasePrompt
        | PromptSource::MemoryGuidance
        | PromptSource::OperatingRules
        | PromptSource::FinalDelivery
        | PromptSource::GoalProtocol
        | PromptSource::AgentRole { .. }
        | PromptSource::AgentBrief
        | PromptSource::CommitTrailer
        | PromptSource::DelegationHint => "contract",
        _ => "other",
    }
}

fn report(case: &str, request: &ModelRequest) -> String {
    let mut out = String::new();
    let control = estimate_text(&request.control_context.text());
    let (desc, schema) = split_tools(&request.tools);
    let tools = estimate_tool_definitions(&request.tools);
    let messages = estimate_tokens(&request.messages);
    let projection = request
        .projection
        .as_ref()
        .map(|p| p.estimated_tokens())
        .unwrap_or(0);
    let mut columns = std::collections::BTreeMap::<&str, u64>::new();
    for segment in &request.control_context.blocks {
        *columns.entry(column(&segment.source)).or_default() += segment.token_estimate;
    }
    out.push_str(&format!(
        "CASE {case}\n  projection {projection}\n  control {control}\n  tools {tools} (description {desc} schema {schema})\n  messages {messages}\n  columns contract {} project {} repo {} catalog {} runtime {} other {}\n  tool_count {}\n  tools {}\n",
        columns.get("contract").copied().unwrap_or(0),
        columns.get("project").copied().unwrap_or(0),
        columns.get("repo").copied().unwrap_or(0),
        columns.get("catalog").copied().unwrap_or(0),
        columns.get("runtime").copied().unwrap_or(0),
        columns.get("other").copied().unwrap_or(0),
        request.tools.len(),
        request
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>()
            .join(","),
    ));
    let mut segments: Vec<&PromptSegment> = request.control_context.blocks.iter().collect();
    segments.sort_by_key(|segment| std::cmp::Reverse(segment.token_estimate));
    for segment in segments {
        out.push_str(&format!(
            "  SEG {:<22} {:<28} {:<24} {}\n",
            segment.name,
            format!("{:?}", segment.source)
                .chars()
                .take(28)
                .collect::<String>(),
            format!("{:?}", segment.authority),
            segment.token_estimate,
        ));
    }
    let mut ranked = request.tools.clone();
    ranked.sort_by_key(|tool| {
        std::cmp::Reverse(
            estimate_text(&tool.description) + estimate_text(&tool.input_schema.to_string()),
        )
    });
    for tool in ranked {
        let description = estimate_text(&tool.description);
        let mut schema_est = TokenEstimate::new();
        schema_est.add_tool(&tool.name);
        schema_est.add_tool(&tool.input_schema.to_string());
        out.push_str(&format!(
            "  TOOL {:<24} desc {:5} schema {:5} bytes_desc {}\n",
            tool.name,
            description,
            schema_est.tokens(),
            tool.description.len(),
        ));
    }
    const COACHING: &[&str] = &[
        "Prefer this",
        "Prefer ",
        "Use after",
        "Use before",
        "Do NOT",
        "Do not call",
        "when you should",
        "the moment",
        "Keep the work yourself",
        "before acting",
        "you must read",
        "instead of",
    ];
    for tool in &request.tools {
        for needle in COACHING {
            if tool.description.contains(needle) {
                out.push_str(&format!("  COACH {} contains {needle:?}\n", tool.name));
            }
        }
    }
    for segment in &request.control_context.blocks {
        for needle in COACHING {
            if segment.text.contains(needle) {
                out.push_str(&format!(
                    "  COACH segment {} contains {needle:?}\n",
                    segment.name
                ));
            }
        }
    }
    out
}

async fn capture(
    root: &Path,
    home: &Path,
    minimal_baseline: bool,
    goal: bool,
    prompt: &str,
    responses: Vec<ModelResponse>,
) -> ModelRequest {
    let script = Script::new(responses);
    executor(root, home, script.clone(), minimal_baseline, goal)
        .run(prompt, &mut |_| {}, &mut NoopSink, CancellationToken::new())
        .await
        .expect("turn");
    script.requests.lock().unwrap().remove(0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn current_prompt_surface_inventory() {
    let root = repo_root();
    assert!(
        root.join("AGENTS.md").is_file() || root.join("Agents.md").is_file(),
        "inventory measures this repo's project rules: {root:?}"
    );
    let home = tempfile::tempdir().unwrap();
    let simple = "What is 2+2?";
    let coding = "Read src/lib.rs, change one comment, and run the smallest check.";

    let chat = capture(&root, home.path(), false, false, simple, vec![text("4")]).await;
    let minimal_baseline = capture(&root, home.path(), true, false, simple, vec![text("4")]).await;
    let full_baseline = capture(&root, home.path(), false, false, coding, vec![text("done")]).await;
    let goal = capture(
        &root,
        home.path(),
        false,
        true,
        coding,
        vec![tool_call(
            "g1",
            "update_goal",
            serde_json::json!({"status": "complete", "summary": "done"}),
        )],
    )
    .await;

    let explorer_script = Script::new(vec![
        tool_call(
            "s1",
            "spawn_agent",
            serde_json::json!({
                "task": "Read one file and report what it exports.",
                "title": "read exports",
                "profile": "explorer",
                "run_in_background": false
            }),
        ),
        text("child report"),
        text("parent done"),
    ]);
    executor(&root, home.path(), explorer_script.clone(), false, false)
        .run(coding, &mut |_| {}, &mut NoopSink, CancellationToken::new())
        .await
        .expect("explorer spawn");
    let explorer = explorer_script
        .requests
        .lock()
        .unwrap()
        .iter()
        .find(|request| {
            request
                .tools
                .iter()
                .any(|tool| tool.name == "report_finding")
        })
        .cloned()
        .expect("explorer request");

    let worker_script = Script::new(vec![
        tool_call(
            "s1",
            "spawn_agent",
            serde_json::json!({
                "task": "Change one comment in the named file.",
                "title": "edit comment",
                "profile": "worker",
                "files": ["crates/leveler-agent/src/lib.rs"],
                "run_in_background": false
            }),
        ),
        text("child report"),
        text("parent done"),
    ]);
    executor(&root, home.path(), worker_script.clone(), false, false)
        .run(coding, &mut |_| {}, &mut NoopSink, CancellationToken::new())
        .await
        .expect("worker spawn");
    let worker = worker_script
        .requests
        .lock()
        .unwrap()
        .iter()
        .find(|request| {
            request
                .tools
                .iter()
                .any(|tool| tool.name == "report_finding")
                && request.tools.iter().any(|tool| tool.name == "apply_patch")
        })
        .cloned()
        .expect("worker request");

    let reviewer_script = Script::new(vec![text("no defect")]);
    let reviewer_executor = executor(&root, home.path(), reviewer_script.clone(), false, false);
    let _ = reviewer_executor
        .run_reviewer_child(
            "rev-1".into(),
            "Judge the comment change.".into(),
            vec!["crates/leveler-agent/src/lib.rs".into()],
            std::time::Duration::ZERO,
            None,
            &mut |_| {},
            CancellationToken::new(),
        )
        .await;
    let reviewer = reviewer_script
        .requests
        .lock()
        .unwrap()
        .first()
        .cloned()
        .expect("reviewer request");

    let mut report_text = String::from(
        "CURRENT_PROMPT_SURFACE\n\
         pin: minimal_baseline=no optional packs; full_baseline=code_intelligence+vcs+web_fetch+memory+skills; \
         web_search/browser/media off (no key, no browser handle, non-vision)\n\
         home: empty, so user-level skills and agents are not in these numbers\n\
         fixed = control + tools; messages are the turn input, not history growth\n\n",
    );
    let cases = [
        ("chat", &chat, 22_415_u64),
        ("minimal_baseline", &minimal_baseline, 19_715),
        ("full_baseline", &full_baseline, 22_415),
        ("goal", &goal, 23_145),
        ("explorer", &explorer, 13_820),
        ("worker", &worker, 18_444),
        ("reviewer", &reviewer, 13_850),
    ];
    for (name, request, baseline) in cases {
        report_text.push_str(&report(name, request));
        report_text.push('\n');
        assert!(request.projection.is_some(), "{name} has a projection");
        let fixed = estimate_text(&request.control_context.text())
            + estimate_tool_definitions(&request.tools);
        // Five percent, and at least 500 tokens, so a short copy edit does not
        // fail the gate and a few thousand tokens does.
        let slack = (baseline / 20).max(500);
        assert!(
            fixed <= baseline + slack,
            "{name} fixed context {fixed} exceeds baseline {baseline} + {slack}"
        );
        assert!(
            request.tools.iter().all(|tool| tool.name != "ask_user"),
            "{name} still advertises ask_user"
        );
    }
    assert!(
        minimal_baseline
            .control_context
            .blocks
            .iter()
            .all(|segment| segment.name != "skill_index"),
        "minimal_baseline has no load_skill, so it does not get the skill catalog"
    );
    for (name, request) in [("explorer", &explorer), ("reviewer", &reviewer)] {
        for absent in ["request_user_input", "request_permissions", "spawn_agent"] {
            assert!(
                request.tools.iter().all(|tool| tool.name != absent),
                "{name} advertises {absent}"
            );
        }
    }
    let rules = chat
        .control_context
        .blocks
        .iter()
        .find(|segment| segment.name == "project_rules")
        .expect("project rules");
    assert!(
        rules.token_estimate <= 5_800,
        "project-rule delivery grew past the section budget: {}",
        rules.token_estimate
    );
    assert!(
        !rules.text.contains("sections NOT delivered"),
        "omitted sections are listed by read_project_rules"
    );
    let path = std::env::temp_dir().join("prompt-surface-current.txt");
    std::fs::write(&path, &report_text).unwrap();
    eprintln!("{report_text}\nWROTE {}", path.display());
}

/// Internal primitive baseline used to compare schema costs with the progressive
/// initial surface. This fixture is not a user-selectable product mode.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initial_baseline_inventory() {
    let root = repo_root();
    let home = tempfile::tempdir().unwrap();

    let initial_baseline = {
        let env = isolated_env(&root, home.path());
        let capabilities = leveler_tools::Capabilities::in_process(env.clone());
        let mut registry = model_surface(
            CapabilityPacks {
                vcs: true,
                ..CapabilityPacks::NONE
            },
            &capabilities,
        );
        leveler_agent::register_harness_controls_with(
            &mut registry,
            leveler_agent::HarnessControls::PROTOCOL_ONLY,
        );
        let script = Script::new(vec![text("done")]);
        leveler_agent::Executor::new(
            script.clone(),
            Arc::new(registry),
            ToolContext::with_environment(
                Workspace::new(&root).unwrap(),
                PermissionProfile::Assisted,
                env,
            ),
            ModelRef::new("mock", "m"),
            4,
        )
        .with_delegation(false)
        .with_host_input(false)
        .with_goal_mode(false)
        .run(
            "Read src/lib.rs, change one comment, and run the smallest check.",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .expect("initial_baseline turn");
        script.requests.lock().unwrap().remove(0)
    };

    let goal_initial_baseline = {
        let env = isolated_env(&root, home.path());
        let capabilities = leveler_tools::Capabilities::in_process(env.clone());
        let mut registry = model_surface(
            CapabilityPacks {
                vcs: true,
                ..CapabilityPacks::NONE
            },
            &capabilities,
        );
        leveler_agent::register_harness_controls_with(
            &mut registry,
            leveler_agent::HarnessControls::PROTOCOL_ONLY,
        );
        let script = Script::new(vec![tool_call(
            "g1",
            "update_goal",
            serde_json::json!({"status": "complete", "summary": "done"}),
        )]);
        leveler_agent::Executor::new(
            script.clone(),
            Arc::new(registry),
            ToolContext::with_environment(
                Workspace::new(&root).unwrap(),
                PermissionProfile::Assisted,
                env,
            ),
            ModelRef::new("mock", "m"),
            4,
        )
        .with_delegation(false)
        .with_host_input(false)
        .with_goal_mode(true)
        .run(
            "Read src/lib.rs, change one comment, and run the smallest check.",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .expect("initial_baseline goal turn");
        script.requests.lock().unwrap().remove(0)
    };

    for (name, request) in [
        ("initial_baseline", &initial_baseline),
        ("initial_baseline_goal", &goal_initial_baseline),
    ] {
        eprintln!("{}", report(name, request));
    }
    for absent in [
        "save_agent",
        "save_skill",
        "spawn_agent",
        "request_user_input",
        "remember",
        "browser_act",
        "web_fetch",
        "find_symbol",
        "load_skill",
    ] {
        assert!(
            initial_baseline
                .tools
                .iter()
                .all(|tool| tool.name != absent),
            "initial primitive baseline must not carry {absent}"
        );
    }
    for present in [
        "read_file",
        "apply_patch",
        "run_command",
        "shell_command",
        "git_status",
        "git_diff",
        "update_plan",
    ] {
        assert!(
            initial_baseline
                .tools
                .iter()
                .any(|tool| tool.name == present),
            "initial primitive baseline must carry {present}"
        );
    }
    assert!(
        initial_baseline
            .tools
            .iter()
            .all(|tool| tool.name != "update_goal"),
        "update_goal is goal-mode only"
    );
    assert!(
        goal_initial_baseline
            .tools
            .iter()
            .any(|tool| tool.name == "update_goal"),
        "goal initial_baseline carries update_goal"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn progressive_initial_surface_inventory() {
    use leveler_agent::capability::{CapabilityDisclosure, CapabilityId, control_definition};
    let root = repo_root();
    let home = tempfile::tempdir().unwrap();
    let available = vec![
        CapabilityId::Memory,
        CapabilityId::MultiAgent,
        CapabilityId::Skills,
        CapabilityId::CodeIntelligence,
        CapabilityId::Web,
        CapabilityId::Authoring,
        CapabilityId::HostInteraction,
    ];
    let disclosure =
        Arc::new(CapabilityDisclosure::new(available.clone(), available, vec![]).unwrap());
    let script = Script::new(vec![tool_call(
        "done",
        "update_goal",
        serde_json::json!({"status":"complete","summary":"done"}),
    )]);
    executor(&root, home.path(), script.clone(), false, true)
        .with_capabilities(Some(disclosure.clone()))
        .run(
            "Inspect a local source file.",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let initial = script.requests.lock().unwrap().remove(0);
    let full = capture(
        &root,
        home.path(),
        false,
        true,
        "Inspect a local source file.",
        vec![tool_call(
            "done",
            "update_goal",
            serde_json::json!({"status":"complete","summary":"done"}),
        )],
    )
    .await;
    for required in [
        "read_file",
        "list_files",
        "find_files",
        "grep",
        "read_project_rules",
        "apply_patch",
        "write_file",
        "run_command",
        "shell_command",
        "get_task",
        "wait_task",
        "kill_task",
        "git_status",
        "git_diff",
        "update_plan",
        "update_goal",
        "capability",
        "request_permissions",
    ] {
        assert!(
            initial.tools.iter().any(|tool| tool.name == required),
            "initial surface missing {required}"
        );
    }
    for optional in [
        "find_symbol",
        "read_symbol",
        "find_references",
        "diagnostics",
        "blast_radius",
        "web_fetch",
        "web_search",
        "view_image",
        "memory",
        "remember",
        "forget",
        "load_skill",
        "browser_tab",
        "browser_act",
        "browser_inspect",
        "spawn_agent",
        "list_agents",
        "save_agent",
        "delete_agent",
        "save_skill",
        "delete_skill",
        "request_user_input",
    ] {
        assert!(
            !initial.tools.iter().any(|tool| tool.name == optional),
            "initial surface exposes {optional}"
        );
    }
    let catalog_tokens = estimate_text(&disclosure.catalog().to_string());
    let control_tokens = estimate_tool_definitions(&[control_definition()]);
    let schema_tokens = estimate_tool_definitions(&initial.tools);
    assert!(
        catalog_tokens + control_tokens < 1000,
        "capability discovery exceeded its budget"
    );
    assert_eq!(initial.tools.len(), 18, "pinned initial surface grew");
    assert!(
        schema_tokens <= 6800,
        "initial schema grew beyond its fixed budget: {schema_tokens}"
    );
    assert!(initial.tools.len() < full.tools.len());
    eprintln!(
        "PROGRESSIVE_INITIAL_SURFACE initial_tool_count={} initial_schema_tokens={} catalog_tokens={} control_schema_tokens={} available_full_count={} full_schema_tokens={} fixed_request_tokens={}",
        initial.tools.len(),
        schema_tokens,
        catalog_tokens,
        control_tokens,
        full.tools.len(),
        estimate_tool_definitions(&full.tools),
        schema_tokens + estimate_text(&initial.control_context.text())
    );
    eprintln!("{}", report("progressive_initial", &initial));
}
