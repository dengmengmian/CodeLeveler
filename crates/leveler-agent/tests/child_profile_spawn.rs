//! Spawn integration for Child Profile: profile=explorer/worker, harness
//! reviewer, security fences, and the omitted-profile compatibility path.

use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use leveler_agent::{AgentEvent, Executor, NoopSink};
use leveler_core::{RequestId, ToolCallId};
use leveler_execution::{PermissionProfile, Workspace};
use leveler_model::{
    ContentPart, FinishReason, Message, ModelError, ModelEventStream, ModelRef, ModelRequest,
    ModelResponse, ModelRuntime, Role, TokenUsage, ToolCall,
};
use leveler_tools::{ToolContext, default_registry};

struct ScriptedRuntime {
    responses: Mutex<VecDeque<ModelResponse>>,
}

impl ScriptedRuntime {
    fn new(responses: Vec<ModelResponse>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
        }
    }
}

#[async_trait]
impl ModelRuntime for ScriptedRuntime {
    async fn generate(
        &self,
        _request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        unimplemented!()
    }

    async fn stream(
        &self,
        _request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        use leveler_model::ModelEvent;
        let response = self.responses.lock().unwrap().pop_front().ok_or_else(|| {
            ModelError::new(
                leveler_model::ModelErrorKind::Other,
                "scripted runtime exhausted",
            )
        })?;
        let mut events: Vec<Result<ModelEvent, ModelError>> =
            vec![Ok(ModelEvent::MessageStarted {
                request_id: response.request_id.clone(),
            })];
        for part in &response.message.content {
            match part {
                ContentPart::Text { text } => events.push(Ok(ModelEvent::TextDelta {
                    delta: text.clone(),
                })),
                ContentPart::ToolCall { call } => {
                    events.push(Ok(ModelEvent::ToolCallCompleted { call: call.clone() }))
                }
                _ => {}
            }
        }
        events.push(Ok(ModelEvent::MessageCompleted {
            finish_reason: response.finish_reason,
        }));
        Ok(Box::pin(futures::stream::iter(events)))
    }

    async fn profile(&self, _model: &ModelRef) -> Result<leveler_model::ModelProfile, ModelError> {
        unimplemented!()
    }
}

fn assistant_with(parts: Vec<ContentPart>, finish: FinishReason) -> ModelResponse {
    ModelResponse {
        request_id: RequestId::generate(),
        message: Message {
            role: Role::Assistant,
            content: parts,
        },
        finish_reason: finish,
        usage: TokenUsage::default(),
    }
}

fn assistant_text(text: &str) -> ModelResponse {
    ModelResponse {
        request_id: RequestId::generate(),
        message: Message::text(Role::Assistant, text),
        finish_reason: FinishReason::Stop,
        usage: TokenUsage::default(),
    }
}

fn spawn_call(id: &str, args: serde_json::Value) -> ContentPart {
    let mut args = args;
    if args.get("run_in_background").is_none() {
        args["run_in_background"] = serde_json::Value::Bool(false);
    }
    ContentPart::ToolCall {
        call: ToolCall {
            id: ToolCallId::new(id),
            name: "spawn_agent".to_string(),
            arguments: args,
        },
    }
}

fn tmp(tag: &str, salt: u64) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("leveler-child-profile-{tag}-{salt}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn run_spawn(dir: &std::path::Path, script: Vec<ModelResponse>) -> Vec<AgentEvent> {
    let workspace = Workspace::new(dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let mut events = Vec::new();
    Executor::new(
        std::sync::Arc::new(ScriptedRuntime::new(script)),
        std::sync::Arc::new(default_registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        8,
    )
    .run(
        "delegate",
        &mut |e| events.push(e),
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    events
}

fn started_profile(events: &[AgentEvent]) -> (String, Option<String>, Option<String>, Vec<String>) {
    events
        .iter()
        .find_map(|e| match e {
            AgentEvent::SubAgentStarted {
                role,
                profile_id,
                profile_role,
                capabilities,
                ..
            } => Some((
                role.clone(),
                profile_id.clone(),
                profile_role.clone(),
                capabilities.clone(),
            )),
            _ => None,
        })
        .expect("SubAgentStarted")
}

fn spawn_error(events: &[AgentEvent]) -> String {
    events
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
        .expect("spawn_agent error")
}

#[tokio::test]
async fn spawn_with_explorer_profile_records_the_contract() {
    let dir = tmp("explorer", 1);
    let events = run_spawn(
        &dir,
        vec![
            assistant_with(
                vec![spawn_call(
                    "s1",
                    serde_json::json!({"task": "map the architecture", "profile": "explorer"}),
                )],
                FinishReason::ToolCalls,
            ),
            assistant_text("nothing to flag"),
            assistant_text("parent done"),
        ],
    )
    .await;
    let (role, profile_id, profile_role, caps) = started_profile(&events);
    assert_eq!(role, "explorer");
    assert_eq!(profile_id.as_deref(), Some("explorer"));
    assert_eq!(profile_role.as_deref(), Some("explorer"));
    assert!(caps.iter().any(|c| c == "repository_analysis"));
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn spawn_with_worker_profile_requires_files_and_records_the_contract() {
    let dir = tmp("worker", 2);
    std::fs::write(dir.join("a.rs"), "fn a() {}\n").unwrap();
    let events = run_spawn(
        &dir,
        vec![
            assistant_with(
                vec![spawn_call(
                    "s1",
                    serde_json::json!({
                        "task": "edit a.rs",
                        "profile": "worker",
                        "files": ["a.rs"]
                    }),
                )],
                FinishReason::ToolCalls,
            ),
            assistant_text("done"),
            assistant_text("parent done"),
        ],
    )
    .await;
    let (role, profile_id, _, caps) = started_profile(&events);
    assert_eq!(role, "worker");
    assert_eq!(profile_id.as_deref(), Some("worker"));
    assert!(caps.iter().any(|c| c == "implementation"));
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn spawn_with_reviewer_profile_is_refused() {
    let dir = tmp("reviewer", 3);
    let events = run_spawn(
        &dir,
        vec![
            assistant_with(
                vec![spawn_call(
                    "s1",
                    serde_json::json!({"task": "review the patch", "profile": "reviewer"}),
                )],
                FinishReason::ToolCalls,
            ),
            assistant_text("parent done"),
        ],
    )
    .await;
    let err = spawn_error(&events);
    assert!(err.contains("harness"), "{err}");
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::SubAgentStarted { .. })),
        "a model-requested reviewer must not start"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn omitted_profile_maps_to_the_default_child() {
    let dir = tmp("default", 4);
    let events = run_spawn(
        &dir,
        vec![
            assistant_with(
                vec![spawn_call(
                    "s1",
                    serde_json::json!({"task": "do the subtask"}),
                )],
                FinishReason::ToolCalls,
            ),
            assistant_text("child done"),
            assistant_text("parent done"),
        ],
    )
    .await;
    let (role, profile_id, profile_role, _) = started_profile(&events);
    assert_eq!(role, "default");
    assert_eq!(profile_id.as_deref(), Some("default"));
    assert_eq!(profile_role.as_deref(), Some("default"));
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn historical_role_argument_still_selects_explorer() {
    let dir = tmp("role-compat", 5);
    let events = run_spawn(
        &dir,
        vec![
            assistant_with(
                vec![spawn_call(
                    "s1",
                    serde_json::json!({"task": "look around", "role": "explorer"}),
                )],
                FinishReason::ToolCalls,
            ),
            assistant_text("nothing"),
            assistant_text("parent done"),
        ],
    )
    .await;
    let (role, profile_id, _, _) = started_profile(&events);
    assert_eq!(role, "explorer");
    assert_eq!(profile_id.as_deref(), Some("explorer"));
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn unknown_profile_is_an_honest_denial() {
    let dir = tmp("unknown", 6);
    let events = run_spawn(
        &dir,
        vec![
            assistant_with(
                vec![spawn_call(
                    "s1",
                    serde_json::json!({"task": "go", "profile": "marketplace.cool"}),
                )],
                FinishReason::ToolCalls,
            ),
            assistant_text("parent done"),
        ],
    )
    .await;
    let err = spawn_error(&events);
    assert!(err.contains("Unknown profile"), "{err}");
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::SubAgentStarted { .. }))
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn explorer_profile_cannot_write() {
    let dir = tmp("explorer-write", 7);
    std::fs::write(dir.join("lib.rs"), "pub fn old() {}\n").unwrap();
    let _events = run_spawn(
        &dir,
        vec![
            assistant_with(
                vec![spawn_call(
                    "s1",
                    serde_json::json!({"task": "look and try to edit", "profile": "explorer"}),
                )],
                FinishReason::ToolCalls,
            ),
            assistant_with(
                vec![ContentPart::ToolCall {
                    call: ToolCall {
                        id: ToolCallId::new("c1"),
                        name: "apply_patch".to_string(),
                        arguments: serde_json::json!({
                            "patch": "*** Begin Patch\n*** Update File: lib.rs\n pub fn old() {}\n+pub fn added() {}\n*** End Patch"
                        }),
                    },
                }],
                FinishReason::ToolCalls,
            ),
            assistant_text("could not edit"),
            assistant_text("parent done"),
        ],
    )
    .await;
    let content = std::fs::read_to_string(dir.join("lib.rs")).unwrap();
    assert_eq!(
        content, "pub fn old() {}\n",
        "explorer must not modify files"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn worker_profile_without_files_is_refused() {
    let dir = tmp("worker-noscope", 8);
    let events = run_spawn(
        &dir,
        vec![
            assistant_with(
                vec![spawn_call(
                    "s1",
                    serde_json::json!({"task": "edit something", "profile": "worker"}),
                )],
                FinishReason::ToolCalls,
            ),
            assistant_text("parent done"),
        ],
    )
    .await;
    let err = spawn_error(&events);
    assert!(err.contains("files"), "{err}");
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::SubAgentStarted { .. }))
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn contribution_projection_carries_the_profile() {
    let dir = tmp("trace", 9);
    let events = run_spawn(
        &dir,
        vec![
            assistant_with(
                vec![spawn_call(
                    "s1",
                    serde_json::json!({"task": "inspect", "profile": "explorer"}),
                )],
                FinishReason::ToolCalls,
            ),
            assistant_text("nothing"),
            assistant_text("parent done"),
        ],
    )
    .await;
    let contribution = events.iter().find_map(|e| match e {
        AgentEvent::SubAgentFinished { contribution, .. } => contribution.clone(),
        _ => None,
    });
    let p = contribution.expect("settled child must carry a projection");
    assert_eq!(p.profile_id.as_deref(), Some("explorer"));
    assert_eq!(p.profile_role.as_deref(), Some("explorer"));
    assert!(p.capabilities.iter().any(|c| c == "repository_analysis"));
    std::fs::remove_dir_all(&dir).ok();
}
