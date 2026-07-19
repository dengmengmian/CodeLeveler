//! Offline eval smoke: load a real `evals/smoke` case, drive the real agent
//! loop with a scripted mock model, and run the case's own expect command.
//! No network, no API key — this is the CI-safe canary that keeps the eval
//! case format, the loop's edit path, and the verification command working
//! together. (Real-model evals stay manual: `leveler eval run`.)

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use leveler_agent::{Executor, NoopSink};
use leveler_core::{RequestId, ToolCallId};
use leveler_execution::{PermissionProfile, Workspace};
use leveler_model::{
    ContentPart, FinishReason, Message, ModelError, ModelEventStream, ModelProfile, ModelRef,
    ModelRequest, ModelResponse, ModelRuntime, Role, TokenUsage, ToolCall,
};
use leveler_tools::{ToolContext, default_registry};

struct MockRuntime {
    responses: Mutex<VecDeque<ModelResponse>>,
}

#[async_trait]
impl ModelRuntime for MockRuntime {
    async fn generate(
        &self,
        _request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        self.responses.lock().unwrap().pop_front().ok_or_else(|| {
            ModelError::new(leveler_model::ModelErrorKind::Other, "no more responses")
        })
    }

    async fn stream(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        use leveler_model::ModelEvent;
        let response = self.generate(request, cancellation).await?;
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
                    events.push(Ok(ModelEvent::ToolCallCompleted { call: call.clone() }));
                }
                _ => {}
            }
        }
        events.push(Ok(ModelEvent::MessageCompleted {
            finish_reason: response.finish_reason,
        }));
        Ok(Box::pin(futures::stream::iter(events)))
    }

    async fn profile(&self, _m: &ModelRef) -> Result<ModelProfile, ModelError> {
        unimplemented!("smoke test drives the executor directly")
    }
}

fn tool_call(id: &str, name: &str, args: serde_json::Value) -> ModelResponse {
    ModelResponse {
        request_id: RequestId::generate(),
        message: Message {
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

fn text(t: &str) -> ModelResponse {
    ModelResponse {
        request_id: RequestId::generate(),
        message: Message::text(Role::Assistant, t),
        finish_reason: FinishReason::Stop,
        usage: TokenUsage::default(),
    }
}

#[tokio::test]
async fn smoke_case_runs_offline_end_to_end() {
    // 1. Load the real committed case — its format must stay parseable.
    let case_path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../evals/smoke/rust-mul.yaml");
    let case = leveler_eval::EvaluationCase::load(&case_path).expect("smoke case must parse");
    assert_eq!(case.id, "rust-mul");

    // 2. Materialize its workspace.
    let dir = std::env::temp_dir().join(format!("leveler-smoke-offline-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for (rel, content) in &case.files {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, content).unwrap();
    }

    // 3. Drive the REAL loop (registry, workspace, patch engine) with a
    //    scripted model that solves the task.
    let patch = "*** Begin Patch\n*** Update File: lib.rs\n pub fn add(a: i32, b: i32) -> i32 {\n     a + b\n }\n+\n+pub fn mul(a: i32, b: i32) -> i32 {\n+    a * b\n+}\n+\n+#[cfg(test)]\n+mod tests {\n+    #[test]\n+    fn mul_works() {\n+        assert_eq!(super::mul(3, 4), 12);\n+    }\n+}\n*** End Patch";
    let runtime = Arc::new(MockRuntime {
        responses: Mutex::new(VecDeque::from(vec![
            tool_call("c1", "apply_patch", serde_json::json!({ "patch": patch })),
            text("Added mul with a unit test."),
        ])),
    });
    let workspace = Workspace::new(&dir).unwrap();
    let outcome = Executor::new(
        runtime,
        Arc::new(default_registry()),
        ToolContext::new(workspace, PermissionProfile::Assisted),
        ModelRef::new("mock", "m"),
        10,
    )
    .run(
        &case.task,
        &mut |_| {},
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await
    .expect("the scripted run must finish");
    assert!(
        outcome.modified_files.contains(&"lib.rs".to_string()),
        "the case edit must land: {outcome:?}"
    );

    // 4. The case's own expect command is the pass/fail oracle.
    let expect = &case.expect;
    let status = std::process::Command::new(&expect.program)
        .args(&expect.args)
        .current_dir(&dir)
        .status()
        .expect("expect command must be runnable (cargo is on PATH in CI)");
    assert!(
        status.success(),
        "the case's expect command must pass after the scripted solution"
    );

    std::fs::remove_dir_all(&dir).ok();
}
