//! Multi-agent (CC-style star delegation): the parent spawns focused sub-agents
//! via the `spawn_agent` tool. Multiple spawns in one round run CONCURRENTLY;
//! roles restrict a sub-agent's tools; workers are pinned to owned files.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use leveler_agent::{AgentEvent, Executor, NoopSink, StopReason};
use leveler_core::{RequestId, ToolCallId};
use leveler_execution::{PermissionProfile, Workspace};
use leveler_model::{
    ContentPart, FinishReason, Message, ModelError, ModelEventStream, ModelProfile, ModelRef,
    ModelRequest, ModelResponse, ModelRuntime, Role, TokenUsage, ToolCall,
};
use leveler_tools::{ToolContext, default_registry};

/// Replays scripted responses in order; each `stream` sleeps either the next
/// staged delay or the default delay, so concurrent sub-agents can overlap.
struct SleepyRuntime {
    responses: Mutex<VecDeque<ModelResponse>>,
    /// Per-stream delays (front is next). When empty, `default_delay` is used.
    delays: Mutex<VecDeque<Duration>>,
    default_delay: Duration,
    /// Invoked with the 0-based stream index at the start of each `stream`
    /// call, so tests can time cancellation deterministically.
    on_stream: Option<Arc<dyn Fn(usize) + Send + Sync>>,
    stream_count: std::sync::atomic::AtomicUsize,
}

impl SleepyRuntime {
    fn new(responses: Vec<ModelResponse>, delay: Duration) -> Self {
        Self {
            responses: Mutex::new(VecDeque::from(responses)),
            delays: Mutex::new(VecDeque::new()),
            default_delay: delay,
            on_stream: None,
            stream_count: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Per-response delays in stream order (extra responses use `default_delay`).
    fn with_delays(mut self, delays: Vec<Duration>) -> Self {
        self.delays = Mutex::new(VecDeque::from(delays));
        self
    }

    /// Hook invoked at the start of each `stream` call with its 0-based index.
    fn with_stream_hook(mut self, hook: impl Fn(usize) + Send + Sync + 'static) -> Self {
        self.on_stream = Some(Arc::new(hook));
        self
    }
}

#[async_trait]
impl ModelRuntime for SleepyRuntime {
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
        let delay = self
            .delays
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(self.default_delay);
        tokio::time::sleep(delay).await;
        let response = self.responses.lock().unwrap().pop_front().ok_or_else(|| {
            ModelError::new(leveler_model::ModelErrorKind::Other, "no more responses")
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
        if response.usage.total() > 0 {
            events.push(Ok(ModelEvent::UsageUpdated {
                usage: response.usage,
            }));
        }
        events.push(Ok(ModelEvent::MessageCompleted {
            finish_reason: response.finish_reason,
        }));
        Ok(Box::pin(futures::stream::iter(events)))
    }

    async fn profile(&self, _m: &ModelRef) -> Result<ModelProfile, ModelError> {
        unimplemented!()
    }
}

fn spawn_call(id: &str, args: serde_json::Value) -> ContentPart {
    ContentPart::ToolCall {
        call: ToolCall {
            id: ToolCallId::new(id),
            name: "spawn_agent".to_string(),
            arguments: args,
        },
    }
}

fn tool_call_part(id: &str, name: &str, args: serde_json::Value) -> ContentPart {
    ContentPart::ToolCall {
        call: ToolCall {
            id: ToolCallId::new(id),
            name: name.to_string(),
            arguments: args,
        },
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

fn assistant_text_with_usage(text: &str, usage: TokenUsage) -> ModelResponse {
    ModelResponse {
        request_id: RequestId::generate(),
        message: Message::text(Role::Assistant, text),
        finish_reason: FinishReason::Stop,
        usage,
    }
}

fn tmp(tag: &str, salt: u64) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "leveler-multiagent-{tag}-{}",
        std::process::id() as u64 * 101 + salt
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test]
async fn multiple_spawns_in_one_round_run_concurrently() {
    let dir = tmp("concurrent", 1);
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    // Parent emits TWO spawn calls in one assistant round, then finishes.
    // Each sub-agent answers in one round. With a 120ms per-stream delay,
    // 4 model calls run serially in ~480ms but concurrently in ~240ms.
    let runtime = Arc::new(SleepyRuntime::new(
        vec![
            assistant_with(
                vec![
                    spawn_call("s1", serde_json::json!({"task": "investigate module A"})),
                    spawn_call("s2", serde_json::json!({"task": "investigate module B"})),
                ],
                FinishReason::ToolCalls,
            ),
            assistant_text("sub A report"),
            assistant_text("sub B report"),
            assistant_text("Synthesized both reports."),
        ],
        Duration::from_millis(120),
    ));

    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    );

    let mut events = Vec::new();
    let started = Instant::now();
    let outcome = executor
        .run(
            "delegate two investigations",
            &mut |e| events.push(e),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let elapsed = started.elapsed();

    assert_eq!(outcome.final_text, "Synthesized both reports.");

    // Both sub-agents started before either finished (batch concurrency).
    let started_n = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::SubAgentStarted { .. }))
        .count();
    let finished_n = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::SubAgentFinished { .. }))
        .count();
    assert_eq!(started_n, 2, "both sub-agents should emit Started");
    assert_eq!(finished_n, 2, "both sub-agents should emit Finished");

    // Wall-clock proves concurrency: parent(1) + max(childA, childB)(1) + parent(1)
    // ≈ 3 × 120ms = 360ms, well under the serial 4 × 120ms = 480ms.
    assert!(
        elapsed < Duration::from_millis(440),
        "two spawns should overlap; took {elapsed:?}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn sub_agent_reports_active_state_and_its_own_cumulative_usage() {
    let dir = tmp("progress", 71);
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());
    let mut first_child_round = read_call("progress-read");
    first_child_round.usage = TokenUsage {
        input_tokens: 700,
        output_tokens: 30,
        cached_input_tokens: 300,
    };
    let runtime = Arc::new(SleepyRuntime::new(
        vec![
            assistant_with(
                vec![spawn_call(
                    "s1",
                    serde_json::json!({"task": "inspect providers", "role": "explorer"}),
                )],
                FinishReason::ToolCalls,
            ),
            first_child_round,
            assistant_text_with_usage(
                "provider report",
                TokenUsage {
                    input_tokens: 1_200,
                    output_tokens: 80,
                    cached_input_tokens: 600,
                },
            ),
            assistant_text("parent done"),
        ],
        Duration::from_millis(0),
    ));
    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        0,
    );

    let mut events = Vec::new();
    executor
        .run(
            "delegate provider inspection",
            &mut |event| events.push(event),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert!(
        events.iter().any(|event| {
            matches!(event, AgentEvent::SubAgentProgress {
            id,
            active: true,
            input_tokens: 1_900,
            output_tokens: 110,
            cached_input_tokens: 900,
        } if id == "agent-1")
        }),
        "per-agent progress must bubble while it is executing: {events:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn explorer_role_cannot_modify_files() {
    let dir = tmp("explorer", 2);
    std::fs::write(dir.join("lib.rs"), "pub fn old() {}\n").unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    // The explorer sub-agent tries to apply_patch, which is not in its toolset.
    let runtime = Arc::new(SleepyRuntime::new(
        vec![
            assistant_with(
                vec![spawn_call(
                    "s1",
                    serde_json::json!({"task": "look and try to edit", "role": "explorer"}),
                )],
                FinishReason::ToolCalls,
            ),
            // Child round 1: attempt a forbidden edit.
            assistant_with(
                vec![tool_call_part(
                    "c1",
                    "apply_patch",
                    serde_json::json!({
                        "patch": "*** Begin Patch\n*** Update File: lib.rs\n pub fn old() {}\n+pub fn added() {}\n*** End Patch"
                    }),
                )],
                FinishReason::ToolCalls,
            ),
            // Child round 2: give up and report.
            assistant_text("I could not edit; read-only."),
            // Parent finishes.
            assistant_text("Explorer done."),
        ],
        Duration::from_millis(0),
    ));

    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    );
    executor
        .run(
            "explore read-only",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    // The explorer had no write tool, so the file is untouched.
    let content = std::fs::read_to_string(dir.join("lib.rs")).unwrap();
    assert_eq!(
        content, "pub fn old() {}\n",
        "explorer must not modify files"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn worker_ownership_rejects_out_of_scope_edit() {
    let dir = tmp("worker", 3);
    std::fs::write(dir.join("a.rs"), "pub fn a() {}\n").unwrap();
    std::fs::write(dir.join("b.rs"), "pub fn b() {}\n").unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    // Worker owns only a.rs but tries to patch b.rs.
    let runtime = Arc::new(SleepyRuntime::new(
        vec![
            assistant_with(
                vec![spawn_call(
                    "s1",
                    serde_json::json!({
                        "task": "edit files",
                        "role": "worker",
                        "files": ["a.rs"]
                    }),
                )],
                FinishReason::ToolCalls,
            ),
            // Child tries to edit b.rs (out of scope).
            assistant_with(
                vec![tool_call_part(
                    "c1",
                    "apply_patch",
                    serde_json::json!({
                        "patch": "*** Begin Patch\n*** Update File: b.rs\n pub fn b() {}\n+pub fn hacked() {}\n*** End Patch"
                    }),
                )],
                FinishReason::ToolCalls,
            ),
            assistant_text("Blocked from editing b.rs."),
            assistant_text("Worker done."),
        ],
        Duration::from_millis(0),
    ));

    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    );
    executor
        .run(
            "delegate to a worker",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    // b.rs is outside the worker's ownership → the edit was rejected.
    let b = std::fs::read_to_string(dir.join("b.rs")).unwrap();
    assert_eq!(b, "pub fn b() {}\n", "out-of-scope edit must be rejected");

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn explorer_can_finish_after_more_than_six_rounds() {
    let dir = tmp("explorer-budget", 31);
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    let mut responses = vec![assistant_with(
        vec![spawn_call(
            "s1",
            serde_json::json!({"task": "inspect thoroughly", "role": "explorer"}),
        )],
        FinishReason::ToolCalls,
    )];
    responses.extend((1..=7).map(|round| read_call(&format!("c{round}"))));
    responses.push(assistant_text("Explorer finished after seven tool rounds."));
    responses.push(assistant_text("Parent received the exploration result."));
    let runtime = Arc::new(SleepyRuntime::new(responses, Duration::from_millis(0)));
    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        2,
    )
    .with_continuation_policy(leveler_agent::ContinuationPolicy::UntilTerminal);

    let mut events = Vec::new();
    let outcome = executor
        .run(
            "delegate thorough exploration",
            &mut |event| events.push(event),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(outcome.stop_reason, StopReason::Answered);
    let (ok, summary) = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::SubAgentFinished { ok, summary, .. } => Some((*ok, summary.as_str())),
            _ => None,
        })
        .expect("sub-agent finish event");
    assert!(ok, "an explorer must be allowed to finish after round six");
    assert!(
        summary.contains("finished after seven tool rounds"),
        "actual summary: {summary}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn worker_can_finish_after_more_than_twelve_rounds() {
    let dir = tmp("worker-budget", 37);
    std::fs::write(dir.join("owned.rs"), "pub fn owned() {}\n").unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    let mut responses = vec![assistant_with(
        vec![spawn_call(
            "s1",
            serde_json::json!({
                "task": "finish the owned work",
                "role": "worker",
                "files": ["owned.rs"]
            }),
        )],
        FinishReason::ToolCalls,
    )];
    responses.extend((1..=13).map(|round| read_call(&format!("w{round}"))));
    responses.push(assistant_text(
        "Worker finished after thirteen tool rounds.",
    ));
    responses.push(assistant_text("Parent received the worker result."));
    let runtime = Arc::new(SleepyRuntime::new(responses, Duration::from_millis(0)));
    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        2,
    )
    .with_continuation_policy(leveler_agent::ContinuationPolicy::UntilTerminal);

    let mut events = Vec::new();
    executor
        .run(
            "delegate owned work",
            &mut |event| events.push(event),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let (ok, summary) = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::SubAgentFinished { ok, summary, .. } => Some((*ok, summary.as_str())),
            _ => None,
        })
        .expect("sub-agent finish event");
    assert!(ok, "a worker must be allowed to finish after round twelve");
    assert!(
        summary.contains("finished after thirteen tool rounds"),
        "actual summary: {summary}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn sub_agent_events_carry_nickname_and_task() {
    let dir = tmp("events", 4);
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    let runtime = Arc::new(SleepyRuntime::new(
        vec![
            assistant_with(
                vec![spawn_call(
                    "s1",
                    serde_json::json!({"task": "count the crates"}),
                )],
                FinishReason::ToolCalls,
            ),
            assistant_text("There are 12 crates."),
            assistant_text("Reported."),
        ],
        Duration::from_millis(0),
    ));

    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    );
    let mut events = Vec::new();
    executor
        .run(
            "count crates",
            &mut |e| events.push(e),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let started = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::SubAgentStarted { nickname, task, .. } => {
                Some((nickname.clone(), task.clone()))
            }
            _ => None,
        })
        .expect("a SubAgentStarted event");
    assert!(!started.0.is_empty(), "sub-agent has a nickname");
    assert!(started.1.contains("count the crates"), "carries the task");

    let finished = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::SubAgentFinished {
                nickname,
                ok,
                summary,
                ..
            } => Some((nickname.clone(), *ok, summary.clone())),
            _ => None,
        })
        .expect("a SubAgentFinished event");
    assert_eq!(finished.0, started.0, "same nickname across start/finish");
    assert!(finished.1, "sub-agent succeeded");
    assert!(
        finished.2.contains("12 crates"),
        "summary carries the result"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn total_agent_cap_rejects_excess_spawns() {
    let dir = tmp("cap", 5);
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    // Three spawns in one round, but the cap is 2.
    let runtime = Arc::new(SleepyRuntime::new(
        vec![
            assistant_with(
                vec![
                    spawn_call("s1", serde_json::json!({"task": "task one"})),
                    spawn_call("s2", serde_json::json!({"task": "task two"})),
                    spawn_call("s3", serde_json::json!({"task": "task three"})),
                ],
                FinishReason::ToolCalls,
            ),
            assistant_text("one done"),
            assistant_text("two done"),
            assistant_text("Parent wrap-up."),
        ],
        Duration::from_millis(0),
    ));

    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .with_agents(4, 2);

    let mut events = Vec::new();
    executor
        .run(
            "spawn three",
            &mut |e| events.push(e),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let started_n = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::SubAgentStarted { .. }))
        .count();
    assert_eq!(
        started_n, 2,
        "only two sub-agents may start under a cap of 2"
    );

    std::fs::remove_dir_all(&dir).ok();
}

fn read_call(id: &str) -> ModelResponse {
    // Distinct path per round so identical-observe thrash does not cut
    // UntilTerminal multi-round exploration tests short.
    ModelResponse {
        request_id: RequestId::generate(),
        message: Message {
            role: Role::Assistant,
            content: vec![tool_call_part(
                id,
                "list_files",
                serde_json::json!({"path": format!("./{id}")}),
            )],
        },
        finish_reason: FinishReason::ToolCalls,
        usage: TokenUsage::default(),
    }
}

#[tokio::test]
async fn until_terminal_run_is_not_cut_off_by_a_round_budget() {
    let dir = tmp("until-terminal", 5);
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    // `0` is the compatibility spelling for the new unbounded top-level
    // continuation mode. The run must reach the model's natural terminal
    // response instead of clamping the budget to one round.
    let runtime = Arc::new(SleepyRuntime::new(
        vec![
            read_call("c1"),
            read_call("c2"),
            read_call("c3"),
            ModelResponse {
                request_id: RequestId::generate(),
                message: Message::text(Role::Assistant, "Task complete."),
                finish_reason: FinishReason::Stop,
                usage: TokenUsage::default(),
            },
        ],
        Duration::from_millis(0),
    ));
    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        0,
    );

    let outcome = executor
        .run(
            "keep working until terminal",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(outcome.stop_reason, StopReason::Answered);
    assert_eq!(outcome.rounds, 4);
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn bounded_run_stops_at_the_round_budget() {
    let dir = tmp("nopersist", 6);
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    // A delegated/measured unit remains bounded even though top-level turns are
    // not. The model keeps calling a tool and must stop at exactly two rounds.
    let runtime = Arc::new(SleepyRuntime::new(
        vec![read_call("c1"), read_call("c2"), read_call("c3")],
        Duration::from_millis(0),
    ));
    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        2,
    );
    let outcome = executor
        .run(
            "keep working",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(outcome.stop_reason, StopReason::BudgetExhausted);
    assert_eq!(outcome.rounds, 2);

    std::fs::remove_dir_all(&dir).ok();
}

/// A parallel-safe tool that records how many executions overlap, so the
/// execution knob `max_parallel_tools` is observable.
struct GaugedTool {
    current: Arc<std::sync::atomic::AtomicUsize>,
    max_seen: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait]
impl leveler_tools::Tool for GaugedTool {
    fn name(&self) -> &'static str {
        "gauged_read"
    }
    fn description(&self) -> &'static str {
        "test-only read that measures concurrency"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {"n": {"type": "integer"}}})
    }
    fn risk(&self) -> leveler_tools::RiskLevel {
        leveler_tools::RiskLevel::Safe
    }
    fn supports_parallel(&self) -> bool {
        true
    }
    async fn execute(
        &self,
        _input: serde_json::Value,
        _context: ToolContext,
        _cancellation: CancellationToken,
    ) -> Result<leveler_tools::ToolOutput, leveler_tools::ToolError> {
        use std::sync::atomic::Ordering;
        let now = self.current.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_seen.fetch_max(now, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(80)).await;
        self.current.fetch_sub(1, Ordering::SeqCst);
        Ok(leveler_tools::ToolOutput::ok("ok"))
    }
}

/// One round issues three parallel-safe calls; runs the round once unbounded
/// and once with `max_parallel_tools = 1`, asserting the observed overlap.
async fn run_gauged_round(max_parallel_tools: usize) -> usize {
    let dir = tmp("gauge", max_parallel_tools as u64);
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);

    let current = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let max_seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut registry = default_registry();
    registry.register(Arc::new(GaugedTool {
        current: current.clone(),
        max_seen: max_seen.clone(),
    }));

    let runtime = Arc::new(SleepyRuntime::new(
        vec![
            assistant_with(
                vec![
                    tool_call_part("g1", "gauged_read", serde_json::json!({"n": 1})),
                    tool_call_part("g2", "gauged_read", serde_json::json!({"n": 2})),
                    tool_call_part("g3", "gauged_read", serde_json::json!({"n": 3})),
                ],
                FinishReason::ToolCalls,
            ),
            assistant_text("all read"),
        ],
        Duration::from_millis(1),
    ));

    let executor = Executor::new(
        runtime,
        Arc::new(registry),
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .with_execution_controls(0, 0, max_parallel_tools);

    let outcome = executor
        .run(
            "read three things",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(outcome.final_text, "all read");

    std::fs::remove_dir_all(&dir).ok();
    max_seen.load(std::sync::atomic::Ordering::SeqCst)
}

#[tokio::test]
async fn max_parallel_tools_bounds_the_readonly_batch() {
    // Unbounded (0): all three overlap.
    assert_eq!(run_gauged_round(0).await, 3, "0 = unbounded, full overlap");
    // Leveled to 1: strictly serial even though the tools are parallel-safe.
    assert_eq!(run_gauged_round(1).await, 1, "cap of 1 must serialize");
}

#[tokio::test]
async fn worker_sub_agent_serializes_parallel_safe_tools() {
    let dir = tmp("worker-serial", 91);
    std::fs::write(dir.join("owned.rs"), "pub fn owned() {}\n").unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);

    let current = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let max_seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut registry = default_registry();
    registry.register(Arc::new(GaugedTool {
        current,
        max_seen: max_seen.clone(),
    }));

    let runtime = Arc::new(SleepyRuntime::new(
        vec![
            assistant_with(
                vec![spawn_call(
                    "s1",
                    serde_json::json!({
                        "task": "inspect before editing",
                        "role": "worker",
                        "files": ["owned.rs"]
                    }),
                )],
                FinishReason::ToolCalls,
            ),
            assistant_with(
                vec![
                    tool_call_part("g1", "gauged_read", serde_json::json!({"n": 1})),
                    tool_call_part("g2", "gauged_read", serde_json::json!({"n": 2})),
                    tool_call_part("g3", "gauged_read", serde_json::json!({"n": 3})),
                ],
                FinishReason::ToolCalls,
            ),
            assistant_text("worker inspected serially"),
            assistant_text("parent received worker report"),
        ],
        Duration::from_millis(1),
    ));

    let outcome = Executor::new(
        runtime,
        Arc::new(registry),
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .with_execution_controls(0, 0, 4)
    .run(
        "delegate a write task",
        &mut |_| {},
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await
    .unwrap();

    assert_eq!(outcome.final_text, "parent received worker report");
    assert_eq!(max_seen.load(std::sync::atomic::Ordering::SeqCst), 1);
    std::fs::remove_dir_all(&dir).ok();
}


/// Blocker 1: exhausted parent residual (`Some(0)`) must hard-block child
/// commands — never reopen unlimited via `0 == unlimited` confusion.
#[tokio::test]
async fn exhausted_parent_command_budget_hard_blocks_child() {
    let dir = tmp("exhausted-child", 201);
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);

    let runtime = Arc::new(SleepyRuntime::new(
        vec![
            // Parent spends the only command slot, then spawns a child.
            assistant_with(
                vec![tool_call_part(
                    "p1",
                    "run_command",
                    serde_json::json!({"program": "echo", "args": ["parent"]}),
                )],
                FinishReason::ToolCalls,
            ),
            assistant_with(
                vec![spawn_call(
                    "s1",
                    serde_json::json!({"task": "run a command", "role": "explorer"}),
                )],
                FinishReason::ToolCalls,
            ),
            // Child tries a command — residual is Some(0), must be refused.
            assistant_with(
                vec![tool_call_part(
                    "c1",
                    "run_command",
                    serde_json::json!({"program": "echo", "args": ["child"]}),
                )],
                FinishReason::ToolCalls,
            ),
            assistant_text("child finished under budget"),
            assistant_text("parent done"),
        ],
        Duration::from_millis(0),
    ));

    let outcome = Executor::new(
        runtime,
        Arc::new(default_registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .with_step_limits(leveler_agent::StepLimits {
        max_commands: Some(1),
        ..Default::default()
    })
    .run(
        "one command then spawn",
        &mut |_| {},
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await
    .unwrap();

    // Parent used 1; child must not add another successful shell execution.
    assert_eq!(
        outcome.progress.cumulative_commands, 1,
        "child must not run commands when parent residual is Some(0); progress={:?}",
        outcome.progress
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Blocker 2: two parallel children must not each receive the full residual
/// (oversell). Parent max_commands=2 → each child gets 1; total ≤ 2.
#[tokio::test]
async fn parallel_children_do_not_oversell_command_budget() {
    let dir = tmp("parallel-budget", 202);
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);

    let runtime = Arc::new(SleepyRuntime::new(
        vec![
            assistant_with(
                vec![
                    spawn_call(
                        "s1",
                        serde_json::json!({"task": "run two echoes A", "role": "explorer"}),
                    ),
                    spawn_call(
                        "s2",
                        serde_json::json!({"task": "run two echoes B", "role": "explorer"}),
                    ),
                ],
                FinishReason::ToolCalls,
            ),
            // Child A: try 2 commands (only 1 residual share allowed).
            assistant_with(
                vec![
                    tool_call_part(
                        "a1",
                        "run_command",
                        serde_json::json!({"program": "echo", "args": ["a1"]}),
                    ),
                    tool_call_part(
                        "a2",
                        "run_command",
                        serde_json::json!({"program": "echo", "args": ["a2"]}),
                    ),
                ],
                FinishReason::ToolCalls,
            ),
            assistant_text("A done"),
            // Child B: same.
            assistant_with(
                vec![
                    tool_call_part(
                        "b1",
                        "run_command",
                        serde_json::json!({"program": "echo", "args": ["b1"]}),
                    ),
                    tool_call_part(
                        "b2",
                        "run_command",
                        serde_json::json!({"program": "echo", "args": ["b2"]}),
                    ),
                ],
                FinishReason::ToolCalls,
            ),
            assistant_text("B done"),
            assistant_text("parent done"),
        ],
        Duration::from_millis(0),
    ));

    let outcome = Executor::new(
        runtime,
        Arc::new(default_registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .with_step_limits(leveler_agent::StepLimits {
        max_commands: Some(2),
        ..Default::default()
    })
    .run(
        "spawn two",
        &mut |_| {},
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await
    .unwrap();

    assert!(
        outcome.progress.cumulative_commands <= 2,
        "parallel children oversold parent residual: commands={} progress={:?}",
        outcome.progress.cumulative_commands,
        outcome.progress
    );
    // Each share is 1; both should get to run their first command.
    assert_eq!(
        outcome.progress.cumulative_commands, 2,
        "each child should use its 1-command share"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Blocker 3: cancel after a child spent a command must still surface that
/// spend via ProgressUpdated (not a silent empty ledger).
#[tokio::test]
async fn cancel_after_child_spend_still_flushes_ledger() {
    let dir = tmp("cancel-spend", 203);
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);

    let token = CancellationToken::new();
    let cancel = token.clone();
    let runtime = Arc::new(
        SleepyRuntime::new(
            vec![
                assistant_with(
                    vec![spawn_call(
                        "s1",
                        serde_json::json!({"task": "run then hang", "role": "explorer"}),
                    )],
                    FinishReason::ToolCalls,
                ),
                // Child: one command, then a slow model round so cancel can land.
                assistant_with(
                    vec![tool_call_part(
                        "c1",
                        "run_command",
                        serde_json::json!({"program": "echo", "args": ["spent"]}),
                    )],
                    FinishReason::ToolCalls,
                ),
                // Slow second child model call — parent cancel fires during this.
                assistant_text("child still going"),
                assistant_text("parent should not reach here cleanly"),
            ],
            Duration::from_millis(40),
        )
        // Streams: 0 = parent spawn, 1 = child run_command (its spend commits
        // before the next stream is requested), 2 = the child's following
        // round. Cancel on stream 2 deterministically — a wall-clock timer
        // races the child's command on loaded runners.
        .with_stream_hook(move |index| {
            if index == 2 {
                cancel.cancel();
            }
        }),
    );

    let mut events = Vec::new();
    let result = Executor::new(
        runtime,
        Arc::new(default_registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .run(
        "spawn and cancel",
        &mut |e| events.push(e),
        &mut NoopSink,
        token,
    )
    .await;

    // Cancelled is fine — but spend must have been flushed into ProgressUpdated.
    let max_commands = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ProgressUpdated { ledger } => Some(ledger.cumulative_commands),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    assert!(
        max_commands >= 1,
        "cancel must not drop child command spend; max_commands={max_commands} result={result:?} events={events:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Mixed tool batch: parent `run_command` + `spawn_agent` in the **same**
/// assistant response. Parent local command must not be dropped when child
/// spend is absorbed (ledger lag overwrite).
#[tokio::test]
async fn mixed_parent_command_and_child_both_count_in_same_batch() {
    let dir = tmp("mixed-batch", 205);
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);

    let runtime = Arc::new(SleepyRuntime::new(
        vec![
            // One assistant message: parent shell + spawn child.
            assistant_with(
                vec![
                    tool_call_part(
                        "p1",
                        "run_command",
                        serde_json::json!({"program": "echo", "args": ["parent"]}),
                    ),
                    spawn_call(
                        "s1",
                        serde_json::json!({"task": "run one command", "role": "explorer"}),
                    ),
                ],
                FinishReason::ToolCalls,
            ),
            // Child: one shell.
            assistant_with(
                vec![tool_call_part(
                    "c1",
                    "run_command",
                    serde_json::json!({"program": "echo", "args": ["child"]}),
                )],
                FinishReason::ToolCalls,
            ),
            assistant_text("child done"),
            assistant_text("parent done"),
        ],
        Duration::from_millis(0),
    ));

    let outcome = Executor::new(
        runtime,
        Arc::new(default_registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .run(
        "mixed batch",
        &mut |_| {},
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await
    .unwrap();

    assert_eq!(
        outcome.progress.cumulative_commands, 2,
        "same-batch parent shell + child shell must both persist; progress={:?}",
        outcome.progress
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Queued child (concurrency=1) must refresh duration residual after waiting
/// for a permit — not keep a pre-queue residual past the parent deadline.
///
/// Timing (true queue): parent stream is fast so residual > 0 at spawn;
/// child A holds the only permit past the parent wall; B waits then must see
/// refreshed residual ~0 (not the stale pre-queue residual).
#[tokio::test]
async fn queued_child_refreshes_duration_after_semaphore_wait() {
    let dir = tmp("queue-duration", 206);
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);

    let runtime = Arc::new(
        SleepyRuntime::new(
            vec![
                assistant_with(
                    vec![
                        spawn_call(
                            "s1",
                            serde_json::json!({"task": "A slow", "role": "explorer"}),
                        ),
                        spawn_call(
                            "s2",
                            serde_json::json!({"task": "B after wait", "role": "explorer"}),
                        ),
                    ],
                    FinishReason::ToolCalls,
                ),
                // Child A: holds the only concurrency slot past parent budget.
                assistant_text("A done after delay"),
                // Child B: after queue wait residual must be 0 — no shell.
                assistant_with(
                    vec![tool_call_part(
                        "b1",
                        "run_command",
                        serde_json::json!({"program": "echo", "args": ["b-should-not-run"]}),
                    )],
                    FinishReason::ToolCalls,
                ),
                assistant_text("B done"),
                assistant_text("parent done"),
            ],
            Duration::from_millis(0),
        )
        .with_delays(vec![
            Duration::from_millis(5),   // parent spawn (fast → residual > 0)
            Duration::from_millis(200), // child A holds permit past 100ms budget
            Duration::from_millis(0),   // child B model (if still runs)
            Duration::from_millis(0),
            Duration::from_millis(0),
        ]),
    );

    let mut events = Vec::new();
    let outcome = Executor::new(
        runtime,
        Arc::new(default_registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .with_agents(1, 4)
    .with_step_limits(leveler_agent::StepLimits {
        max_duration: Some(Duration::from_millis(100)),
        ..Default::default()
    })
    .run(
        "queue duration",
        &mut |e| events.push(e),
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await
    .unwrap();

    // B must not successfully execute its shell after waiting past the deadline.
    let b_shell_ok = events.iter().any(|e| {
        matches!(
            e,
            AgentEvent::ToolResult {
                id,
                is_error: false,
                ..
            } if id == "b1"
        )
    });
    assert!(
        !b_shell_ok,
        "queued child past parent duration must not run shell successfully; \
         cmds={} events={events:?}",
        outcome.progress.cumulative_commands
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Child Cancelled path must roll up partial ProgressUpdated (commands spent
/// before cancel), not an empty ledger.
#[tokio::test]
async fn child_cancelled_mid_run_rolls_up_partial_command_spend() {
    let dir = tmp("child-cancel-partial", 207);
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);

    let runtime = Arc::new(SleepyRuntime::new(
        vec![
            assistant_with(
                vec![spawn_call(
                    "s1",
                    serde_json::json!({"task": "spend then hang", "role": "explorer"}),
                )],
                FinishReason::ToolCalls,
            ),
            // Child: command first (fast enough to commit).
            assistant_with(
                vec![tool_call_part(
                    "c1",
                    "run_command",
                    serde_json::json!({"program": "echo", "args": ["spent"]}),
                )],
                FinishReason::ToolCalls,
            ),
            // Second child model call is slow — cancel during this stream.
            assistant_text("still working"),
            assistant_text("parent unused"),
        ],
        // Every stream sleeps this long. Timeline with cancel@220ms:
        // 0–80 parent spawn stream; 80–160 child tool stream + shell;
        // 160–240 child second stream — cancel lands mid-stream after spend.
        Duration::from_millis(80),
    ));

    let token = CancellationToken::new();
    let cancel = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(220)).await;
        cancel.cancel();
    });

    let mut events = Vec::new();
    let result = Executor::new(
        runtime,
        Arc::new(default_registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .run(
        "cancel mid child",
        &mut |e| events.push(e),
        &mut NoopSink,
        token,
    )
    .await;

    // Prefer Cancelled (mid-child) over clean finish; either way spend must land.
    let max_commands = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ProgressUpdated { ledger } => Some(ledger.cumulative_commands),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    assert!(
        max_commands >= 1,
        "Cancelled child partial ledger must include the pre-cancel command; \
         max_commands={max_commands} result={result:?} events={events:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Blocker 4: parent duration is wall-clock of the parent drive; concurrent
/// children must not inflate cumulative_duration_ms by summing their runtimes.
#[tokio::test]
async fn child_duration_does_not_inflate_parent_wall_clock() {
    let dir = tmp("duration-rollup", 204);
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);

    // Two concurrent children each delayed 80ms → concurrent wall ~80ms, serial ~160ms.
    let runtime = Arc::new(SleepyRuntime::new(
        vec![
            assistant_with(
                vec![
                    spawn_call("s1", serde_json::json!({"task": "A", "role": "explorer"})),
                    spawn_call("s2", serde_json::json!({"task": "B", "role": "explorer"})),
                ],
                FinishReason::ToolCalls,
            ),
            assistant_text("A"),
            assistant_text("B"),
            assistant_text("parent done"),
        ],
        Duration::from_millis(80),
    ));

    let started = Instant::now();
    let outcome = Executor::new(
        runtime,
        Arc::new(default_registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .run(
        "parallel duration",
        &mut |_| {},
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let wall = started.elapsed();

    // If absorb incorrectly summed child durations, ledger would approach
    // serial sum of child walls; parent wall is concurrent and shorter.
    let ledger_ms = outcome.progress.cumulative_duration_ms;
    assert!(
        ledger_ms <= wall.as_millis() as u64 + 50,
        "ledger duration must track parent wall, not sum of children; ledger_ms={ledger_ms} wall={wall:?}"
    );
    // Sanity: we actually ran concurrent-ish (under serial 4×80=320ms).
    assert!(
        wall < Duration::from_millis(350),
        "test setup expected concurrency; wall={wall:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}
