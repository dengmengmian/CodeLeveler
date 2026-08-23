//! Spawn Reliability Gate — how the Multi-Agent runtime behaves as fan-out and
//! event pressure rise.
//!
//! **Why these are deterministic tests and not model runs.** Delegation is
//! opportunity-based: the model elects to spawn, and forcing it is forbidden
//! (ROADMAP standing constraint 1, and constraint 5 — eval observes, it does not
//! special-case). The largest fan-out ever seen in the wild is three. So "run
//! the same task with eight children" is not something a real session can be
//! asked for; through a model it is a coin landing on eight.
//!
//! Reliability, though, is a property of the *runtime*, not of the model's
//! willingness. Scripting the fan-out here buys exact concurrency, free
//! repetitions, and results that model variance cannot confound. What a real
//! session is still needed for is the realistic baseline — completion, wall
//! time, tokens, child contribution — and that is measured separately.
//!
//! Gate definitions:
//!   PASS    no lost lifecycle event, no ghost child, no runtime failure
//!   WARNING correct but degraded
//!   FAIL    lost lifecycle event, ghost child, crash, or false completion

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

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

/// A scripted model: hands back one prepared response per request.
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
        // Running out mid-fan-out would look like a child hanging, so say what
        // actually happened rather than stalling.
        let response = self.responses.lock().unwrap().pop_front().ok_or_else(|| {
            ModelError::new(
                leveler_model::ModelErrorKind::Other,
                "scripted runtime exhausted: the script is shorter than the run",
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

fn spawn_call(id: &str, task: &str) -> ContentPart {
    ContentPart::ToolCall {
        call: ToolCall {
            id: ToolCallId::new(id),
            name: "spawn_agent".to_string(),
            arguments: serde_json::json!({
                "task": task,
                "role": "explorer",
                "run_in_background": false,
            }),
        },
    }
}

fn tmp(tag: &str, salt: u64) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("leveler-spawn-gate-{tag}-{salt}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Everything the gate judges, collected from the observer.
#[derive(Default, Debug, Clone)]
struct Lifecycle {
    started: Vec<String>,
    finished: Vec<String>,
    /// Spawns the runtime refused, with the reason it gave.
    refusals: Vec<String>,
}

impl Lifecycle {
    /// Children that started and never reported — the ghost class.
    fn ghosts(&self) -> Vec<&String> {
        self.started
            .iter()
            .filter(|id| !self.finished.contains(id))
            .collect()
    }

    /// Finishes with no matching start — a lifecycle event attributed to
    /// nothing, which would make the log unreadable in the other direction.
    fn orphans(&self) -> Vec<&String> {
        self.finished
            .iter()
            .filter(|id| !self.started.contains(id))
            .collect()
    }
}

/// Run one parent that spawns `n` children, each of which just answers.
async fn run_fan_out(n: usize, salt: u64) -> Lifecycle {
    let dir = tmp(&format!("fanout-{n}"), salt);
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);

    let mut script = vec![assistant_with(
        (0..n)
            .map(|i| spawn_call(&format!("s{i}"), &format!("explore slice {i}")))
            .collect(),
        FinishReason::ToolCalls,
    )];
    // One reply per child, then the parent's own close.
    for i in 0..n {
        script.push(assistant_text(&format!("child {i} report")));
    }
    script.push(assistant_text("parent synthesis"));

    let lifecycle = Arc::new(Mutex::new(Lifecycle::default()));
    let sink = lifecycle.clone();
    let mut observer = move |event: AgentEvent| match event {
        AgentEvent::SubAgentStarted { id, .. } => sink.lock().unwrap().started.push(id),
        AgentEvent::SubAgentFinished { id, .. } => sink.lock().unwrap().finished.push(id),
        AgentEvent::ToolResult {
            name,
            is_error: true,
            preview,
            ..
        } if name == "spawn_agent" => sink.lock().unwrap().refusals.push(preview),
        _ => {}
    };

    let _ = Executor::new(
        Arc::new(ScriptedRuntime::new(script)),
        Arc::new(default_registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        // Enough rounds for the fan-out plus the parent's close.
        (n + 4) as u32,
    )
    .run(
        "spawn and synthesise",
        &mut observer,
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await;

    std::fs::remove_dir_all(&dir).ok();
    lifecycle.lock().unwrap().clone()
}

/// EXPERIMENT 1 — fan-out scaling, and where the runtime says no.
///
/// The boundary is designed, not accidental: `DEFAULT_MAX_CONCURRENT_AGENTS`
/// is 4 and `DEFAULT_MAX_TOTAL_AGENTS` is 6. Asking for sixteen children was
/// never going to give sixteen, so the gate does not ask whether it does. It
/// asks the question that actually decides reliability:
///
///   below the cap  — every child starts and settles
///   above the cap  — the surplus is REFUSED, explicitly, and the refusal names
///                    the limit so the model can do the work itself
///   either way     — no ghost, no orphan
///
/// A cap enforced by silence would be the real defect: the parent would wait on
/// children that never existed. Session `446c71ad` is what waiting on a child
/// that never settles looks like.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fan_out_scales_to_the_cap_and_refuses_past_it() {
    const MAX_TOTAL: usize = 6;
    for width in [1usize, 2, 4, 6, 8, 16] {
        for rep in 0..5u64 {
            let life = run_fan_out(width, width as u64 * 100 + rep).await;
            let expected_started = width.min(MAX_TOTAL);
            assert_eq!(
                life.started.len(),
                expected_started,
                "width {width} rep {rep}: expected {expected_started} to start (cap {MAX_TOTAL})"
            );
            assert!(
                life.ghosts().is_empty(),
                "width {width} rep {rep}: children started and never settled: {:?}",
                life.ghosts()
            );
            assert!(
                life.orphans().is_empty(),
                "width {width} rep {rep}: finishes with no start: {:?}",
                life.orphans()
            );
            if width > MAX_TOTAL {
                assert_eq!(
                    life.refusals.len(),
                    width - MAX_TOTAL,
                    "width {width} rep {rep}: every surplus spawn must be refused, not dropped"
                );
                assert!(
                    life.refusals.iter().all(|r| r.contains("limit reached")),
                    "a refusal must say why: {:?}",
                    life.refusals
                );
            } else {
                assert!(
                    life.refusals.is_empty(),
                    "width {width} rep {rep}: nothing should be refused below the cap: {:?}",
                    life.refusals
                );
            }
        }
    }
}

/// EXPERIMENT 3a — a child that fails still settles.
///
/// A failure must arrive as a `SubAgentFinished`, not as silence. Silence is
/// what leaves a child `running` forever, and the parent then waits on
/// something that will never report.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_child_that_fails_still_reports_a_terminal() {
    let dir = tmp("child-failure", 7);
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);

    let script = vec![
        assistant_with(
            vec![
                spawn_call("s0", "this one works"),
                spawn_call("s1", "this one calls a tool that does not exist"),
            ],
            FinishReason::ToolCalls,
        ),
        assistant_text("child 0 report"),
        // Child 1 reaches for a tool an explorer does not have, then closes.
        assistant_with(
            vec![ContentPart::ToolCall {
                call: ToolCall {
                    id: ToolCallId::new("x1"),
                    name: "definitely_not_a_tool".to_string(),
                    arguments: serde_json::json!({}),
                },
            }],
            FinishReason::ToolCalls,
        ),
        assistant_text("child 1 gave up"),
        assistant_text("parent synthesis"),
    ];

    let lifecycle = Arc::new(Mutex::new(Lifecycle::default()));
    let sink = lifecycle.clone();
    let mut observer = move |event: AgentEvent| match event {
        AgentEvent::SubAgentStarted { id, .. } => sink.lock().unwrap().started.push(id),
        AgentEvent::SubAgentFinished { id, .. } => sink.lock().unwrap().finished.push(id),
        AgentEvent::ToolResult {
            name,
            is_error: true,
            preview,
            ..
        } if name == "spawn_agent" => sink.lock().unwrap().refusals.push(preview),
        _ => {}
    };

    let _ = Executor::new(
        Arc::new(ScriptedRuntime::new(script)),
        Arc::new(default_registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        12,
    )
    .run(
        "spawn two, one misbehaves",
        &mut observer,
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await;

    let life = lifecycle.lock().unwrap().clone();
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(life.started.len(), 2, "both children start");
    assert!(
        life.ghosts().is_empty(),
        "a child that misbehaves must still settle; ghosts: {:?}",
        life.ghosts()
    );
}

/// EXPERIMENT 2 — event pressure from a wide fan-out.
///
/// The failure this guards is not "slow" but "cancelled": a canonical event
/// arriving at a full channel kills the run. Here the concern is the agent
/// layer's own accounting under the widest fan-out the gate covers — every
/// child accounted for, no lifecycle event lost in the crowd.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wide_fan_out_loses_no_lifecycle_event() {
    let life = run_fan_out(16, 9_999).await;
    assert_eq!(
        life.started.len(),
        6,
        "the total cap holds under the widest ask"
    );
    assert_eq!(
        life.finished.len(),
        6,
        "every child that started produces exactly one terminal; started={:?} finished={:?}",
        life.started,
        life.finished
    );
    let mut unique = life.finished.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(
        unique.len(),
        6,
        "a child must not report twice: {:?}",
        life.finished
    );
    assert_eq!(
        life.refusals.len(),
        10,
        "the ten surplus spawns are each refused explicitly"
    );
}
