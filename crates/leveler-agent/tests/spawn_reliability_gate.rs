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
    /// Delay before each stream, so concurrent children genuinely overlap and
    /// a cancellation can land while they are in flight.
    delay: std::time::Duration,
    /// Called with the 0-based stream index before each response. Cancelling
    /// from here is deterministic; a wall-clock timer races the children.
    on_stream: Option<Arc<dyn Fn(usize) + Send + Sync>>,
    stream_count: std::sync::atomic::AtomicUsize,
}

impl ScriptedRuntime {
    fn new(responses: Vec<ModelResponse>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            delay: std::time::Duration::ZERO,
            on_stream: None,
            stream_count: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn with_delay(mut self, delay: std::time::Duration) -> Self {
        self.delay = delay;
        self
    }

    fn with_stream_hook(mut self, hook: impl Fn(usize) + Send + Sync + 'static) -> Self {
        self.on_stream = Some(Arc::new(hook));
        self
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
        let index = self
            .stream_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let Some(hook) = &self.on_stream {
            hook(index);
        }
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
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

/// EXPERIMENT 3b — parent cancellation with children in flight.
///
/// The gap the first pass of this gate reported. A cancelled parent must not
/// strand its children: every child that started has to reach a terminal, or
/// the log keeps asserting that stopped work is still running. That is exactly
/// the state session `446c71ad` left behind — two explorers `running` forever,
/// there because the turn died mid-flight and nobody spoke for them.
///
/// Cancellation fires from the stream hook rather than a timer: a wall-clock
/// race against three children is the kind of test that passes on a quiet
/// machine and fails in CI.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_the_parent_settles_every_child_in_flight() {
    let dir = tmp("cancel-fanout", 41);
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);

    let mut script = vec![assistant_with(
        (0..3)
            .map(|i| spawn_call(&format!("s{i}"), &format!("explore slice {i}")))
            .collect(),
        FinishReason::ToolCalls,
    )];
    for i in 0..3 {
        script.push(assistant_text(&format!("child {i} report")));
    }
    script.push(assistant_text("parent synthesis"));

    let token = CancellationToken::new();
    let cancel = token.clone();
    // Stream 0 is the parent's spawn round; 1.. are the children. Cancel once
    // the children are actually running.
    let runtime = Arc::new(
        ScriptedRuntime::new(script)
            .with_delay(std::time::Duration::from_millis(40))
            .with_stream_hook(move |index| {
                if index == 2 {
                    cancel.cancel();
                }
            }),
    );

    let lifecycle = Arc::new(Mutex::new(Lifecycle::default()));
    let sink = lifecycle.clone();
    let mut observer = move |event: AgentEvent| match event {
        AgentEvent::SubAgentStarted { id, .. } => sink.lock().unwrap().started.push(id),
        AgentEvent::SubAgentFinished { id, .. } => sink.lock().unwrap().finished.push(id),
        _ => {}
    };

    let result = Executor::new(
        runtime,
        Arc::new(default_registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .run(
        "spawn three, then cancel",
        &mut observer,
        &mut NoopSink,
        token,
    )
    .await;

    let life = lifecycle.lock().unwrap().clone();
    std::fs::remove_dir_all(&dir).ok();

    // Guard against passing for the wrong reason. If the cancellation landed
    // after every child had already reported, nothing about stranding was
    // tested — the run has to have actually been cancelled, with children
    // already started when it was.
    assert!(
        matches!(result, Err(leveler_agent::AgentError::Cancelled)),
        "cancellation never took effect, so nothing was tested: {result:?}"
    );
    assert!(
        !life.started.is_empty(),
        "the test is meaningless unless children actually started"
    );
    assert!(
        life.ghosts().is_empty(),
        "cancelling the parent stranded {} of {} children: {:?}",
        life.ghosts().len(),
        life.started.len(),
        life.ghosts()
    );
}

/// Contribution trace, end to end: a settled child's terminal event must carry
/// a projection naming that child, not just a prose preview.
///
/// This is the gap MA-VALUE-A ran into. That round could measure that
/// Multi-Agent scored 16 % higher and not why, because the scorer graded the
/// parent's prose — while 490 finding records sat in the ledger with
/// `source_child` on every one and nothing joined them to the outcome.
///
/// The assertion is deliberately about *presence and attribution*, not counts:
/// a scripted child reports no findings, so the counts are zero. Zero from a
/// child that ran is a different fact from `None`, which means "not measured",
/// and conflating them is how a child that contributed nothing would become
/// indistinguishable from a child nobody looked at.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_settled_child_reports_a_contribution_projection() {
    let dir = tmp("contribution", 77);
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);

    let script = vec![
        assistant_with(
            vec![spawn_call("s0", "look at the repository")],
            FinishReason::ToolCalls,
        ),
        assistant_text("child report"),
        assistant_text("parent synthesis"),
    ];

    let seen: Arc<Mutex<Vec<(String, Option<leveler_lifecycle::ChildResultProjection>)>>> =
        Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let mut observer = move |event: AgentEvent| {
        if let AgentEvent::SubAgentFinished {
            id, contribution, ..
        } = event
        {
            sink.lock().unwrap().push((id, contribution));
        }
    };

    let _ = Executor::new(
        Arc::new(ScriptedRuntime::new(script)),
        Arc::new(default_registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        8,
    )
    .run(
        "spawn one and settle it",
        &mut observer,
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await;

    let seen = seen.lock().unwrap().clone();
    std::fs::remove_dir_all(&dir).ok();

    assert_eq!(seen.len(), 1, "exactly one child settled");
    let (id, contribution) = &seen[0];
    let projection = contribution
        .as_ref()
        .expect("a settled child must carry a projection, not None");
    assert_eq!(
        &projection.child_id, id,
        "the projection must name the child it projects, or it cannot be joined"
    );
    assert_eq!(projection.role, "explorer");
    assert_eq!(projection.profile_id.as_deref(), Some("explorer"));
    assert_eq!(projection.profile_role.as_deref(), Some("explorer"));
    assert!(
        projection
            .capabilities
            .iter()
            .any(|c| c == "repository_analysis"),
        "explorer capabilities must appear on the projection: {:?}",
        projection.capabilities
    );
    assert!(
        !projection.contributed(),
        "this child reported nothing, and the projection must say so rather than flatter it"
    );
}

// ── MA-RT-4: spawn persistence (durable-before-execute) ──────────────────────

/// Order log entries shared by the observer, the event barrier and the model,
/// so a single sequence answers "was the start durable before the child ran".
type OrderLog = Arc<Mutex<Vec<String>>>;

struct RecordingBarrier {
    order: OrderLog,
}

#[async_trait]
impl leveler_agent::EventBarrier for RecordingBarrier {
    async fn flush(&self) -> Result<(), leveler_agent::AgentError> {
        self.order.lock().unwrap().push("flush".to_string());
        Ok(())
    }

    fn record_child_tool_event(&self, _event: leveler_agent::ChildToolEvent) {}
}

/// A barrier that reports durability failure once armed. Arming from the
/// observer at `SubAgentStarted` makes the SPAWN-path flush the first failing
/// one, so the test proves that seam specifically rather than an earlier
/// tool-dispatch flush.
struct ArmedFailingBarrier {
    armed: Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait]
impl leveler_agent::EventBarrier for ArmedFailingBarrier {
    async fn flush(&self) -> Result<(), leveler_agent::AgentError> {
        if self.armed.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(leveler_agent::AgentError::Persistence(
                "event log unavailable (injected)".to_string(),
            ));
        }
        Ok(())
    }

    fn record_child_tool_event(&self, _event: leveler_agent::ChildToolEvent) {}
}

/// MA_RT_STARTED_DURABLE_BEFORE_EXECUTE — the runtime must flush the batch's
/// `SubAgentStarted` events (make them durable) after emitting them and before
/// any accepted child begins executing. The sequence proves it: both starts,
/// then a flush, then the first child model stream.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_child_executes_before_its_start_event_is_flushed() {
    let dir = tmp("durable-before-execute", 71);
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);

    let script = vec![
        assistant_with(
            vec![
                spawn_call("s0", "explore slice 0"),
                spawn_call("s1", "explore slice 1"),
            ],
            FinishReason::ToolCalls,
        ),
        assistant_text("child 0 report"),
        assistant_text("child 1 report"),
        assistant_text("parent synthesis"),
    ];

    let order: OrderLog = Arc::new(Mutex::new(Vec::new()));
    let model_order = order.clone();
    let observer_order = order.clone();
    let mut observer = move |event: AgentEvent| {
        if let AgentEvent::SubAgentStarted { .. } = event {
            observer_order.lock().unwrap().push("started".to_string());
        }
    };

    let result = Executor::new(
        Arc::new(ScriptedRuntime::new(script).with_stream_hook(move |i| {
            model_order.lock().unwrap().push(format!("model{i}"));
        })),
        Arc::new(default_registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        8,
    )
    .with_event_barrier(Arc::new(RecordingBarrier {
        order: order.clone(),
    }))
    .run(
        "spawn and synthesise",
        &mut observer,
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await;
    std::fs::remove_dir_all(&dir).ok();
    result.expect("the fan-out run itself must succeed");

    let log = order.lock().unwrap().clone();
    let second_started = log
        .iter()
        .enumerate()
        .filter(|(_, e)| *e == "started")
        .nth(1)
        .map(|(i, _)| i)
        .expect("both children must be announced");
    let first_child_stream = log
        .iter()
        .position(|e| e == "model1")
        .expect("the children must actually run");
    let flush_between = log[second_started..first_child_stream]
        .iter()
        .any(|e| e == "flush");
    assert!(
        flush_between,
        "no barrier flush between the SubAgentStarted announcements and the first \
         child execution — a crash there leaves running children no durable record \
         can reconcile; sequence: {log:?}"
    );
}

/// MA_RT_STARTED_FLUSH_FAILURE_FAIL_CLOSED — if `SubAgentStarted` cannot become
/// durable, the accepted children must never begin executing and the run must
/// abort with the persistence error, not limp on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_start_that_cannot_become_durable_stops_the_children() {
    let dir = tmp("flush-fail-closed", 72);
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);

    let script = vec![
        assistant_with(
            vec![
                spawn_call("s0", "explore slice 0"),
                spawn_call("s1", "explore slice 1"),
            ],
            FinishReason::ToolCalls,
        ),
        assistant_text("child 0 report"),
        assistant_text("child 1 report"),
        assistant_text("parent synthesis"),
    ];

    let armed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let arm = armed.clone();
    let streams = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let stream_counter = streams.clone();
    let lifecycle = Arc::new(Mutex::new(Lifecycle::default()));
    let sink = lifecycle.clone();
    let mut observer = move |event: AgentEvent| match event {
        AgentEvent::SubAgentStarted { id, .. } => {
            arm.store(true, std::sync::atomic::Ordering::SeqCst);
            sink.lock().unwrap().started.push(id);
        }
        AgentEvent::SubAgentFinished { id, .. } => sink.lock().unwrap().finished.push(id),
        _ => {}
    };

    let result = Executor::new(
        Arc::new(ScriptedRuntime::new(script).with_stream_hook(move |_| {
            stream_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        })),
        Arc::new(default_registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        8,
    )
    .with_event_barrier(Arc::new(ArmedFailingBarrier { armed }))
    .run(
        "spawn and synthesise",
        &mut observer,
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await;
    std::fs::remove_dir_all(&dir).ok();

    assert!(
        matches!(result, Err(leveler_agent::AgentError::Persistence(_))),
        "an unflushable start must abort the run with the persistence error, got {result:?}"
    );
    let life = lifecycle.lock().unwrap().clone();
    assert_eq!(
        life.started.len(),
        2,
        "the starts were announced before the flush refused them"
    );
    assert!(
        life.finished.is_empty(),
        "no child may settle: none was allowed to begin"
    );
    assert_eq!(
        streams.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "only the parent's own model stream may have run — a child stream here \
         means a child executed without a durable start"
    );
}

// ── MA-RT-1: durable total child cap ─────────────────────────────────────────

/// MA_RT_TOTAL_CAP_ACROSS_TURNS + MA_RT_SETTLED_CHILD_COUNTS_TOTAL — the
/// total-delegation cap is a property of the task epoch, carried in the
/// durable `ProgressLedger`, so a second window seeded from the first one's
/// ledger starts with the quota already partly consumed — settled children
/// included. 4 spawned-and-settled in window one leaves room for exactly 2 of
/// the 3 asked for in window two.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_total_child_cap_survives_a_window_boundary() {
    let dir = tmp("total-cap-windows", 73);

    // Window one: spawn 4, all settle.
    let mut script = vec![assistant_with(
        (0..4)
            .map(|i| spawn_call(&format!("w1-{i}"), &format!("explore slice {i}")))
            .collect(),
        FinishReason::ToolCalls,
    )];
    for i in 0..4 {
        script.push(assistant_text(&format!("child {i} report")));
    }
    script.push(assistant_text("window one synthesis"));

    let carried: Arc<Mutex<Option<leveler_agent::ProgressLedger>>> = Arc::new(Mutex::new(None));
    let capture = carried.clone();
    let mut observer = move |event: AgentEvent| {
        if let AgentEvent::ProgressUpdated { ledger } = event {
            *capture.lock().unwrap() = Some(ledger);
        }
    };
    Executor::new(
        Arc::new(ScriptedRuntime::new(script)),
        Arc::new(default_registry()),
        ToolContext::new(Workspace::new(&dir).unwrap(), PermissionProfile::Assisted),
        ModelRef::new("mock", "m"),
        10,
    )
    .run(
        "window one",
        &mut observer,
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await
    .expect("window one must run");
    let progress = carried
        .lock()
        .unwrap()
        .clone()
        .expect("window one must persist a progress ledger");
    assert_eq!(
        progress.children_spawned_total, 4,
        "four accepted spawns must be durably counted, settled or not"
    );

    // Window two: seeded with window one's durable ledger, asks for 3 more.
    let mut script = vec![assistant_with(
        (0..3)
            .map(|i| spawn_call(&format!("w2-{i}"), &format!("explore more {i}")))
            .collect(),
        FinishReason::ToolCalls,
    )];
    for i in 0..2 {
        script.push(assistant_text(&format!("late child {i} report")));
    }
    script.push(assistant_text("window two synthesis"));

    let lifecycle = Arc::new(Mutex::new(Lifecycle::default()));
    let sink = lifecycle.clone();
    let mut observer = move |event: AgentEvent| match event {
        AgentEvent::SubAgentStarted { id, .. } => sink.lock().unwrap().started.push(id),
        AgentEvent::ToolResult {
            name,
            is_error: true,
            preview,
            ..
        } if name == "spawn_agent" => sink.lock().unwrap().refusals.push(preview),
        _ => {}
    };
    Executor::new(
        Arc::new(ScriptedRuntime::new(script)),
        Arc::new(default_registry()),
        ToolContext::new(Workspace::new(&dir).unwrap(), PermissionProfile::Assisted),
        ModelRef::new("mock", "m"),
        10,
    )
    .with_seeded_progress(progress)
    .run(
        "window two",
        &mut observer,
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await
    .expect("window two must run");
    std::fs::remove_dir_all(&dir).ok();

    let life = lifecycle.lock().unwrap().clone();
    assert_eq!(
        life.started.len(),
        2,
        "4 of 6 consumed in window one leaves exactly 2: {life:?}"
    );
    assert_eq!(life.refusals.len(), 1, "the 7th total must be refused");
    assert!(
        life.refusals[0].contains("limit reached"),
        "the refusal must name the limit, got: {}",
        life.refusals[0]
    );
}
