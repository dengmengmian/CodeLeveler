//! Request control must survive recovery and compaction without becoming dialogue.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use leveler_agent::{Executor, NoopSink, TranscriptSink};
use leveler_core::{RequestId, ToolCallId};
use leveler_execution::{PermissionProfile, Workspace};
use leveler_model::{
    ContentPart, FinishReason, Message, ModelError, ModelEvent, ModelEventStream, ModelProfile,
    ModelRef, ModelRequest, ModelResponse, ModelRuntime, Role, TokenUsage, ToolCall, ToolChoice,
};
use leveler_tools::ToolContext;
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
        request: ModelRequest,
        _: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        self.requests.lock().unwrap().push(request);
        Ok(answer("A model-generated summary of earlier conversation."))
    }

    async fn stream(
        &self,
        request: ModelRequest,
        _: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        let summary = request.tool_choice == ToolChoice::None;
        self.requests.lock().unwrap().push(request);
        let response = if summary {
            answer("A model-generated summary of earlier conversation.")
        } else {
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("script exhausted")
        };
        let mut events = vec![Ok(ModelEvent::MessageStarted {
            request_id: response.request_id,
        })];
        for part in response.message.content {
            match part {
                ContentPart::ToolCall { call } => {
                    events.push(Ok(ModelEvent::ToolCallCompleted { call }))
                }
                ContentPart::Text { text } => {
                    events.push(Ok(ModelEvent::TextDelta { delta: text }))
                }
                _ => panic!("unexpected scripted content"),
            }
        }
        events.push(Ok(ModelEvent::MessageCompleted {
            finish_reason: response.finish_reason,
        }));
        Ok(Box::pin(futures::stream::iter(events)))
    }

    async fn profile(&self, model: &ModelRef) -> Result<ModelProfile, ModelError> {
        Ok(serde_json::from_value(serde_json::json!({
            "id":model.to_string(),"provider":model.provider,"model_id":model.model,
            "protocol":"openai_chat",
            "capabilities":{"streaming":true,"tool_calling":true,"parallel_tool_calls":false,
                "structured_output":false,"reasoning":false,"vision":false},
            "limits":{"context_window":131072,"reliable_context":65536,"max_output_tokens":8192,
                "max_tool_schema_bytes":32768,"max_parallel_tool_calls":1},
            "reasoning":{"style":"none"}
        }))
        .unwrap())
    }
}

fn answer(text: &str) -> ModelResponse {
    ModelResponse {
        request_id: RequestId::generate(),
        message: Message::text(Role::Assistant, text),
        finish_reason: FinishReason::Stop,
        usage: TokenUsage::default(),
    }
}

fn read(path: &str) -> ModelResponse {
    ModelResponse {
        request_id: RequestId::generate(),
        message: Message {
            origin: None,
            role: Role::Assistant,
            content: vec![ContentPart::ToolCall {
                call: ToolCall {
                    id: ToolCallId::new("read-source"),
                    name: "read_file".into(),
                    arguments: serde_json::json!({"path":path}),
                },
            }],
        },
        finish_reason: FinishReason::ToolCalls,
        usage: TokenUsage::default(),
    }
}

fn executor(
    root: &std::path::Path,
    script: Arc<Script>,
    mode: PermissionProfile,
    rounds: u32,
) -> Executor {
    Executor::new(
        script,
        Arc::new(leveler_tools::default_registry()),
        ToolContext::new(Workspace::new(root).unwrap(), mode),
        ModelRef::new("mock", "m"),
        rounds,
    )
    .with_delegation(false)
}

struct Saved(Arc<Mutex<Vec<Message>>>);

#[async_trait]
impl TranscriptSink for Saved {
    async fn append(&mut self, messages: &[Message]) -> Result<(), leveler_engine::PortError> {
        self.0.lock().unwrap().extend_from_slice(messages);
        Ok(())
    }
}

#[tokio::test]
async fn resume_rebuilds_current_control_and_preserves_conversation() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("AGENTS.md"), "PROJECT_A_SENTINEL").unwrap();
    let first = Script::new(vec![answer("first reply")]);
    let saved = Arc::new(Mutex::new(Vec::new()));
    executor(dir.path(), first.clone(), PermissionProfile::Assisted, 3)
        .run(
            "inspect",
            &mut |_| {},
            &mut Saved(saved.clone()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(
        first.requests.lock().unwrap()[0]
            .control_context
            .text()
            .contains("PROJECT_A_SENTINEL")
    );
    let conversation = saved.lock().unwrap().clone();
    assert!(conversation.iter().all(|m| m.role != Role::System));

    std::fs::write(dir.path().join("AGENTS.md"), "PROJECT_B_SENTINEL").unwrap();
    let second = Script::new(vec![answer("resumed reply")]);
    let mut legacy = vec![Message::text(Role::System, "OLD_RUNTIME_A")];
    legacy.extend(conversation.clone());
    legacy.push(Message::text(Role::User, "continue, do not edit"));
    legacy.push(Message::text(Role::System, "OLD_RESUME_CONTRACT"));
    executor(dir.path(), second.clone(), PermissionProfile::FullAccess, 3)
        .resume(legacy, &mut |_| {}, &mut NoopSink, CancellationToken::new())
        .await
        .unwrap();
    let requests = second.requests.lock().unwrap();
    let request = &requests[0];
    let control = request.control_context.text();
    assert!(control.contains("PROJECT_B_SENTINEL"));
    assert!(control.contains("full-access"));
    for old in ["PROJECT_A_SENTINEL", "OLD_RUNTIME_A", "OLD_RESUME_CONTRACT"] {
        assert!(!control.contains(old), "stale control survived: {old}");
    }
    assert!(request.messages.iter().all(|m| m.role != Role::System));
    assert_eq!(
        &request.messages[..conversation.len()],
        conversation.as_slice()
    );
    assert_eq!(
        request.messages.last().unwrap().text_content(),
        "continue, do not edit"
    );
    assert!(
        request
            .control_context
            .blocks
            .iter()
            .any(|b| b.name == "resume_constraint")
    );
}

#[tokio::test]
async fn scoped_sources_survive_recovery_without_rule_bodies_in_history() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/AGENTS.md"), "SCOPED_A_SENTINEL").unwrap();
    std::fs::write(dir.path().join("src/a.rs"), "pub fn a() {}\n").unwrap();
    let first = Script::new(vec![read("src/a.rs"), answer("read")]);
    let saved = Arc::new(Mutex::new(Vec::new()));
    let outcome = executor(dir.path(), first.clone(), PermissionProfile::Assisted, 3)
        .run(
            "inspect source",
            &mut |_| {},
            &mut Saved(saved.clone()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(outcome.progress.scoped_rule_sources, vec!["src/AGENTS.md"]);
    assert!(
        !saved
            .lock()
            .unwrap()
            .iter()
            .any(|m| m.text_content().contains("SCOPED_A_SENTINEL"))
    );
    assert!(
        first.requests.lock().unwrap()[1]
            .control_context
            .text()
            .contains("SCOPED_A_SENTINEL")
    );

    std::fs::write(dir.path().join("src/AGENTS.md"), "SCOPED_B_SENTINEL").unwrap();
    let second = Script::new(vec![answer("continued")]);
    // A context checkpoint can have elided the original tool calls. Source
    // facts, rather than old prompt bodies, still rebuild the current scope.
    executor(dir.path(), second.clone(), PermissionProfile::Assisted, 3)
        .with_seeded_progress(outcome.progress)
        .resume(
            vec![Message::text(Role::User, "inspect source")],
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let requests = second.requests.lock().unwrap();
    assert!(
        requests[0]
            .control_context
            .text()
            .contains("SCOPED_B_SENTINEL")
    );
    assert!(
        !requests[0]
            .control_context
            .text()
            .contains("SCOPED_A_SENTINEL")
    );
}

#[tokio::test]
async fn resume_reloads_selected_skill_and_honors_current_memory_exposure() {
    let dir = tempfile::tempdir().unwrap();
    let skill_dir = dir.path().join(".leveler/skills/demo");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: demo\ndescription: Demo.\n---\nSKILL_A_SENTINEL",
    )
    .unwrap();
    let first = Script::new(vec![answer("first reply")]);
    let saved = Arc::new(Mutex::new(Vec::new()));
    executor(dir.path(), first.clone(), PermissionProfile::Assisted, 3)
        .with_memory_expose(true)
        .with_memory_catalog("OLD_MEMORY_CATALOG_SENTINEL")
        .run(
            "use $demo to inspect",
            &mut |_| {},
            &mut Saved(saved.clone()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(
        first.requests.lock().unwrap()[0]
            .control_context
            .text()
            .contains("SKILL_A_SENTINEL")
    );
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: demo\ndescription: Demo.\n---\nSKILL_B_SENTINEL",
    )
    .unwrap();
    let second = Script::new(vec![answer("resumed")]);
    let mut prior = saved.lock().unwrap().clone();
    prior.push(Message::text(Role::User, "continue"));
    executor(dir.path(), second.clone(), PermissionProfile::Assisted, 3)
        .with_memory_expose(false)
        .resume(prior, &mut |_| {}, &mut NoopSink, CancellationToken::new())
        .await
        .unwrap();
    let requests = second.requests.lock().unwrap();
    let control = requests[0].control_context.text();
    assert!(
        control.contains("SKILL_B_SENTINEL"),
        "resume lost the active selected procedure"
    );
    assert!(!control.contains("SKILL_A_SENTINEL"));
    assert!(!control.contains("OLD_MEMORY_CATALOG_SENTINEL"));
    assert!(
        !requests[0]
            .control_context
            .blocks
            .iter()
            .any(|s| s.name == "memory_guidance")
    );
}

#[tokio::test]
async fn compaction_summarizes_history_and_preserves_current_control() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("AGENTS.md"),
        "CONTROL_NOT_FOR_SUMMARY_SENTINEL",
    )
    .unwrap();
    std::fs::write(dir.path().join("input.txt"), "evidence").unwrap();
    let script = Script::new(vec![read("input.txt"), answer("done")]);
    let policy = leveler_agent::coding::policy::ResolvedContextPolicy {
        pressure_threshold: 100,
        retention: leveler_agent::coding::policy::ContextRetentionPolicy {
            keep_recent_messages: 3,
            keep_recent_tokens: 1000,
        },
        ..Default::default()
    };
    let prior: Vec<_> = (0..12)
        .map(|i| {
            Message::text(
                if i % 2 == 0 {
                    Role::User
                } else {
                    Role::Assistant
                },
                format!("OLD_HISTORY_SENTINEL_{i} {}", "history ".repeat(60)),
            )
        })
        .collect();
    let mut events = Vec::new();
    executor(dir.path(), script.clone(), PermissionProfile::Assisted, 5)
        .with_context_policy(policy)
        .run_conversation(
            prior,
            vec![ContentPart::Text {
                text: "inspect input".into(),
            }],
            &mut |event| events.push(event),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let requests = script.requests.lock().unwrap();
    let summary = requests
        .iter()
        .find(|r| r.tool_choice == ToolChoice::None)
        .expect("summary request");
    assert!(summary.control_context.blocks.is_empty());
    assert!(!summary.messages.iter().any(|m| {
        m.text_content()
            .contains("CONTROL_NOT_FOR_SUMMARY_SENTINEL")
    }));
    let last = requests.last().unwrap();
    assert!(
        last.control_context
            .text()
            .contains("CONTROL_NOT_FOR_SUMMARY_SENTINEL")
    );
    assert!(
        last.messages
            .iter()
            .any(|m| m.text_content().contains("Earlier context was compacted"))
    );
    assert!(
        !last
            .messages
            .iter()
            .any(|m| m.text_content().contains("OLD_HISTORY_SENTINEL_2"))
    );
    assert!(
        last.messages
            .iter()
            .any(|m| m.text_content().contains("inspect input")),
        "objective must remain pinned"
    );
    let accounting = events
        .iter()
        .filter_map(|event| match event {
            leveler_agent::AgentEvent::ContextUsage { accounting } => Some(accounting),
            _ => None,
        })
        .next_back()
        .expect("request accounting");
    let fold = accounting
        .last_compaction
        .as_ref()
        .expect("fold accounting");
    let stable_control = leveler_model::ControlContext {
        blocks: last
            .control_context
            .blocks
            .iter()
            .filter(|s| s.name != "execution_state")
            .cloned()
            .collect(),
    };
    let control_and_tools = leveler_model::RequestProjection::project_with_control_context(
        &[],
        &last.tools,
        leveler_model::ReasoningReplayContract::NONE,
        leveler_model::ReasoningRetention::All,
        &stable_control,
    )
    .estimated_tokens();
    assert!(
        fold.after_tokens >= control_and_tools,
        "post-compaction accounting must include the still-visible control and tools"
    );
}

#[tokio::test]
async fn resume_uses_user_objective_instead_of_runtime_user_rows() {
    let dir = tempfile::tempdir().unwrap();
    let script = Script::new(vec![answer("继续处理")]);
    executor(dir.path(), script.clone(), PermissionProfile::Assisted, 3)
        .resume(
            vec![
                Message::text(Role::User, "请检查这个中文任务"),
                Message::text(Role::Assistant, "正在检查"),
                Message::text(
                    Role::User,
                    "[Earlier context was compacted: runtime notification]",
                ),
            ],
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let requests = script.requests.lock().unwrap();
    let turn = requests[0]
        .control_context
        .blocks
        .iter()
        .find(|s| s.name == "turn_facts")
        .unwrap();
    assert!(
        turn.text.contains("Chinese"),
        "runtime notification must not decide user language: {}",
        turn.text
    );
}
