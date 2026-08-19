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

fn first_started_id(events: &[AgentEvent]) -> String {
    events
        .iter()
        .find_map(|e| match e {
            AgentEvent::SubAgentStarted { id, .. } => Some(id.clone()),
            _ => None,
        })
        .expect("SubAgentStarted")
}

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
    /// The message list of every request, in stream order, so tests can assert
    /// what the model was actually shown on each round.
    requests: Mutex<Vec<Vec<Message>>>,
}

impl SleepyRuntime {
    fn new(responses: Vec<ModelResponse>, delay: Duration) -> Self {
        Self {
            responses: Mutex::new(VecDeque::from(responses)),
            delays: Mutex::new(VecDeque::new()),
            default_delay: delay,
            on_stream: None,
            stream_count: std::sync::atomic::AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
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
        request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        use leveler_model::ModelEvent;
        self.requests.lock().unwrap().push(request.messages.clone());
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

fn spawn_call(id: &str, mut args: serde_json::Value) -> ContentPart {
    // V2 made background the default; the legacy tests below exercise the
    // synchronous (foreground) fold and say so explicitly. Background-path
    // tests pass `run_in_background: true` (or use `spawn_call_default`).
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

/// A spawn with NO `run_in_background` key: exercises the runtime default
/// resolution (background).
fn spawn_call_default(id: &str, args: serde_json::Value) -> ContentPart {
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

    let child = first_started_id(&events);
    assert!(
        events.iter().any(|event| {
            matches!(event, AgentEvent::SubAgentProgress {
            id,
            active: true,
            input_tokens: 1_900,
            output_tokens: 110,
            cached_input_tokens: 900,
        } if id == &child)
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

/// Human-review P2: a Worker must not keep a shell mutation outside its
/// exclusive files. The existing command_write_allowlist + snapshot restore
/// is the close — this test proves it is wired through spawn_agent, not
/// only the isolated run_command unit test.
#[cfg(unix)]
#[tokio::test]
async fn a_worker_cannot_keep_a_shell_write_outside_its_scope() {
    let dir = tmp("worker-shell-escape", 110);
    std::fs::write(dir.join("owned.rs"), "owned\n").unwrap();
    std::fs::write(dir.join("other.rs"), "other\n").unwrap();
    init_git(&dir);
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    let runtime = Arc::new(SleepyRuntime::new(
        vec![
            assistant_with(
                vec![spawn_call(
                    "s1",
                    serde_json::json!({
                        "task": "edit owned.rs",
                        "role": "worker",
                        "files": ["owned.rs"]
                    }),
                )],
                FinishReason::ToolCalls,
            ),
            assistant_with(
                vec![tool_call_part(
                    "c1",
                    "run_command",
                    serde_json::json!({
                        "program": "sh",
                        "args": ["-c", "echo hacked > other.rs"]
                    }),
                )],
                FinishReason::ToolCalls,
            ),
            assistant_text("tried the shell write"),
            assistant_text("parent wrap-up"),
        ],
        Duration::from_millis(0),
    ));

    Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .run(
        "delegate",
        &mut |_| {},
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await
    .unwrap();

    let other = std::fs::read_to_string(dir.join("other.rs")).unwrap();
    assert_eq!(
        other, "other\n",
        "out-of-scope shell write must not survive"
    );
    std::fs::remove_dir_all(&dir).ok();
}

fn init_git(dir: &std::path::Path) {
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["config", "user.email", "t@t"]);
    git(&["config", "user.name", "t"]);
    git(&["config", "core.autocrlf", "false"]);
    git(&["add", "-A"]);
    git(&["commit", "-qm", "init"]);
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
    .with_execution_controls(0, max_parallel_tools);

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
    .with_execution_controls(0, 4)
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
///
/// The children here — and in the three command-budget tests below — must NOT
/// take `role: "explorer"`. An explorer runs on `read_only_subset()`, which has
/// no `run_command`, so its calls resolve to "unknown tool" and never charge the
/// command budget. A real model cannot make that call either (the tool is absent
/// from the definitions it is shown), so scripting one tests nothing about
/// budget sharing.
#[tokio::test]
async fn parallel_children_do_not_oversell_command_budget() {
    let dir = tmp("parallel-budget", 202);
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);

    let runtime = Arc::new(SleepyRuntime::new(
        vec![
            assistant_with(
                vec![
                    spawn_call("s1", serde_json::json!({"task": "run two echoes A"})),
                    spawn_call("s2", serde_json::json!({"task": "run two echoes B"})),
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
                        serde_json::json!({"task": "run then hang"}),
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
                    spawn_call("s1", serde_json::json!({"task": "run one command"})),
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
                    serde_json::json!({"task": "spend then hang"}),
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

/// Concurrent children that each run a real tool must emit attributed activity
/// (agent id + tool name) so clients can show current/recent steps.
///
/// Responses are routed by task text so concurrent children do not steal each
/// other's scripted turns from a shared FIFO.
#[tokio::test]
async fn concurrent_spawns_emit_attributed_tool_activity() {
    let dir = tmp("activity", 21);
    std::fs::write(dir.join("a.txt"), "alpha\n").unwrap();
    std::fs::write(dir.join("b.txt"), "beta\n").unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    /// Parent first, then per-task child scripts: tool then text.
    struct TaskRoutedRuntime {
        parent_done: std::sync::atomic::AtomicBool,
        /// task marker → remaining responses for that child.
        by_task: Mutex<std::collections::HashMap<&'static str, VecDeque<ModelResponse>>>,
        parent_final: Mutex<Option<ModelResponse>>,
    }

    impl TaskRoutedRuntime {
        fn new() -> Self {
            let mut by_task = std::collections::HashMap::new();
            by_task.insert(
                "TASK_ALPHA",
                VecDeque::from(vec![
                    assistant_with(
                        vec![tool_call_part(
                            "cA",
                            "list_files",
                            serde_json::json!({"path": "."}),
                        )],
                        FinishReason::ToolCalls,
                    ),
                    assistant_text("A listed."),
                ]),
            );
            by_task.insert(
                "TASK_BETA",
                VecDeque::from(vec![
                    assistant_with(
                        vec![tool_call_part(
                            "cB",
                            "list_files",
                            serde_json::json!({"path": "."}),
                        )],
                        FinishReason::ToolCalls,
                    ),
                    assistant_text("B listed."),
                ]),
            );
            Self {
                parent_done: std::sync::atomic::AtomicBool::new(false),
                by_task: Mutex::new(by_task),
                parent_final: Mutex::new(Some(assistant_text("Both listed."))),
            }
        }

        fn response_for(&self, request: &ModelRequest) -> ModelResponse {
            let blob: String = request
                .messages
                .iter()
                .map(|m| m.text_content())
                .collect::<Vec<_>>()
                .join("\n");
            if blob.contains("TASK_ALPHA") {
                return self
                    .by_task
                    .lock()
                    .unwrap()
                    .get_mut("TASK_ALPHA")
                    .and_then(|q| q.pop_front())
                    .expect("TASK_ALPHA responses exhausted");
            }
            if blob.contains("TASK_BETA") {
                return self
                    .by_task
                    .lock()
                    .unwrap()
                    .get_mut("TASK_BETA")
                    .and_then(|q| q.pop_front())
                    .expect("TASK_BETA responses exhausted");
            }
            // Parent conversation (no child task markers).
            if !self
                .parent_done
                .swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                return assistant_with(
                    vec![
                        spawn_call(
                            "s1",
                            serde_json::json!({
                                "task": "TASK_ALPHA list workspace",
                                "role": "explorer"
                            }),
                        ),
                        spawn_call(
                            "s2",
                            serde_json::json!({
                                "task": "TASK_BETA list workspace",
                                "role": "explorer"
                            }),
                        ),
                    ],
                    FinishReason::ToolCalls,
                );
            }
            self.parent_final
                .lock()
                .unwrap()
                .take()
                .expect("parent final already used")
        }
    }

    #[async_trait]
    impl ModelRuntime for TaskRoutedRuntime {
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
            use leveler_model::ModelEvent;
            let response = self.response_for(&request);
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

        async fn profile(&self, _m: &ModelRef) -> Result<ModelProfile, ModelError> {
            unimplemented!()
        }
    }

    let mut events = Vec::new();
    Executor::new(
        Arc::new(TaskRoutedRuntime::new()),
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        20,
    )
    .run(
        "parallel list",
        &mut |e| events.push(e),
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await
    .unwrap();

    let started_ids: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::SubAgentStarted { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        started_ids.len(),
        2,
        "need two concurrent children: {events:?}"
    );

    let activities: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::SubAgentActivity {
                id, phase, tool, ..
            } => Some((id.clone(), phase.clone(), tool.clone())),
            _ => None,
        })
        .collect();
    for id in &started_ids {
        assert!(
            activities.iter().any(|(aid, phase, tool)| aid == id
                && phase == "tool_started"
                && tool == "list_files"),
            "child {id} must report list_files start: {activities:?}"
        );
        assert!(
            activities.iter().any(|(aid, phase, tool)| {
                aid == id && phase == "tool_finished" && tool == "list_files"
            }),
            "child {id} must report list_files finish: {activities:?}"
        );
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// With `with_delegation(false)`, the model must not see `spawn_agent`.
#[tokio::test]
async fn delegation_off_hides_spawn_agent_from_tool_list() {
    let dir = tmp("nodeleg", 22);
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    let tool_names: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let capture = tool_names.clone();

    struct CaptureToolsRuntime {
        names: Arc<Mutex<Vec<String>>>,
        inner: SleepyRuntime,
    }

    #[async_trait]
    impl ModelRuntime for CaptureToolsRuntime {
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
            cancellation: CancellationToken,
        ) -> Result<ModelEventStream, ModelError> {
            let names: Vec<String> = request.tools.iter().map(|t| t.name.clone()).collect();
            *self.names.lock().unwrap() = names;
            self.inner.stream(request, cancellation).await
        }

        async fn profile(&self, m: &ModelRef) -> Result<ModelProfile, ModelError> {
            self.inner.profile(m).await
        }
    }

    let runtime = Arc::new(CaptureToolsRuntime {
        names: capture,
        inner: SleepyRuntime::new(vec![assistant_text("ok")], Duration::from_millis(0)),
    });

    Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        5,
    )
    .with_delegation(false)
    .run(
        "hello",
        &mut |_| {},
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await
    .unwrap();

    let names = tool_names.lock().unwrap().clone();
    assert!(
        !names.iter().any(|n| n == "spawn_agent"),
        "spawn_agent must be absent when delegation is off: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "read_file" || n == "list_files"),
        "core tools should still be present: {names:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Named agents: `agent="<name>"` must actually deliver the persona to the
/// child. A sub-agent starts a FRESH conversation, so if the instructions do
/// not travel with the task, the child just improvises and the run still looks
/// like it worked.
mod named_agents {
    use super::*;

    struct CapturingRuntime {
        parent_done: std::sync::atomic::AtomicBool,
        /// Every prompt blob the runtime was asked to answer.
        seen: Mutex<Vec<String>>,
        /// Tool names advertised on each request, in the same order.
        tools_seen: Mutex<Vec<Vec<String>>>,
        parent_call: Mutex<Option<serde_json::Value>>,
    }

    impl CapturingRuntime {
        fn new(spawn_args: serde_json::Value) -> Self {
            Self {
                parent_done: std::sync::atomic::AtomicBool::new(false),
                seen: Mutex::new(Vec::new()),
                tools_seen: Mutex::new(Vec::new()),
                parent_call: Mutex::new(Some(spawn_args)),
            }
        }

        fn response_for(&self, request: &ModelRequest) -> ModelResponse {
            let blob: String = request
                .messages
                .iter()
                .map(|m| m.text_content())
                .collect::<Vec<_>>()
                .join("\n");
            self.seen.lock().unwrap().push(blob.clone());
            self.tools_seen
                .lock()
                .unwrap()
                .push(request.tools.iter().map(|t| t.name.clone()).collect());
            if blob.contains("SUBTASK_MARKER") {
                return assistant_text("child done");
            }
            if !self
                .parent_done
                .swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                let args = self.parent_call.lock().unwrap().take().unwrap();
                return assistant_with(vec![spawn_call("s1", args)], FinishReason::ToolCalls);
            }
            assistant_text("parent done")
        }
    }

    #[async_trait]
    impl ModelRuntime for CapturingRuntime {
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
            use leveler_model::ModelEvent;
            let response = self.response_for(&request);
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

        async fn profile(&self, _m: &ModelRef) -> Result<ModelProfile, ModelError> {
            unimplemented!()
        }
    }

    /// Returns (events, prompt blobs, advertised tool names) — one entry per
    /// model request, in order.
    #[allow(clippy::type_complexity)]
    async fn run_with(
        dir: &std::path::Path,
        args: serde_json::Value,
    ) -> (Vec<AgentEvent>, Vec<String>, Vec<Vec<String>>) {
        let workspace = Workspace::new(dir).unwrap();
        let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
        let runtime = Arc::new(CapturingRuntime::new(args));
        let executor = Executor::new(
            Arc::clone(&runtime) as Arc<dyn ModelRuntime>,
            Arc::new(default_registry()),
            tool_context,
            ModelRef::new("mock", "m"),
            10,
        );
        let mut events = Vec::new();
        let _ = executor
            .run(
                "delegate",
                &mut |e| events.push(e),
                &mut NoopSink,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let seen = runtime.seen.lock().unwrap().clone();
        let tools = runtime.tools_seen.lock().unwrap().clone();
        (events, seen, tools)
    }

    #[tokio::test]
    async fn a_named_agent_delivers_its_persona_and_its_role() {
        let dir = tmp("named-persona", 71);
        let (events, seen, _) = run_with(
            &dir,
            serde_json::json!({"agent": "code-explorer", "task": "SUBTASK_MARKER 查登录流程"}),
        )
        .await;

        let child = seen
            .iter()
            .find(|blob| blob.contains("SUBTASK_MARKER"))
            .expect("the child must have been asked something");
        assert!(
            child.contains("代码探查者"),
            "the built-in persona never reached the child: {child}"
        );
        assert!(
            child.contains("查登录流程"),
            "the concrete assignment must travel with it"
        );

        // The definition carries role: explorer, and no `role` was passed.
        let role = events.iter().find_map(|e| match e {
            AgentEvent::SubAgentStarted { role, .. } => Some(role.clone()),
            _ => None,
        });
        assert_eq!(
            role.as_deref(),
            Some("explorer"),
            "the definition's role must apply without the caller repeating it"
        );
    }

    /// A typo must fail loudly. Falling back to a personaless spawn would run
    /// something subtly different while reporting success.
    /// A declared tool list must bind the child's REAL toolset. If it only
    /// shaped the prompt, an explorer persona could still call a write tool.
    #[tokio::test]
    async fn a_declared_tool_list_binds_the_child_toolset() {
        let dir = tmp("named-tools", 73);
        let agents = dir.join(".leveler").join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(
            agents.join("narrow.md"),
            "---\nname: narrow\ndescription: d\nrole: explorer\ntools: [read_file, grep]\n---\n只读查。\n",
        )
        .unwrap();

        let (_events, seen, tools) = run_with(
            &dir,
            serde_json::json!({"agent": "narrow", "task": "SUBTASK_MARKER 看看"}),
        )
        .await;

        let child = seen
            .iter()
            .position(|b| b.contains("SUBTASK_MARKER"))
            .expect("the child must have run");
        let child_tools = &tools[child];
        assert!(
            child_tools.iter().any(|t| t == "read_file"),
            "declared tools must be present: {child_tools:?}"
        );
        assert!(
            child_tools.iter().any(|t| t == "grep"),
            "declared tools must be present: {child_tools:?}"
        );
        for forbidden in ["apply_patch", "run_command", "shell_command", "replace"] {
            assert!(
                !child_tools.iter().any(|t| t == forbidden),
                "`{forbidden}` was not declared and must not reach the child: {child_tools:?}"
            );
        }
    }

    #[tokio::test]
    async fn an_unknown_agent_name_is_rejected_with_the_available_ones() {
        let dir = tmp("named-unknown", 72);
        let (events, _, _) = run_with(
            &dir,
            serde_json::json!({"agent": "code-explorerr", "task": "SUBTASK_MARKER x"}),
        )
        .await;

        let error = events
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
            .expect("an unknown agent must produce a tool error");
        assert!(error.contains("Unknown agent"), "{error}");
        assert!(
            error.contains("code-explorer"),
            "the error must list what IS available: {error}"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::SubAgentStarted { .. })),
            "nothing may be spawned for an unknown name"
        );
    }
}

/// Full access means "no prompts at all". `handle_request_permissions` used to
/// call the approver directly, bypassing `ApprovalPolicy::evaluate` entirely —
/// so a user who explicitly opted out of prompting still got interrupted, to
/// grant a permission they already had.
mod full_access_is_silent {
    use super::*;

    struct CountingApprover {
        asked: Arc<Mutex<usize>>,
    }

    #[async_trait]
    impl leveler_execution::Approver for CountingApprover {
        async fn decide(
            &self,
            _request: &leveler_execution::ApprovalRequest,
        ) -> leveler_execution::ApprovalDecision {
            *self.asked.lock().unwrap() += 1;
            leveler_execution::ApprovalDecision::Deny
        }
    }

    fn permission_call(id: &str) -> ContentPart {
        tool_call_part(
            id,
            "request_permissions",
            serde_json::json!({"network": true, "reason": "需要下载依赖"}),
        )
    }

    #[tokio::test]
    async fn requesting_permissions_under_full_access_asks_nobody() {
        let dir = tmp("fullaccess-silent", 41);
        let workspace = Workspace::new(&dir).unwrap();
        let tool_context = ToolContext::new(workspace, PermissionProfile::FullAccess);
        let asked = Arc::new(Mutex::new(0usize));

        let runtime = Arc::new(SleepyRuntime::new(
            vec![
                assistant_with(vec![permission_call("p1")], FinishReason::ToolCalls),
                assistant_text("done"),
            ],
            Duration::from_millis(0),
        ));
        let executor = Executor::new(
            runtime,
            Arc::new(default_registry()),
            tool_context,
            ModelRef::new("mock", "m"),
            10,
        )
        .with_approver(Arc::new(CountingApprover {
            asked: asked.clone(),
        }));

        let mut events = Vec::new();
        let _ = executor
            .run(
                "install deps",
                &mut |e| events.push(e),
                &mut NoopSink,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert_eq!(
            *asked.lock().unwrap(),
            0,
            "full access must not route a permission request to a human"
        );
        // `request_permissions` is handled inline (drive.rs) and does not emit a
        // ToolResult event; its outcome lands in the transcript the next round
        // carries, so assert on that.
        let transcript = format!("{events:?}");
        assert!(
            transcript.contains("已获授权"),
            "the request must be granted, not refused: {transcript}"
        );
    }

    /// Better still: do not advertise the tool at all when there is nothing to
    /// ask for. It costs tokens and invites a pointless round trip.
    #[tokio::test]
    async fn full_access_does_not_advertise_the_permission_tool() {
        let dir = tmp("fullaccess-tools", 42);
        let workspace = Workspace::new(&dir).unwrap();
        let tool_context = ToolContext::new(workspace, PermissionProfile::FullAccess);
        let names: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

        struct CaptureTools {
            names: Arc<Mutex<Vec<String>>>,
            inner: SleepyRuntime,
        }

        #[async_trait]
        impl ModelRuntime for CaptureTools {
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
                cancellation: CancellationToken,
            ) -> Result<ModelEventStream, ModelError> {
                *self.names.lock().unwrap() =
                    request.tools.iter().map(|t| t.name.clone()).collect();
                self.inner.stream(request, cancellation).await
            }
            async fn profile(&self, m: &ModelRef) -> Result<ModelProfile, ModelError> {
                self.inner.profile(m).await
            }
        }

        let executor = Executor::new(
            Arc::new(CaptureTools {
                names: names.clone(),
                inner: SleepyRuntime::new(vec![assistant_text("ok")], Duration::from_millis(0)),
            }),
            Arc::new(default_registry()),
            tool_context,
            ModelRef::new("mock", "m"),
            10,
        );
        let _ = executor
            .run("hi", &mut |_| {}, &mut NoopSink, CancellationToken::new())
            .await
            .unwrap();

        let listed = names.lock().unwrap().clone();
        assert!(
            !listed.iter().any(|n| n == "request_permissions"),
            "nothing to request under full access: {listed:?}"
        );
    }
}

/// Steering: user input that arrives while a turn is running must reach the
/// model at the next round, not after the turn. Queuing it until the end makes
/// a correction ("actually use the other module") arrive too late to matter.
mod steering {
    use super::*;
    use leveler_agent::SteeringSource;

    struct QueuedOnce {
        pending: Mutex<Vec<String>>,
        takes: Mutex<usize>,
    }

    impl SteeringSource for QueuedOnce {
        fn take_pending(&self) -> Vec<String> {
            *self.takes.lock().unwrap() += 1;
            std::mem::take(&mut *self.pending.lock().unwrap())
        }
    }

    fn runtime_seeing(marker: &'static str) -> Arc<SleepyRuntime> {
        let _ = marker;
        Arc::new(SleepyRuntime::new(
            vec![
                assistant_with(
                    vec![tool_call_part(
                        "c1",
                        "list_files",
                        serde_json::json!({"path": "."}),
                    )],
                    FinishReason::ToolCalls,
                ),
                assistant_text("done"),
            ],
            Duration::from_millis(0),
        ))
    }

    #[tokio::test]
    async fn steering_text_reaches_the_transcript_mid_turn() {
        let dir = tmp("steering", 81);
        let workspace = Workspace::new(&dir).unwrap();
        let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
        let source = Arc::new(QueuedOnce {
            pending: Mutex::new(vec!["STEER_MARKER 改用另一个模块".to_string()]),
            takes: Mutex::new(0),
        });

        let executor = Executor::new(
            runtime_seeing("x"),
            Arc::new(default_registry()),
            tool_context,
            ModelRef::new("mock", "m"),
            10,
        )
        .with_steering(source.clone() as Arc<dyn SteeringSource>);

        let mut events = Vec::new();
        let _ = executor
            .run(
                "原任务",
                &mut |e| events.push(e),
                &mut NoopSink,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        let blob = format!("{events:?}");
        assert!(
            blob.contains("STEER_MARKER"),
            "steering text never reached the conversation: {blob}"
        );
        assert!(
            *source.takes.lock().unwrap() >= 2,
            "the loop must ask every round, not once"
        );
    }

    /// The common case is nothing queued; it must not disturb the transcript.
    #[tokio::test]
    async fn an_empty_steering_source_changes_nothing() {
        let dir = tmp("steering-empty", 82);
        let workspace = Workspace::new(&dir).unwrap();
        let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
        let source = Arc::new(QueuedOnce {
            pending: Mutex::new(vec![String::new(), "   ".to_string()]),
            takes: Mutex::new(0),
        });

        let executor = Executor::new(
            runtime_seeing("y"),
            Arc::new(default_registry()),
            tool_context,
            ModelRef::new("mock", "m"),
            10,
        )
        .with_steering(source as Arc<dyn SteeringSource>);

        let outcome = executor
            .run(
                "原任务",
                &mut |_| {},
                &mut NoopSink,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(outcome.final_text, "done", "blank steering must be dropped");
    }
}

// ── a delegated agent's side effects must be recoverable ────────────────────

/// A worker child that crashes mid-edit used to leave nothing the host could
/// reconcile: its tool calls surfaced only as transient `SubAgentActivity`,
/// never as durable facts. These assert the two properties recovery needs —
/// the call is recorded, attributed to the child that made it, and the record
/// is durable BEFORE the tool runs.
mod child_side_effects_are_recoverable {
    use super::*;
    use std::sync::Mutex;

    use leveler_agent::{ChildToolEvent, EventBarrier};

    #[derive(Default)]
    struct RecordingBarrier {
        events: Mutex<Vec<ChildToolEvent>>,
        /// Order of operations, so "recorded before flushed" is checkable.
        order: Mutex<Vec<&'static str>>,
    }

    #[async_trait::async_trait]
    impl EventBarrier for RecordingBarrier {
        async fn flush(&self) -> Result<(), leveler_agent::AgentError> {
            self.order.lock().unwrap().push("flush");
            Ok(())
        }
        fn record_child_tool_event(&self, event: ChildToolEvent) {
            self.order.lock().unwrap().push(match event {
                ChildToolEvent::Started { .. } => "started",
                ChildToolEvent::Finished { .. } => "finished",
            });
            self.events.lock().unwrap().push(event);
        }
    }

    #[tokio::test]
    async fn a_child_tool_call_is_recorded_and_attributed() {
        let dir = tmp("child-attribution", 71);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "pub fn old() {}\n").unwrap();

        let barrier = Arc::new(RecordingBarrier::default());
        let runtime = Arc::new(SleepyRuntime::new(
            vec![
                // The parent delegates…
                assistant_with(
                    vec![spawn_call(
                        "s1",
                        serde_json::json!({"role": "explorer", "task": "read the library"}),
                    )],
                    FinishReason::ToolCalls,
                ),
                // …the child reads a file…
                assistant_with(
                    vec![tool_call_part(
                        "c1",
                        "read_file",
                        serde_json::json!({"path": "src/lib.rs"}),
                    )],
                    FinishReason::ToolCalls,
                ),
                // …and both finish.
                assistant_text("child done"),
                assistant_text("parent done"),
            ],
            Duration::from_millis(0),
        ));

        let workspace = Workspace::new(&dir).unwrap();
        let executor = Executor::new(
            runtime,
            Arc::new(default_registry()),
            ToolContext::new(workspace, PermissionProfile::Assisted),
            ModelRef::new("mock", "m"),
            10,
        )
        .with_delegation(true)
        .with_event_barrier(barrier.clone());

        executor
            .run(
                "delegate a read",
                &mut |_| {},
                &mut NoopSink,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        let events = barrier.events.lock().unwrap().clone();
        assert!(
            !events.is_empty(),
            "a delegated tool call produced no durable record at all"
        );
        let started = events
            .iter()
            .find_map(|e| match e {
                ChildToolEvent::Started { agent_id, name, .. } => {
                    Some((agent_id.clone(), name.clone()))
                }
                _ => None,
            })
            .expect("the child's call must be recorded as started");
        assert_eq!(started.1, "read_file");
        assert!(
            !started.0.is_empty(),
            "the record must name WHICH agent made the call"
        );
        assert!(
            events.iter().any(|e| matches!(
                e,
                ChildToolEvent::Finished { name, .. } if name == "read_file"
            )),
            "the child's call must also be closed, or it looks dangling forever"
        );
    }

    /// Ordering is the property that makes the record useful: if the flush
    /// could overtake the event, the barrier would report durability for a
    /// call that is not recorded yet — precisely the crash window this is
    /// supposed to close.
    #[tokio::test]
    async fn the_child_record_precedes_the_barrier_flush() {
        let dir = tmp("child-order", 73);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "pub fn old() {}\n").unwrap();

        let barrier = Arc::new(RecordingBarrier::default());
        let runtime = Arc::new(SleepyRuntime::new(
            vec![
                assistant_with(
                    vec![spawn_call(
                        "s1",
                        serde_json::json!({"role": "explorer", "task": "read it"}),
                    )],
                    FinishReason::ToolCalls,
                ),
                assistant_with(
                    vec![tool_call_part(
                        "c1",
                        "read_file",
                        serde_json::json!({"path": "src/lib.rs"}),
                    )],
                    FinishReason::ToolCalls,
                ),
                assistant_text("child done"),
                assistant_text("parent done"),
            ],
            Duration::from_millis(0),
        ));

        let workspace = Workspace::new(&dir).unwrap();
        Executor::new(
            runtime,
            Arc::new(default_registry()),
            ToolContext::new(workspace, PermissionProfile::Assisted),
            ModelRef::new("mock", "m"),
            10,
        )
        .with_delegation(true)
        .with_event_barrier(barrier.clone())
        .run(
            "delegate a read",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

        let order = barrier.order.lock().unwrap().clone();
        let started_at = order.iter().position(|s| *s == "started");
        assert!(started_at.is_some(), "no child record was made: {order:?}");
        let flush_after = order[started_at.unwrap()..].contains(&"flush");
        assert!(
            flush_after,
            "the child's call was recorded but never flushed before dispatch: {order:?}"
        );
    }
}

// ── Gate 5 / N1: a child's terminal result must be legible to the parent ────
//
// R007b: a child died on its wall budget and returned only a stop string. The
// parent could not tell that apart from "investigated and found nothing", so it
// closed the task without ever opening the file the child had been reading.
// These four cases are the whole contract, asserted on the tool result the
// parent model actually receives.

/// Captures the transcript so a test can read the tool result the parent saw.
struct RecordingSink {
    messages: Arc<Mutex<Vec<Message>>>,
}

#[async_trait]
impl leveler_agent::TranscriptSink for RecordingSink {
    async fn append(&mut self, messages: &[Message]) -> Result<(), leveler_agent::AgentError> {
        self.messages.lock().unwrap().extend_from_slice(messages);
        Ok(())
    }
}

/// The `spawn_agent` tool result for `call_id`, as (content, is_error).
fn spawn_result(messages: &[Message], call_id: &str) -> (String, bool) {
    for message in messages {
        for part in &message.content {
            if let ContentPart::ToolResult { result } = part
                && result.call_id.as_str() == call_id
            {
                return (result.content.clone(), result.is_error);
            }
        }
    }
    panic!("no tool result for {call_id} in {messages:?}");
}

fn child_result_harness(
    tag: &str,
    salt: u64,
    responses: Vec<ModelResponse>,
    delay: Duration,
) -> (std::path::PathBuf, Arc<SleepyRuntime>, ToolContext) {
    let dir = tmp(tag, salt);
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    (
        dir,
        Arc::new(SleepyRuntime::new(responses, delay)),
        tool_context,
    )
}

/// A: the child completed and produced findings.
#[tokio::test]
async fn child_completed_with_findings_is_reported_as_such() {
    let (dir, runtime, tool_context) = child_result_harness(
        "child-result-a",
        301,
        vec![
            assistant_with(
                vec![spawn_call(
                    "s1",
                    serde_json::json!({"task": "audit Headers"}),
                )],
                FinishReason::ToolCalls,
            ),
            assistant_text("Headers.vue drops the auth header on retry."),
            assistant_text("parent done"),
        ],
        Duration::from_millis(0),
    );
    let transcript = Arc::new(Mutex::new(Vec::new()));
    let mut sink = RecordingSink {
        messages: transcript.clone(),
    };
    let _ = Executor::new(
        runtime,
        Arc::new(default_registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .run("delegate", &mut |_| {}, &mut sink, CancellationToken::new())
    .await;

    let (content, is_error) = spawn_result(&transcript.lock().unwrap(), "s1");
    assert!(
        content.contains("COMPLETED_WITH_FINDINGS"),
        "status must be explicit: {content}"
    );
    assert!(
        content.contains("Headers.vue drops the auth header"),
        "the findings themselves must survive: {content}"
    );
    assert!(!is_error, "a completed child is not an error");
    std::fs::remove_dir_all(&dir).ok();
}

/// B: the child completed and had nothing to report. This is a RESULT, and must
/// not read as a failure.
#[tokio::test]
async fn child_completed_without_findings_is_distinguishable_from_failure() {
    let (dir, runtime, tool_context) = child_result_harness(
        "child-result-b",
        302,
        vec![
            assistant_with(
                vec![spawn_call(
                    "s1",
                    serde_json::json!({"task": "audit Headers"}),
                )],
                FinishReason::ToolCalls,
            ),
            // A silent child is nudged before its silence is accepted, so it
            // takes several empty rounds to reach a clean end with an empty
            // report — the one shape that means "finished, nothing to say".
            assistant_text("   "),
            assistant_text("   "),
            assistant_text("   "),
            assistant_text("parent done"),
        ],
        Duration::from_millis(0),
    );
    let transcript = Arc::new(Mutex::new(Vec::new()));
    let mut sink = RecordingSink {
        messages: transcript.clone(),
    };
    let _ = Executor::new(
        runtime,
        Arc::new(default_registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .run("delegate", &mut |_| {}, &mut sink, CancellationToken::new())
    .await;

    let (content, is_error) = spawn_result(&transcript.lock().unwrap(), "s1");
    assert!(
        content.contains("COMPLETED_NO_FINDINGS"),
        "completing with nothing to say needs its own status: {content}"
    );
    assert!(
        !content.contains("INCOMPLETE"),
        "a completed child must never be labelled incomplete: {content}"
    );
    assert!(!is_error, "reporting nothing is not an error");
    std::fs::remove_dir_all(&dir).ok();
}

/// C: the child ran out of budget mid-investigation. What it had already
/// learned must reach the parent, marked partial.
#[tokio::test]
async fn budget_limited_child_preserves_its_partial_findings() {
    let (dir, runtime, tool_context) = child_result_harness(
        "child-result-c",
        303,
        vec![
            assistant_with(
                vec![spawn_call(
                    "s1",
                    serde_json::json!({"task": "audit Headers"}),
                )],
                FinishReason::ToolCalls,
            ),
            // Child round 1: says what it found so far, then keeps working.
            assistant_with(
                vec![
                    ContentPart::Text {
                        text: "so far: Headers.vue sets the header in two places".to_string(),
                    },
                    tool_call_part(
                        "c1",
                        "run_command",
                        serde_json::json!({"program": "echo", "args": ["still reading"]}),
                    ),
                ],
                FinishReason::ToolCalls,
            ),
            // Child round 2 is slow; the budget (cancellation) lands during it.
            assistant_text("never delivered"),
            assistant_text("parent unused"),
        ],
        Duration::from_millis(80),
    );
    let token = CancellationToken::new();
    let cancel = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(220)).await;
        cancel.cancel();
    });
    let transcript = Arc::new(Mutex::new(Vec::new()));
    let mut sink = RecordingSink {
        messages: transcript.clone(),
    };
    let _ = Executor::new(
        runtime,
        Arc::new(default_registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .run("delegate", &mut |_| {}, &mut sink, token)
    .await;

    let (content, _) = spawn_result(&transcript.lock().unwrap(), "s1");
    assert!(
        content.contains("INCOMPLETE"),
        "a child stopped by its budget did not complete: {content}"
    );
    assert!(
        content.contains("Headers.vue sets the header in two places"),
        "work the child had already done must not be discarded: {content}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// D: the child failed before producing anything. The parent must be told there
/// is NO result — the one reading that R007b got wrong.
#[tokio::test]
async fn failed_child_reports_no_result_rather_than_no_findings() {
    let (dir, runtime, tool_context) = child_result_harness(
        "child-result-d",
        304,
        // Only the parent's spawn round is scripted: the child's first model
        // call errors out.
        vec![assistant_with(
            vec![spawn_call(
                "s1",
                serde_json::json!({"task": "audit Headers"}),
            )],
            FinishReason::ToolCalls,
        )],
        Duration::from_millis(0),
    );
    let transcript = Arc::new(Mutex::new(Vec::new()));
    let mut sink = RecordingSink {
        messages: transcript.clone(),
    };
    let _ = Executor::new(
        runtime,
        Arc::new(default_registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .run("delegate", &mut |_| {}, &mut sink, CancellationToken::new())
    .await;

    let (content, is_error) = spawn_result(&transcript.lock().unwrap(), "s1");
    assert!(
        content.contains("INCOMPLETE_NO_RESULT"),
        "producing nothing must be its own status: {content}"
    );
    assert!(
        !content.contains("NO_FINDINGS"),
        "a child that produced nothing did NOT report 'no findings': {content}"
    );
    assert!(
        is_error,
        "a child that never ran is an error for the parent"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// The `spawn_agent` schema promises a worker "MUST be given `files` it
/// exclusively owns". Before capability admission, an empty `files` silently
/// produced a worker with UNRESTRICTED write access — the opposite.
#[tokio::test]
async fn a_worker_spawn_without_a_file_scope_is_refused() {
    let dir = tmp("worker-noscope", 91);
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    let runtime = Arc::new(SleepyRuntime::new(
        vec![
            assistant_with(
                vec![spawn_call(
                    "s1",
                    serde_json::json!({"task": "edit something", "role": "worker"}),
                )],
                FinishReason::ToolCalls,
            ),
            assistant_text("Parent wrap-up."),
        ],
        Duration::from_millis(0),
    ));

    let mut events = Vec::new();
    Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .run(
        "delegate",
        &mut |e| events.push(e),
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await
    .unwrap();

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::SubAgentStarted { .. })),
        "an unscoped worker must never start"
    );
    let refusal = events
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
        .expect("the spawn must be refused with a reason");
    assert!(
        refusal.contains("files"),
        "the denial must name the missing scope: {refusal}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// A read-only role asking for a write scope is an incoherent capability
/// request: deny honestly instead of silently ignoring the `files`.
#[tokio::test]
async fn an_explorer_spawn_with_a_write_scope_is_refused() {
    let dir = tmp("explorer-scope", 92);
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    let runtime = Arc::new(SleepyRuntime::new(
        vec![
            assistant_with(
                vec![spawn_call(
                    "s1",
                    serde_json::json!({
                        "task": "look around",
                        "role": "explorer",
                        "files": ["src/a.rs"]
                    }),
                )],
                FinishReason::ToolCalls,
            ),
            assistant_text("Parent wrap-up."),
        ],
        Duration::from_millis(0),
    ));

    let mut events = Vec::new();
    Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .run(
        "delegate",
        &mut |e| events.push(e),
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await
    .unwrap();

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::SubAgentStarted { .. })),
        "the incoherent spawn must not start"
    );
    let refusal = events
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
        .expect("the spawn must be refused with a reason");
    assert!(
        refusal.contains("read-only"),
        "the denial must say why: {refusal}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Two same-batch workers whose "exclusive" scopes overlap cannot both be
/// admitted — that is last-writer-wins waiting to happen. The first keeps its
/// scope; the second is refused honestly.
#[tokio::test]
async fn overlapping_worker_scopes_refuse_the_second_spawn() {
    let dir = tmp("worker-overlap", 93);
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    let runtime = Arc::new(SleepyRuntime::new(
        vec![
            assistant_with(
                vec![
                    spawn_call(
                        "s1",
                        serde_json::json!({
                            "task": "edit auth",
                            "role": "worker",
                            "files": ["src/auth.rs"]
                        }),
                    ),
                    // Directory prefix overlap with the first scope.
                    spawn_call(
                        "s2",
                        serde_json::json!({
                            "task": "edit src tree",
                            "role": "worker",
                            "files": ["src"]
                        }),
                    ),
                    // Disjoint: must be admitted.
                    spawn_call(
                        "s3",
                        serde_json::json!({
                            "task": "edit docs",
                            "role": "worker",
                            "files": ["docs/README.md"]
                        }),
                    ),
                ],
                FinishReason::ToolCalls,
            ),
            assistant_text("auth edited"),
            assistant_text("docs edited"),
            assistant_text("Parent wrap-up."),
        ],
        Duration::from_millis(0),
    ));

    let mut events = Vec::new();
    Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .run(
        "delegate",
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
        "the overlapping worker must be refused; the disjoint pair runs"
    );
    let refusal = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolResult {
                id,
                name,
                is_error: true,
                preview,
            } if name == "spawn_agent" && id == "s2" => Some(preview.clone()),
            _ => None,
        })
        .expect("the overlapping spawn must be refused");
    assert!(
        refusal.contains("overlap"),
        "the denial must name the collision: {refusal}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

// ── Structured findings flow ────────────────────────────────────────────────

fn finding_call(id: &str, args: serde_json::Value) -> ContentPart {
    tool_call_part(id, "report_finding", args)
}

fn resolve_call(id: &str, args: serde_json::Value) -> ContentPart {
    tool_call_part(id, "resolve_finding", args)
}

/// The last ledger snapshot the PARENT emitted (adoption is a parent-side,
/// durable act — receipt must be observable, not implied).
fn last_ledger(events: &[AgentEvent]) -> leveler_lifecycle::EvidenceLedger {
    events
        .iter()
        .rev()
        .find_map(|e| match e {
            AgentEvent::EvidenceLedgerUpdated { ledger } => Some(ledger.clone()),
            _ => None,
        })
        .expect("adoption must persist a ledger snapshot")
}

/// 50.A/D: an explorer's typed finding reaches the parent ledger at
/// Acknowledged, attributed to the child, and the parent-facing tool result
/// names the adopted id so the model can judge it.
#[tokio::test]
async fn an_explorer_finding_is_adopted_by_the_parent_at_acknowledged() {
    let dir = tmp("finding-adopt", 94);
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    let runtime = Arc::new(SleepyRuntime::new(
        vec![
            assistant_with(
                vec![spawn_call(
                    "s1",
                    serde_json::json!({"task": "find the config loader", "role": "explorer"}),
                )],
                FinishReason::ToolCalls,
            ),
            // Child round 1: a typed finding…
            assistant_with(
                vec![finding_call(
                    "f1",
                    serde_json::json!({
                        "kind": "relevant_file",
                        "summary": "config loader lives here",
                        "file": "src/config.rs"
                    }),
                )],
                FinishReason::ToolCalls,
            ),
            // …child round 2: prose wrap-up.
            assistant_text("Found it: src/config.rs."),
            assistant_text("Parent wrap-up."),
        ],
        Duration::from_millis(0),
    ));

    let mut events = Vec::new();
    let transcript = Arc::new(Mutex::new(Vec::new()));
    let mut sink = RecordingSink {
        messages: transcript.clone(),
    };
    Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .run(
        "delegate",
        &mut |e| events.push(e),
        &mut sink,
        CancellationToken::new(),
    )
    .await
    .unwrap();

    let ledger = last_ledger(&events);
    assert_eq!(ledger.findings.len(), 1, "one adopted finding");
    let f = &ledger.findings[0];
    assert_eq!(f.state, leveler_lifecycle::FindingState::Acknowledged);
    // 970e5db: child ids are session-unique UUIDs, not run ordinals.
    assert!(!f.source_child.is_empty() && f.source_child != "agent-1");
    assert_eq!(f.role, "explorer");
    assert_eq!(f.summary, "config loader lives here");
    assert_eq!(f.file.as_deref(), Some("src/config.rs"));
    assert!(!f.blocking);

    let (content, is_error) = spawn_result(&transcript.lock().unwrap(), "s1");
    assert!(!is_error);
    assert!(
        content.contains(&f.id),
        "the parent-facing result must name the adopted finding id: {content}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Only the reviewer role may raise blocking findings; an explorer's
/// blocking=true is recorded, but non-blocking.
#[tokio::test]
async fn a_non_reviewer_blocking_flag_is_not_honored() {
    let dir = tmp("finding-blocking", 95);
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    let runtime = Arc::new(SleepyRuntime::new(
        vec![
            assistant_with(
                vec![spawn_call(
                    "s1",
                    serde_json::json!({"task": "investigate", "role": "explorer"}),
                )],
                FinishReason::ToolCalls,
            ),
            assistant_with(
                vec![finding_call(
                    "f1",
                    serde_json::json!({
                        "kind": "risk",
                        "summary": "this feels risky",
                        "blocking": true
                    }),
                )],
                FinishReason::ToolCalls,
            ),
            assistant_text("Done."),
            assistant_text("Parent wrap-up."),
        ],
        Duration::from_millis(0),
    ));

    let mut events = Vec::new();
    Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .run(
        "delegate",
        &mut |e| events.push(e),
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await
    .unwrap();

    let ledger = last_ledger(&events);
    assert_eq!(ledger.findings.len(), 1);
    assert!(
        !ledger.findings[0].blocking,
        "an explorer cannot gate closure with a blocking finding"
    );
    assert!(ledger.open_blocking_findings().is_empty());
    std::fs::remove_dir_all(&dir).ok();
}

/// 50.C: a child that dies mid-run keeps what it had already established —
/// its typed findings survive into the parent ledger even though the child's
/// own run is an INCOMPLETE error for the parent.
#[tokio::test]
async fn a_stopped_child_still_delivers_its_partial_findings() {
    let dir = tmp("finding-partial", 96);
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    // The child reports one finding, then the runtime dies (no more scripted
    // responses) — an abnormal termination, not a clean end.
    let runtime = Arc::new(SleepyRuntime::new(
        vec![
            assistant_with(
                vec![spawn_call(
                    "s1",
                    serde_json::json!({"task": "investigate", "role": "explorer"}),
                )],
                FinishReason::ToolCalls,
            ),
            assistant_with(
                vec![finding_call(
                    "f1",
                    serde_json::json!({
                        "kind": "callsite",
                        "summary": "handler registered in router.rs"
                    }),
                )],
                FinishReason::ToolCalls,
            ),
        ],
        Duration::from_millis(0),
    ));

    let mut events = Vec::new();
    Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .run(
        "delegate",
        &mut |e| events.push(e),
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await
    .ok();

    let ledger = last_ledger(&events);
    assert_eq!(
        ledger.findings.len(),
        1,
        "partial findings must survive an abnormal child stop"
    );
    assert_eq!(
        ledger.findings[0].summary,
        "handler registered in router.rs"
    );
    assert_eq!(
        ledger.findings[0].state,
        leveler_lifecycle::FindingState::Acknowledged
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// The parent's judgment runs through the audited lifecycle: accept works,
/// rejecting without a reason is refused, rejecting with one lands and is
/// durable.
#[tokio::test]
async fn the_parent_resolves_findings_through_the_audited_lifecycle() {
    let dir = tmp("finding-resolve", 97);
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    let runtime = Arc::new(SleepyRuntime::new(
        vec![
            assistant_with(
                vec![spawn_call(
                    "s1",
                    serde_json::json!({"task": "investigate", "role": "explorer"}),
                )],
                FinishReason::ToolCalls,
            ),
            assistant_with(
                vec![finding_call(
                    "f1",
                    serde_json::json!({"kind": "risk", "summary": "duplicated lock"}),
                )],
                FinishReason::ToolCalls,
            ),
            assistant_text("Done."),
            assistant_with(
                vec![
                    resolve_call(
                        "r1",
                        serde_json::json!({"id": "f-1", "resolution": "accepted"}),
                    ),
                    resolve_call(
                        "r2",
                        serde_json::json!({"id": "f-1", "resolution": "rejected"}),
                    ),
                    resolve_call(
                        "r3",
                        serde_json::json!({
                            "id": "f-1",
                            "resolution": "rejected",
                            "reason": "duplicate of known issue"
                        }),
                    ),
                ],
                FinishReason::ToolCalls,
            ),
            assistant_text("Parent wrap-up."),
        ],
        Duration::from_millis(0),
    ));

    let mut events = Vec::new();
    Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .run(
        "delegate",
        &mut |e| events.push(e),
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await
    .unwrap();

    let ok_results: Vec<(String, bool)> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolResult {
                id, name, is_error, ..
            } if name == "resolve_finding" => Some((id.clone(), *is_error)),
            _ => None,
        })
        .collect();
    assert_eq!(ok_results.len(), 3);
    assert!(!ok_results[0].1, "accept must succeed");
    assert!(ok_results[1].1, "reject without a reason must be refused");
    assert!(!ok_results[2].1, "reject with a reason must succeed");

    let ledger = last_ledger(&events);
    let f = ledger.finding("f-1").expect("finding survives");
    assert_eq!(f.state, leveler_lifecycle::FindingState::Rejected);
    assert_eq!(
        f.resolution_reason.as_deref(),
        Some("duplicate of known issue")
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Completion truth: an open blocking finding refuses update_goal(complete);
/// an explicitly rejected one no longer blocks.
#[tokio::test]
async fn an_open_blocking_finding_refuses_goal_completion_until_resolved() {
    let dir = tmp("finding-gate", 98);
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    // Seed the parent with a reviewer-raised blocking finding, as a resumed
    // turn would after a harness review.
    let mut seeded = leveler_lifecycle::EvidenceLedger::default();
    let reviewer_rec = {
        let mut child = leveler_lifecycle::EvidenceLedger::default();
        let id = child.record_finding(
            leveler_lifecycle::FindingKind::Correctness,
            "unlocked shared counter",
            Some("src/state.rs".into()),
            None,
            true,
        );
        child.finding(&id).unwrap().clone()
    };
    seeded.adopt_finding("reviewer-1", "reviewer", &reviewer_rec);

    let runtime = Arc::new(SleepyRuntime::new(
        vec![
            assistant_with(
                vec![tool_call_part(
                    "g1",
                    "update_goal",
                    serde_json::json!({"status": "complete", "summary": "done"}),
                )],
                FinishReason::ToolCalls,
            ),
            assistant_with(
                vec![
                    resolve_call(
                        "r1",
                        serde_json::json!({
                            "id": "f-1",
                            "resolution": "rejected",
                            "reason": "code path unreachable in this build"
                        }),
                    ),
                    tool_call_part(
                        "g2",
                        "update_goal",
                        serde_json::json!({"status": "complete", "summary": "done"}),
                    ),
                ],
                FinishReason::ToolCalls,
            ),
        ],
        Duration::from_millis(0),
    ));

    let mut events = Vec::new();
    let outcome = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .with_goal_mode(true)
    .with_seeded_ledger(seeded)
    .run(
        "finish the task",
        &mut |e| events.push(e),
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await
    .unwrap();

    let goal_results: Vec<bool> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolResult { name, is_error, .. } if name == "update_goal" => {
                Some(*is_error)
            }
            _ => None,
        })
        .collect();
    assert_eq!(goal_results.len(), 2);
    assert!(
        goal_results[0],
        "completion with an open blocking finding must be refused"
    );
    assert!(!goal_results[1], "completion after rejection must pass");
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::GoalIntercepted { kind, .. } if kind == "blocking_finding"
        )),
        "the interception must be observable"
    );
    assert_eq!(outcome.stop_reason, StopReason::Completed);
    std::fs::remove_dir_all(&dir).ok();
}

/// A Worker that dies before finishing is original-goal debt: the host
/// records a blocking finding so the parent cannot silently Verified.
#[tokio::test]
async fn an_incomplete_worker_raises_a_blocking_finding() {
    let dir = tmp("worker-incomplete-block", 99);
    std::fs::write(dir.join("owned.rs"), "pub fn owned() {}\n").unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    // Only the parent's spawn is scripted: the child's first model call
    // errors, which is INCOMPLETE_NO_RESULT — and must become a blocking
    // parent finding.
    let runtime = Arc::new(SleepyRuntime::new(
        vec![assistant_with(
            vec![spawn_call(
                "s1",
                serde_json::json!({
                    "task": "edit owned.rs",
                    "role": "worker",
                    "files": ["owned.rs"]
                }),
            )],
            FinishReason::ToolCalls,
        )],
        Duration::from_millis(0),
    ));

    let mut events = Vec::new();
    let _ = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .run(
        "delegate",
        &mut |e| events.push(e),
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await;

    let ledger = last_ledger(&events);
    let open = ledger.open_blocking_findings();
    assert_eq!(open.len(), 1, "an incomplete worker must block: {open:?}");
    assert_eq!(open[0].role, "worker");
    assert!(!open[0].source_child.is_empty() && open[0].source_child != "agent-1");
    assert!(
        open[0].summary.contains("did not complete"),
        "the finding must name the incomplete work: {}",
        open[0].summary
    );

    let started = events.iter().find_map(|e| match e {
        AgentEvent::SubAgentStarted { role, task, .. } => Some((role.clone(), task.clone())),
        _ => None,
    });
    let (role, task) = started.expect("worker must have started");
    assert_eq!(role, "worker");
    assert!(
        task.contains("[scope: owned.rs]"),
        "worker start must disclose scope: {task}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// A child that only calls report_finding and writes no wrap-up prose still
/// completed WITH findings — structured records are a result.
#[tokio::test]
async fn structured_findings_without_prose_are_still_a_result() {
    let dir = tmp("structured-is-result", 103);
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());
    let transcript = Arc::new(Mutex::new(Vec::new()));
    let mut sink = RecordingSink {
        messages: transcript.clone(),
    };

    let runtime = Arc::new(SleepyRuntime::new(
        vec![
            assistant_with(
                vec![spawn_call(
                    "s1",
                    serde_json::json!({"task": "find it", "role": "explorer"}),
                )],
                FinishReason::ToolCalls,
            ),
            assistant_with(
                vec![finding_call(
                    "f1",
                    serde_json::json!({
                        "kind": "relevant_file",
                        "summary": "loader is here",
                        "file": "src/config.rs"
                    }),
                )],
                FinishReason::ToolCalls,
            ),
            // Quiet wrap-up is nudged before it is accepted (same shape as
            // child_completed_without_findings). The typed finding must still
            // count as a result.
            assistant_text("   "),
            assistant_text("   "),
            assistant_text("   "),
            assistant_text("Parent wrap-up."),
        ],
        Duration::from_millis(0),
    ));

    Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .run("delegate", &mut |_| {}, &mut sink, CancellationToken::new())
    .await
    .unwrap();

    let (content, is_error) = spawn_result(&transcript.lock().unwrap(), "s1");
    assert!(!is_error, "typed findings are a clean result: {content}");
    assert!(
        content.contains("COMPLETED_WITH_FINDINGS"),
        "empty prose must not erase structured findings: {content}"
    );
    assert!(
        !content.contains("COMPLETED_NO_FINDINGS"),
        "must not be classified as no-findings: {content}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Explorer incomplete is knowledge loss, not a closure gate.
#[tokio::test]
async fn an_incomplete_explorer_does_not_raise_a_blocking_finding() {
    let dir = tmp("explorer-incomplete-noblock", 100);
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    let runtime = Arc::new(SleepyRuntime::new(
        vec![assistant_with(
            vec![spawn_call(
                "s1",
                serde_json::json!({"task": "investigate", "role": "explorer"}),
            )],
            FinishReason::ToolCalls,
        )],
        Duration::from_millis(0),
    ));

    let mut events = Vec::new();
    let _ = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .run(
        "delegate",
        &mut |e| events.push(e),
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await;

    assert!(
        events
            .iter()
            .all(|e| !matches!(e, AgentEvent::EvidenceLedgerUpdated { .. }))
            || last_ledger(&events).open_blocking_findings().is_empty(),
        "an incomplete explorer must not invent a blocking finding"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Replay: a finding accepted in one drive is still Accepted — and still
/// exactly one — when a fresh executor is seeded from that ledger snapshot.
#[tokio::test]
async fn accepted_findings_survive_a_seeded_replay_without_duplication() {
    let dir = tmp("finding-replay", 101);
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    let runtime = Arc::new(SleepyRuntime::new(
        vec![
            assistant_with(
                vec![spawn_call(
                    "s1",
                    serde_json::json!({"task": "investigate", "role": "explorer"}),
                )],
                FinishReason::ToolCalls,
            ),
            assistant_with(
                vec![finding_call(
                    "f1",
                    serde_json::json!({"kind": "risk", "summary": "duplicated lock"}),
                )],
                FinishReason::ToolCalls,
            ),
            assistant_text("Done."),
            assistant_with(
                vec![resolve_call(
                    "r1",
                    serde_json::json!({"id": "f-1", "resolution": "accepted"}),
                )],
                FinishReason::ToolCalls,
            ),
            assistant_text("Parent wrap-up."),
        ],
        Duration::from_millis(0),
    ));

    let mut events = Vec::new();
    Executor::new(
        runtime,
        registry.clone(),
        tool_context.clone(),
        ModelRef::new("mock", "m"),
        10,
    )
    .run(
        "delegate",
        &mut |e| events.push(e),
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await
    .unwrap();

    let snapshot = last_ledger(&events);
    assert_eq!(snapshot.findings.len(), 1);
    assert_eq!(
        snapshot.finding("f-1").unwrap().state,
        leveler_lifecycle::FindingState::Accepted
    );

    // "Crash": a fresh executor seeded from the persisted snapshot.
    let runtime2 = Arc::new(SleepyRuntime::new(
        vec![assistant_text("resumed wrap-up.")],
        Duration::from_millis(0),
    ));
    let mut events2 = Vec::new();
    Executor::new(
        runtime2,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .with_seeded_ledger(snapshot.clone())
    .run(
        "continue",
        &mut |e| events2.push(e),
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await
    .unwrap();

    // The seeded ledger is the in-memory starting point; a quiet resume
    // emits no new snapshot. The contract is: we did not create a second
    // finding, and the snapshot we would persist is still Accepted.
    let after = events2
        .iter()
        .rev()
        .find_map(|e| match e {
            AgentEvent::EvidenceLedgerUpdated { ledger } => Some(ledger.clone()),
            _ => None,
        })
        .unwrap_or(snapshot);
    assert_eq!(after.findings.len(), 1, "replay must not duplicate");
    assert_eq!(
        after.finding("f-1").unwrap().state,
        leveler_lifecycle::FindingState::Accepted
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Authoritative events reconstruct role / status / scope / finding count.
#[tokio::test]
async fn child_observability_reconstructs_from_authoritative_events() {
    let dir = tmp("obs-rebuild", 102);
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    let runtime = Arc::new(SleepyRuntime::new(
        vec![
            assistant_with(
                vec![spawn_call(
                    "s1",
                    serde_json::json!({"task": "find the config loader", "role": "explorer"}),
                )],
                FinishReason::ToolCalls,
            ),
            assistant_with(
                vec![finding_call(
                    "f1",
                    serde_json::json!({
                        "kind": "relevant_file",
                        "summary": "config loader lives here",
                        "file": "src/config.rs"
                    }),
                )],
                FinishReason::ToolCalls,
            ),
            assistant_text("Found it."),
            assistant_text("Parent wrap-up."),
        ],
        Duration::from_millis(0),
    ));

    let mut events = Vec::new();
    Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        10,
    )
    .run(
        "delegate",
        &mut |e| events.push(e),
        &mut NoopSink,
        CancellationToken::new(),
    )
    .await
    .unwrap();

    let started = events.iter().find_map(|e| match e {
        AgentEvent::SubAgentStarted { id, role, task, .. } => {
            Some((id.clone(), role.clone(), task.clone()))
        }
        _ => None,
    });
    let finished = events.iter().rev().find_map(|e| match e {
        AgentEvent::SubAgentFinished {
            id, ok, summary, ..
        } => Some((id.clone(), *ok, summary.clone())),
        _ => None,
    });
    let (id, role, _task) = started.expect("start");
    let (fid, ok, summary) = finished.expect("finish");
    assert_eq!(id, fid);
    assert_eq!(role, "explorer");
    assert!(ok, "explorer finished cleanly");
    assert!(
        summary.contains("Structured findings adopted"),
        "finish summary must name adopted findings: {summary}"
    );
    let ledger = last_ledger(&events);
    assert_eq!(ledger.findings.len(), 1);
    assert_eq!(ledger.findings[0].role, "explorer");
    std::fs::remove_dir_all(&dir).ok();
}

// ── MA-WA1: keep-vs-delegate decision point + disposition observability ──────

fn delegation_stages(events: &[AgentEvent]) -> Vec<(String, String)> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::DelegationStage { action, detail } => {
                Some((action.clone(), detail.clone()))
            }
            _ => None,
        })
        .collect()
}

fn patch_call(id: &str, file: &str, from: &str, to: &str) -> ContentPart {
    tool_call_part(
        id,
        "apply_patch",
        serde_json::json!({
            "patch": format!("*** Begin Patch\n*** Update File: {file}\n-{from}\n+{to}\n*** End Patch")
        }),
    )
}

/// Accident regression (MA-WA1 root defect): once the model registers its own
/// multi-step decomposition, the harness must raise the one-shot
/// keep-vs-delegate decision point — and a mutation after the offer with no
/// Worker records an observable KEEP. Before this repair the harness had no
/// decision point and a KEEP run left zero trace.
#[tokio::test]
async fn plan_registration_offers_the_decision_point_once_and_keep_is_recorded() {
    let dir = tmp("decision-plan", 41);
    std::fs::write(dir.join("a.txt"), "old\n").unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());
    let runtime = Arc::new(SleepyRuntime::new(
        vec![
            assistant_with(
                vec![tool_call_part(
                    "p1",
                    "update_plan",
                    serde_json::json!({
                        "plan": [
                            {"step": "edit the file", "status": "in_progress"},
                            {"step": "verify", "status": "pending"}
                        ]
                    }),
                )],
                FinishReason::ToolCalls,
            ),
            assistant_with(
                vec![patch_call("e1", "a.txt", "old", "new")],
                FinishReason::ToolCalls,
            ),
            assistant_with(
                vec![patch_call("e2", "a.txt", "new", "newer")],
                FinishReason::ToolCalls,
            ),
            assistant_text("done"),
        ],
        Duration::from_millis(1),
    ));
    let executor = Executor::new(
        runtime.clone(),
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        8,
    );
    let mut events = Vec::new();
    executor
        .run(
            "improve the file handling",
            &mut |e| events.push(e),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let stages = delegation_stages(&events);
    assert_eq!(
        stages
            .iter()
            .filter(|(action, _)| action == "offered")
            .count(),
        1,
        "exactly one decision point: {stages:?}"
    );
    assert!(
        stages
            .iter()
            .any(|(action, detail)| action == "offered" && detail == "plan"),
        "plan registration is the trigger: {stages:?}"
    );
    assert_eq!(
        stages.iter().filter(|(action, _)| action == "kept").count(),
        1,
        "first mutation after the offer records KEEP exactly once: {stages:?}"
    );

    // The offer reaches the model exactly once, after the plan round.
    let requests = runtime.requests.lock().unwrap();
    let offer_count_in = |messages: &[Message]| {
        messages
            .iter()
            .filter(|m| {
                m.role == Role::User && m.text_content().contains("## Delegation disposition")
            })
            .count()
    };
    assert_eq!(
        offer_count_in(&requests[0]),
        0,
        "no offer before the decomposition exists"
    );
    let last = requests.last().unwrap();
    assert_eq!(
        offer_count_in(last),
        1,
        "the offer is injected once and never repeated"
    );
    let offer_text = last
        .iter()
        .find(|m| m.role == Role::User && m.text_content().contains("## Delegation disposition"))
        .unwrap()
        .text_content();
    assert!(
        offer_text.contains("1. edit the file") && offer_text.contains("2. verify"),
        "the plan-triggered offer enumerates the model's own open steps: {offer_text}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Fallback trigger: a run that never registers a structured plan still gets
/// exactly one decision point — at its SECOND mutating round (a lone prep
/// edit leaves room for a plan to land first) — and a later mutation records
/// KEEP.
#[tokio::test]
async fn the_second_mutating_round_is_the_fallback_decision_point_without_a_plan() {
    let dir = tmp("decision-mutation", 42);
    std::fs::write(dir.join("a.txt"), "old\n").unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());
    let runtime = Arc::new(SleepyRuntime::new(
        vec![
            assistant_with(
                vec![patch_call("e1", "a.txt", "old", "new")],
                FinishReason::ToolCalls,
            ),
            assistant_with(
                vec![patch_call("e2", "a.txt", "new", "newer")],
                FinishReason::ToolCalls,
            ),
            assistant_with(
                vec![patch_call("e3", "a.txt", "newer", "newest")],
                FinishReason::ToolCalls,
            ),
            assistant_text("done"),
        ],
        Duration::from_millis(1),
    ));
    let executor = Executor::new(
        runtime.clone(),
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        8,
    );
    let mut events = Vec::new();
    executor
        .run(
            "tweak the file",
            &mut |e| events.push(e),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let stages = delegation_stages(&events);
    assert_eq!(
        stages
            .iter()
            .filter(|(action, _)| action == "offered")
            .count(),
        1,
        "{stages:?}"
    );
    assert!(
        stages
            .iter()
            .any(|(action, detail)| action == "offered" && detail == "mutation_fallback"),
        "{stages:?}"
    );
    assert_eq!(
        stages.iter().filter(|(action, _)| action == "kept").count(),
        1,
        "{stages:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// MA-WA1 repair accident regression (EB-3 shape), executor level: the offer
/// lands at plan registration when the tail steps are dependency-blocked →
/// rational KEEP; the plan then progresses (a step completes, ≥2 bounded
/// steps remain) — the harness raises exactly ONE reconsideration, grounded
/// in the parent's own edited paths, records the durable `reoffered` fact,
/// and never asks again.
#[tokio::test]
async fn plan_progress_after_keep_raises_one_reconsideration_with_parent_scope_facts() {
    let dir = tmp("decision-reconsider", 44);
    std::fs::write(dir.join("a.txt"), "old\n").unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());
    let runtime = Arc::new(SleepyRuntime::new(
        vec![
            // r1: decomposition on record → offer this round.
            assistant_with(
                vec![tool_call_part(
                    "p1",
                    "update_plan",
                    serde_json::json!({
                        "plan": [
                            {"step": "implement core", "status": "in_progress"},
                            {"step": "add regression tests", "status": "pending"},
                            {"step": "update docs", "status": "pending"}
                        ]
                    }),
                )],
                FinishReason::ToolCalls,
            ),
            // r2: mutation after the visible offer → durable KEEP.
            assistant_with(
                vec![patch_call("e1", "a.txt", "old", "new")],
                FinishReason::ToolCalls,
            ),
            // r3: core completed; two bounded steps remain open → the one
            // reconsideration fires at this round boundary.
            assistant_with(
                vec![tool_call_part(
                    "p2",
                    "update_plan",
                    serde_json::json!({
                        "plan": [
                            {"step": "implement core", "status": "completed"},
                            {"step": "add regression tests", "status": "in_progress"},
                            {"step": "update docs", "status": "pending"}
                        ]
                    }),
                )],
                FinishReason::ToolCalls,
            ),
            // r4: more progress — must NOT re-ask again.
            assistant_with(
                vec![tool_call_part(
                    "p3",
                    "update_plan",
                    serde_json::json!({
                        "plan": [
                            {"step": "implement core", "status": "completed"},
                            {"step": "add regression tests", "status": "completed"},
                            {"step": "update docs", "status": "in_progress"}
                        ]
                    }),
                )],
                FinishReason::ToolCalls,
            ),
            assistant_text("done"),
        ],
        Duration::from_millis(1),
    ));
    let executor = Executor::new(
        runtime.clone(),
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        8,
    );
    let mut events = Vec::new();
    executor
        .run(
            "land the feature",
            &mut |e| events.push(e),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let stages = delegation_stages(&events);
    assert_eq!(
        stages
            .iter()
            .filter(|(action, _)| action == "offered")
            .count(),
        1,
        "{stages:?}"
    );
    assert_eq!(
        stages.iter().filter(|(action, _)| action == "kept").count(),
        1,
        "{stages:?}"
    );
    assert_eq!(
        stages
            .iter()
            .filter(|(action, _)| action == "reoffered")
            .count(),
        1,
        "exactly one event-driven reconsideration: {stages:?}"
    );
    assert!(
        stages
            .iter()
            .any(|(action, detail)| action == "reoffered" && detail == "plan_progress"),
        "{stages:?}"
    );

    // The durable fact survives into the ledger (resume windows never re-ask).
    let last_ledger = events
        .iter()
        .rev()
        .find_map(|e| match e {
            AgentEvent::ProgressUpdated { ledger } => Some(ledger.clone()),
            _ => None,
        })
        .expect("progress ledger updates exist");
    assert!(last_ledger.delegation_reconsidered);
    assert!(last_ledger.delegation_kept_recorded);

    // The reconsideration reaches the model exactly once, enumerates the
    // remaining open steps, and grounds independence in the parent's own
    // edited paths.
    let requests = runtime.requests.lock().unwrap();
    let last = requests.last().unwrap();
    let reconsider_count = last
        .iter()
        .filter(|m| {
            m.role == Role::User && m.text_content().contains("## Delegation reconsideration")
        })
        .count();
    assert_eq!(reconsider_count, 1, "one reconsideration, never repeated");
    let text = last
        .iter()
        .find(|m| {
            m.role == Role::User && m.text_content().contains("## Delegation reconsideration")
        })
        .unwrap()
        .text_content();
    assert!(
        text.contains("1. add regression tests") && text.contains("2. update docs"),
        "remaining open steps are enumerated: {text}"
    );
    assert!(
        text.contains("a.txt"),
        "parent-edited paths ground the boundary: {text}"
    );
    assert!(
        text.to_ascii_lowercase().contains("valid outcome"),
        "KEEP stays first-class: {text}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// A spontaneous Worker spawn IS the delegation decision: record `delegated`
/// with its scope and never raise the offer afterwards (no nagging a model
/// that already decided).
#[tokio::test]
async fn worker_admission_records_a_delegated_disposition_and_suppresses_the_offer() {
    let dir = tmp("decision-delegate", 43);
    std::fs::write(dir.join("a.txt"), "old\n").unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());
    let runtime = Arc::new(SleepyRuntime::new(
        vec![
            assistant_with(
                vec![spawn_call(
                    "s1",
                    serde_json::json!({
                        "task": "change old to new in a.txt",
                        "role": "worker",
                        "files": ["a.txt"]
                    }),
                )],
                FinishReason::ToolCalls,
            ),
            // Worker child: one edit, then reports.
            assistant_with(
                vec![patch_call("w1", "a.txt", "old", "new")],
                FinishReason::ToolCalls,
            ),
            assistant_text("worker: edited a.txt"),
            // Parent integrates and finishes.
            assistant_text("integrated the worker result"),
        ],
        Duration::from_millis(1),
    ));
    let executor = Executor::new(
        runtime.clone(),
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        8,
    );
    let mut events = Vec::new();
    executor
        .run(
            "update the file",
            &mut |e| events.push(e),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let stages = delegation_stages(&events);
    assert!(
        stages
            .iter()
            .any(|(action, detail)| action == "delegated" && detail.contains("a.txt")),
        "{stages:?}"
    );
    assert!(
        stages.iter().all(|(action, _)| action != "offered"),
        "a model that already delegated must not be offered: {stages:?}"
    );
    assert!(
        stages.iter().all(|(action, _)| action != "kept"),
        "{stages:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Kill-switch: with delegation off there is no decision point and no
/// disposition noise at all.
#[tokio::test]
async fn no_decision_point_when_delegation_is_disabled() {
    let dir = tmp("decision-off", 44);
    std::fs::write(dir.join("a.txt"), "old\n").unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());
    let runtime = Arc::new(SleepyRuntime::new(
        vec![
            assistant_with(
                vec![patch_call("e1", "a.txt", "old", "new")],
                FinishReason::ToolCalls,
            ),
            assistant_text("done"),
        ],
        Duration::from_millis(1),
    ));
    let executor = Executor::new(
        runtime.clone(),
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        8,
    )
    .with_delegation(false);
    let mut events = Vec::new();
    executor
        .run(
            "tweak the file",
            &mut |e| events.push(e),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(
        delegation_stages(&events).is_empty(),
        "no delegation facts when delegation is off"
    );
    std::fs::remove_dir_all(&dir).ok();
}

// ── V2 background-first delegation contract ─────────────────────────────────

/// Routes scripted responses by conversation identity instead of arrival
/// order: with a background child racing the parent for model rounds, queue
/// order is nondeterministic, but a child conversation always carries its own
/// task marker in its transcript.
struct RoutedRuntime {
    parent: Mutex<VecDeque<ModelResponse>>,
    child: Mutex<VecDeque<ModelResponse>>,
    child_marker: &'static str,
    /// Delay before each CHILD response (keeps the child "still running"
    /// while parent rounds execute, deterministically).
    child_delay: Duration,
    /// Message lists of every PARENT request, in order.
    parent_requests: Mutex<Vec<Vec<Message>>>,
}

impl RoutedRuntime {
    fn new(
        parent: Vec<ModelResponse>,
        child: Vec<ModelResponse>,
        child_marker: &'static str,
        child_delay: Duration,
    ) -> Self {
        Self {
            parent: Mutex::new(VecDeque::from(parent)),
            child: Mutex::new(VecDeque::from(child)),
            child_marker,
            child_delay,
            parent_requests: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl ModelRuntime for RoutedRuntime {
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
        use leveler_model::ModelEvent;
        let is_child = request
            .messages
            .iter()
            .any(|m| m.text_content().contains(self.child_marker));
        if is_child {
            tokio::time::sleep(self.child_delay).await;
        } else {
            self.parent_requests
                .lock()
                .unwrap()
                .push(request.messages.clone());
        }
        let queue = if is_child { &self.child } else { &self.parent };
        let response = queue.lock().unwrap().pop_front().ok_or_else(|| {
            ModelError::new(
                leveler_model::ModelErrorKind::Other,
                if is_child {
                    "child queue exhausted"
                } else {
                    "parent queue exhausted"
                },
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

    async fn profile(&self, _m: &ModelRef) -> Result<ModelProfile, ModelError> {
        unimplemented!()
    }
}

const CHILD_MARKER: &str = "CHILD-TASK-7f3a";

/// V2 core: an omitted `run_in_background` resolves to background at the
/// RUNTIME — the spawn returns immediately with the child's id, the parent
/// keeps working, and the runtime injects an unconditional settlement notice
/// carrying the child's truthful result.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_default_spawn_runs_in_background_and_settles_into_a_later_round() {
    let dir = tmp("bg-settle", 61);
    std::fs::write(dir.join("a.txt"), "parent\n").unwrap();
    std::fs::write(dir.join("b.txt"), "old\n").unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    let runtime = Arc::new(RoutedRuntime::new(
        vec![
            // r1: delegate (no run_in_background key → background default)
            // AND read a parent file in the same round.
            assistant_with(
                vec![
                    spawn_call_default(
                        "s1",
                        serde_json::json!({
                            "task": format!("{CHILD_MARKER}: change old to new in b.txt"),
                            "role": "worker",
                            "files": ["b.txt"]
                        }),
                    ),
                    tool_call_part("r1", "read_file", serde_json::json!({"path": "a.txt"})),
                ],
                FinishReason::ToolCalls,
            ),
            // r2: parent continues useful work while the child runs.
            assistant_with(
                vec![patch_call("e1", "a.txt", "parent", "parent-more")],
                FinishReason::ToolCalls,
            ),
            // r3: quiet — the harness must WAIT for the settlement, not stall.
            assistant_text("waiting on my delegation"),
            // r4: after the settlement notice, finish.
            assistant_text("integrated the worker result"),
        ],
        vec![
            assistant_with(
                vec![patch_call("w1", "b.txt", "old", "new")],
                FinishReason::ToolCalls,
            ),
            assistant_text("scoped edit done"),
        ],
        CHILD_MARKER,
        Duration::from_millis(150),
    ));
    let executor = Executor::new(
        runtime.clone(),
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        12,
    );
    let mut events = Vec::new();
    let outcome = executor
        .run(
            "update both files",
            &mut |e| events.push(e),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(outcome.final_text, "integrated the worker result");
    // The child's edit really happened (real scoped mutation).
    assert_eq!(std::fs::read_to_string(dir.join("b.txt")).unwrap(), "new\n");
    // The spawn's tool result was immediate — it does NOT carry the child's
    // report; the settlement notice does.
    let spawn_result = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolResult { id, preview, .. } if id == "s1" => Some(preview.clone()),
            _ => None,
        })
        .expect("spawn tool result");
    assert!(
        spawn_result.contains("started in the background"),
        "{spawn_result}"
    );
    let child = first_started_id(&events);
    assert!(
        spawn_result.contains(&child),
        "durable child id must be model-visible: {spawn_result}"
    );
    // Settlement notice reached the model: some later PARENT request carries
    // it as an injected user message, with the child's truthful report.
    let requests = runtime.parent_requests.lock().unwrap();
    let saw_settlement = requests.iter().any(|messages| {
        messages.iter().any(|m| {
            m.role == Role::User
                && m.text_content().contains("## Background sub-agent settled")
                && m.text_content().contains("COMPLETED_WITH_FINDINGS")
        })
    });
    assert!(saw_settlement, "settlement notice must be injected");
    drop(requests);
    // Truthful lifecycle events: started AND finished for the same child.
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::SubAgentStarted { id, .. } if id == &child))
    );
    assert!(
        events.iter().any(
            |e| matches!(e, AgentEvent::SubAgentFinished { id, ok: true, .. } if id == &child)
        )
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// V2 safety: while a background Worker runs, its scope is exclusive against
/// EVERYONE — a parent edit inside it is refused with an honest message, and a
/// second worker overlapping it is refused at admission.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_running_workers_scope_fences_the_parent_and_new_workers() {
    let dir = tmp("bg-fence", 62);
    std::fs::write(dir.join("a.txt"), "parent\n").unwrap();
    std::fs::write(dir.join("b.txt"), "old\n").unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    let runtime = Arc::new(RoutedRuntime::new(
        vec![
            assistant_with(
                vec![spawn_call_default(
                    "s1",
                    serde_json::json!({
                        "task": format!("{CHILD_MARKER}: change old to new in b.txt"),
                        "role": "worker",
                        "files": ["b.txt"]
                    }),
                )],
                FinishReason::ToolCalls,
            ),
            // r2: parent tries to edit the child's file AND to spawn an
            // overlapping worker — both must be refused while it runs.
            assistant_with(
                vec![
                    patch_call("e1", "b.txt", "old", "hijacked"),
                    spawn_call(
                        "s2",
                        serde_json::json!({
                            "task": "also edit b.txt",
                            "role": "worker",
                            "files": ["b.txt"]
                        }),
                    ),
                ],
                FinishReason::ToolCalls,
            ),
            // r3: quiet → wait for settlement.
            assistant_text("waiting"),
            assistant_text("done"),
        ],
        vec![
            assistant_with(
                vec![patch_call("w1", "b.txt", "old", "new")],
                FinishReason::ToolCalls,
            ),
            assistant_text("child done"),
        ],
        CHILD_MARKER,
        Duration::from_millis(250),
    ));
    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        12,
    );
    let mut events = Vec::new();
    executor
        .run(
            "update b via delegation",
            &mut |e| events.push(e),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let refusal = |id: &str| {
        events
            .iter()
            .find_map(|e| match e {
                AgentEvent::ToolResult {
                    id: rid,
                    is_error: true,
                    preview,
                    ..
                } if rid == id => Some(preview.clone()),
                _ => None,
            })
            .unwrap_or_default()
    };
    let parent_edit = refusal("e1");
    assert!(
        parent_edit.contains("exclusive scope") && parent_edit.contains("still-running"),
        "parent edit inside an active child scope must be refused: {parent_edit}"
    );
    let overlap_spawn = refusal("s2");
    assert!(
        overlap_spawn.contains("overlaps exclusive ownership held by"),
        "overlapping worker vs ACTIVE child must be refused: {overlap_spawn}"
    );
    // The child, not the parent, owns the final content of b.txt.
    assert_eq!(std::fs::read_to_string(dir.join("b.txt")).unwrap(), "new\n");
    std::fs::remove_dir_all(&dir).ok();
}

/// V2 completion gate: a goal cannot close while a delegated child is still
/// running — the harness drains the children, injects their settlements, and
/// refuses that resolution once so the model integrates first.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn goal_completion_is_refused_while_children_are_outstanding() {
    let dir = tmp("bg-goalgate", 63);
    std::fs::write(dir.join("b.txt"), "old\n").unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());

    let runtime = Arc::new(RoutedRuntime::new(
        vec![
            assistant_with(
                vec![spawn_call_default(
                    "s1",
                    serde_json::json!({
                        "task": format!("{CHILD_MARKER}: change old to new in b.txt"),
                        "role": "worker",
                        "files": ["b.txt"]
                    }),
                )],
                FinishReason::ToolCalls,
            ),
            // r2: tries to close the goal while the child still runs.
            assistant_with(
                vec![tool_call_part(
                    "g1",
                    "update_goal",
                    serde_json::json!({"status": "complete", "summary": "done"}),
                )],
                FinishReason::ToolCalls,
            ),
            // r3: after the forced settlement + refusal, close for real.
            assistant_with(
                vec![tool_call_part(
                    "g2",
                    "update_goal",
                    serde_json::json!({"status": "complete", "summary": "integrated"}),
                )],
                FinishReason::ToolCalls,
            ),
        ],
        vec![
            assistant_with(
                vec![patch_call("w1", "b.txt", "old", "new")],
                FinishReason::ToolCalls,
            ),
            assistant_text("child done"),
        ],
        CHILD_MARKER,
        Duration::from_millis(200),
    ));
    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        12,
    )
    .with_goal_mode(true);
    let mut events = Vec::new();
    let outcome = executor
        .run(
            "update b.txt via delegation",
            &mut |e| events.push(e),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    // First completion refused with the outstanding-children intercept.
    let refused = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolResult {
                id,
                is_error: true,
                preview,
                ..
            } if id == "g1" => Some(preview.clone()),
            _ => None,
        })
        .expect("first update_goal(complete) must be refused");
    assert!(refused.contains("Cannot complete"), "{refused}");
    assert!(
        events.iter().any(|e| matches!(e,
            AgentEvent::GoalIntercepted { kind, .. } if kind == "outstanding_children")),
        "the intercept must be durable"
    );
    // The child settled (drained) and its work is on disk.
    assert_eq!(std::fs::read_to_string(dir.join("b.txt")).unwrap(), "new\n");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::SubAgentFinished { ok: true, .. }))
    );
    // The second completion goes through.
    assert!(matches!(
        outcome.stop_reason,
        leveler_agent::StopReason::Completed
    ));
    std::fs::remove_dir_all(&dir).ok();
}

/// V2 resume truth: in-process children do not survive a restart. A run seeded
/// with outstanding children must tell the model exactly which delegations
/// were lost and clear the durable record — never let "you will be told when
/// it settles" dangle forever.
#[tokio::test]
async fn a_resumed_run_reports_children_lost_at_restart() {
    let dir = tmp("bg-lost", 64);
    std::fs::write(dir.join("a.txt"), "x\n").unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());
    let runtime = Arc::new(SleepyRuntime::new(
        vec![assistant_text("nothing else to do")],
        Duration::from_millis(1),
    ));
    let mut seeded = leveler_agent::ProgressLedger::default();
    seeded
        .outstanding_children
        .push("agent-2|Newton|worker|src/lib.rs".to_string());
    let executor = Executor::new(
        runtime.clone(),
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        4,
    )
    .with_seeded_progress(seeded);
    let mut events = Vec::new();
    executor
        .run(
            "continue the task",
            &mut |e| events.push(e),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let requests = runtime.requests.lock().unwrap();
    let first = &requests[0];
    let note = first
        .iter()
        .find(|m| {
            m.role == Role::User && m.text_content().contains("## Delegations lost at restart")
        })
        .map(|m| m.text_content())
        .expect("lost-children note must be injected before the first round");
    assert!(
        note.contains("Newton (agent-2, role=worker, scope: src/lib.rs)"),
        "{note}"
    );
    drop(requests);
    // The durable record is cleared so the note never repeats.
    let cleared = events.iter().rev().find_map(|e| match e {
        AgentEvent::ProgressUpdated { ledger } => Some(ledger.outstanding_children.is_empty()),
        _ => None,
    });
    assert_eq!(cleared, Some(true));
    std::fs::remove_dir_all(&dir).ok();
}

/// V2: user cancellation with a background child running must not orphan it —
/// the child is cancelled through its child token, drained at the exit, and
/// its (cancelled) settlement is still folded truthfully.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_drains_background_children_without_orphans() {
    let dir = tmp("bg-cancel", 65);
    std::fs::write(dir.join("b.txt"), "old\n").unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());
    let cancel = CancellationToken::new();

    let runtime = Arc::new(RoutedRuntime::new(
        vec![
            assistant_with(
                vec![spawn_call_default(
                    "s1",
                    serde_json::json!({
                        "task": format!("{CHILD_MARKER}: slow work"),
                        "role": "worker",
                        "files": ["b.txt"]
                    }),
                )],
                FinishReason::ToolCalls,
            ),
            assistant_text("continuing"),
            assistant_text("never reached"),
        ],
        vec![
            // The child's model round is slow; the cancel lands before it.
            assistant_with(
                vec![patch_call("w1", "b.txt", "old", "new")],
                FinishReason::ToolCalls,
            ),
            assistant_text("child done"),
        ],
        CHILD_MARKER,
        Duration::from_millis(600),
    ));
    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        12,
    );
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        cancel_clone.cancel();
    });
    let started = Instant::now();
    let mut events = Vec::new();
    let outcome = executor
        .run(
            "delegate slow work",
            &mut |e| events.push(e),
            &mut NoopSink,
            cancel,
        )
        .await;
    assert!(
        matches!(outcome, Err(leveler_agent::AgentError::Cancelled)),
        "{outcome:?}"
    );
    // The drain settled the child (cancelled, truthfully) instead of
    // orphaning it; the whole run ended promptly.
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::SubAgentFinished { ok: false, .. })),
        "cancelled child must still settle truthfully"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "no hang on cancellation drain"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Review 必改①: the bounded round-limit exit is the COMMON way a busy run
/// ends — it must drain running background children like every other exit,
/// not let the abort-on-drop backstop hard-kill them (spend lost, findings
/// lost, mid-write kill).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_round_limit_exit_drains_running_children_instead_of_aborting() {
    let dir = tmp("bg-roundlimit", 66);
    std::fs::write(dir.join("a.txt"), "x\n").unwrap();
    std::fs::write(dir.join("b.txt"), "old\n").unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());
    let runtime = Arc::new(RoutedRuntime::new(
        vec![
            assistant_with(
                vec![spawn_call_default(
                    "s1",
                    serde_json::json!({
                        "task": format!("{CHILD_MARKER}: change old to new in b.txt"),
                        "role": "worker",
                        "files": ["b.txt"]
                    }),
                )],
                FinishReason::ToolCalls,
            ),
            // Two more busy rounds exhaust max_rounds=3 while the slow child
            // is still running.
            assistant_with(
                vec![tool_call_part(
                    "r1",
                    "read_file",
                    serde_json::json!({"path": "a.txt"}),
                )],
                FinishReason::ToolCalls,
            ),
            assistant_with(
                vec![tool_call_part(
                    "r2",
                    "read_file",
                    serde_json::json!({"path": "a.txt"}),
                )],
                FinishReason::ToolCalls,
            ),
        ],
        vec![
            assistant_with(
                vec![patch_call("w1", "b.txt", "old", "new")],
                FinishReason::ToolCalls,
            ),
            assistant_text("child done"),
        ],
        CHILD_MARKER,
        Duration::from_millis(300),
    ));
    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        3,
    );
    let mut events = Vec::new();
    let outcome = executor
        .run(
            "delegate then keep busy",
            &mut |e| events.push(e),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(matches!(
        outcome.stop_reason,
        leveler_agent::StopReason::BudgetExhausted | leveler_agent::StopReason::TurnLimitReached
    ));
    // The child settled truthfully at the exit drain — it was NOT aborted.
    let child = first_started_id(&events);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::SubAgentFinished { id, .. } if id == &child)),
        "round-limit exit must settle the running child, not abort it"
    );
    // Its work landed and its scope record is cleared.
    assert_eq!(std::fs::read_to_string(dir.join("b.txt")).unwrap(), "new\n");
    let cleared = events.iter().rev().find_map(|e| match e {
        AgentEvent::ProgressUpdated { ledger } => Some(ledger.outstanding_children.is_empty()),
        _ => None,
    });
    assert_eq!(cleared, Some(true), "outstanding record must be cleared");
    std::fs::remove_dir_all(&dir).ok();
}

/// Review 必改②: completing a goal in the SAME batch as parent commands while
/// children are outstanding must not lose the batch's spend to the settlement
/// fold's lagging-ledger overwrite.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_completion_gate_drain_keeps_same_batch_parent_spend() {
    let dir = tmp("bg-pinspend", 67);
    std::fs::write(dir.join("b.txt"), "old\n").unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());
    let runtime = Arc::new(RoutedRuntime::new(
        vec![
            assistant_with(
                vec![
                    spawn_call_default(
                        "s1",
                        serde_json::json!({
                            "task": format!("{CHILD_MARKER}: change old to new in b.txt"),
                            "role": "worker",
                            "files": ["b.txt"]
                        }),
                    ),
                    tool_call_part("c1", "shell_command", serde_json::json!({"cmd": "true"})),
                ],
                FinishReason::ToolCalls,
            ),
            // Same-batch parent command + completion attempt while the child
            // still runs: the gate drains mid-batch.
            assistant_with(
                vec![
                    tool_call_part("c2", "shell_command", serde_json::json!({"cmd": "true"})),
                    tool_call_part(
                        "g1",
                        "update_goal",
                        serde_json::json!({"status": "complete", "summary": "done"}),
                    ),
                ],
                FinishReason::ToolCalls,
            ),
            assistant_with(
                vec![tool_call_part(
                    "g2",
                    "update_goal",
                    serde_json::json!({"status": "complete", "summary": "integrated"}),
                )],
                FinishReason::ToolCalls,
            ),
        ],
        vec![
            assistant_with(
                vec![patch_call("w1", "b.txt", "old", "new")],
                FinishReason::ToolCalls,
            ),
            assistant_text("child done"),
        ],
        CHILD_MARKER,
        Duration::from_millis(250),
    ));
    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        12,
    )
    .with_goal_mode(true);
    let mut events = Vec::new();
    executor
        .run(
            "update b.txt via delegation",
            &mut |e| events.push(e),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let final_commands = events
        .iter()
        .rev()
        .find_map(|e| match e {
            AgentEvent::ProgressUpdated { ledger } => Some(ledger.cumulative_commands),
            _ => None,
        })
        .unwrap();
    assert!(
        final_commands >= 2,
        "both parent shell commands must survive the mid-batch drain: {final_commands}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Review 必改④: the settlement notice must be durable the moment it is
/// injected — a crash before the next snapshot must not lose the child's
/// report after outstanding_children was already cleared.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settlement_notices_are_appended_to_the_transcript_sink() {
    struct RecordingSink(Arc<Mutex<Vec<Message>>>);
    #[async_trait]
    impl leveler_agent::TranscriptSink for RecordingSink {
        async fn append(&mut self, messages: &[Message]) -> Result<(), leveler_agent::AgentError> {
            self.0.lock().unwrap().extend_from_slice(messages);
            Ok(())
        }
    }
    let dir = tmp("bg-sink", 68);
    std::fs::write(dir.join("a.txt"), "parent\n").unwrap();
    std::fs::write(dir.join("b.txt"), "old\n").unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());
    let runtime = Arc::new(RoutedRuntime::new(
        vec![
            assistant_with(
                vec![spawn_call_default(
                    "s1",
                    serde_json::json!({
                        "task": format!("{CHILD_MARKER}: change old to new in b.txt"),
                        "role": "worker",
                        "files": ["b.txt"]
                    }),
                )],
                FinishReason::ToolCalls,
            ),
            assistant_text("waiting"),
            assistant_text("done"),
        ],
        vec![
            assistant_with(
                vec![patch_call("w1", "b.txt", "old", "new")],
                FinishReason::ToolCalls,
            ),
            assistant_text("child done"),
        ],
        CHILD_MARKER,
        Duration::from_millis(120),
    ));
    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        8,
    );
    let transcript = Arc::new(Mutex::new(Vec::new()));
    let mut sink = RecordingSink(transcript.clone());
    executor
        .run(
            "update b via delegation",
            &mut |_| {},
            &mut sink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let recorded = transcript.lock().unwrap();
    let notices = recorded
        .iter()
        .filter(|m| {
            m.role == Role::User && m.text_content().contains("## Background sub-agent settled")
        })
        .count();
    assert_eq!(
        notices, 1,
        "the settlement notice must be persisted to the sink when injected — exactly once \
         (a duplicate would double-inject the child's report on resume)"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Settlement × continuation seam (FA-2 / ORC-B1): a child that settles at the
/// exit drain — the parent's window closed before it could act on the notice —
/// must be recorded as UNCONSUMED settlement debt on the progress ledger, so
/// the continuation layer above can tell a stranded result from a consumed one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_settlement_at_the_exit_drain_is_recorded_as_unconsumed_debt() {
    let dir = tmp("bg-debt-strand", 69);
    std::fs::write(dir.join("a.txt"), "x\n").unwrap();
    std::fs::write(dir.join("b.txt"), "old\n").unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());
    let runtime = Arc::new(RoutedRuntime::new(
        vec![
            // r1: delegate; the slow child outlives the 3-round window.
            assistant_with(
                vec![spawn_call_default(
                    "s1",
                    serde_json::json!({
                        "task": format!("{CHILD_MARKER}: change old to new in b.txt"),
                        "role": "worker",
                        "files": ["b.txt"]
                    }),
                )],
                FinishReason::ToolCalls,
            ),
            // r2/r3: busy observe-only rounds exhaust the window while the
            // child still runs — it settles at the exit drain, unconsumed.
            assistant_with(
                vec![tool_call_part(
                    "r1",
                    "read_file",
                    serde_json::json!({"path": "a.txt"}),
                )],
                FinishReason::ToolCalls,
            ),
            assistant_with(
                vec![tool_call_part(
                    "r2",
                    "read_file",
                    serde_json::json!({"path": "a.txt"}),
                )],
                FinishReason::ToolCalls,
            ),
        ],
        vec![
            assistant_with(
                vec![patch_call("w1", "b.txt", "old", "new")],
                FinishReason::ToolCalls,
            ),
            assistant_text("child done"),
        ],
        CHILD_MARKER,
        Duration::from_millis(300),
    ));
    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        3,
    );
    let outcome = executor
        .run(
            "delegate then run out of window",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    // The child's work is real and settled (drained, not aborted) …
    assert_eq!(std::fs::read_to_string(dir.join("b.txt")).unwrap(), "new\n");
    // … and the ledger names it as debt the parent never got to act on.
    assert_eq!(
        outcome.progress.unconsumed_child_settlements, 1,
        "a settlement the parent had no round to act on must read as unconsumed debt"
    );
    std::fs::remove_dir_all(&dir).ok();
}

// ── Late-bound ownership (SPAWN != WRITE AUTHORITY) ─────────────────────────

/// A normal `spawn_agent(task)` child (no role, no files) starts WITHOUT write
/// authority: a mutation before any claim is denied, not executed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_default_spawned_child_starts_without_write_authority() {
    let dir = tmp("lbo-noauth", 80);
    std::fs::write(dir.join("b.txt"), "old\n").unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());
    let runtime = Arc::new(RoutedRuntime::new(
        vec![
            // Foreground spawn: the child's whole run folds synchronously.
            assistant_with(
                vec![spawn_call(
                    "s1",
                    serde_json::json!({
                        "task": format!("{CHILD_MARKER}: change old to new in b.txt")
                    }),
                )],
                FinishReason::ToolCalls,
            ),
            assistant_text("done"),
        ],
        vec![
            // Child writes WITHOUT claiming first: must be denied.
            assistant_with(
                vec![patch_call("w1", "b.txt", "old", "new")],
                FinishReason::ToolCalls,
            ),
            assistant_text("child done"),
        ],
        CHILD_MARKER,
        Duration::from_millis(10),
    ));
    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        8,
    );
    executor
        .run(
            "update b",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.join("b.txt")).unwrap(),
        "old\n",
        "a spawned child must not mutate the workspace before claiming a write scope"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// The full late-bound protocol: read → claim_write_scope → mutate inside the
/// claim succeeds; the claim tool result is a grant, not an error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_child_claim_enables_scoped_write() {
    let dir = tmp("lbo-claim", 81);
    std::fs::write(dir.join("b.txt"), "old\n").unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());
    let runtime = Arc::new(RoutedRuntime::new(
        vec![
            assistant_with(
                vec![spawn_call(
                    "s1",
                    serde_json::json!({
                        "task": format!("{CHILD_MARKER}: change old to new in b.txt")
                    }),
                )],
                FinishReason::ToolCalls,
            ),
            assistant_text("done"),
        ],
        vec![
            assistant_with(
                vec![tool_call_part(
                    "c1",
                    "claim_write_scope",
                    serde_json::json!({"paths": ["b.txt"]}),
                )],
                FinishReason::ToolCalls,
            ),
            assistant_with(
                vec![patch_call("w1", "b.txt", "old", "new")],
                FinishReason::ToolCalls,
            ),
            assistant_text("child done"),
        ],
        CHILD_MARKER,
        Duration::from_millis(10),
    ));
    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        8,
    );
    let mut events = Vec::new();
    executor
        .run(
            "update b",
            &mut |e| events.push(e),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    // The claim's own ToolResult stays inside the child executor's event
    // stream; the decisive proof is behavioral — the companion no-claim test
    // shows this exact write is DENIED without a grant, so "new" here can
    // only mean the claim was granted and enabled the scoped write.
    let _ = &events;
    assert_eq!(
        std::fs::read_to_string(dir.join("b.txt")).unwrap(),
        "new\n",
        "a granted claim must enable the scoped write"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// A write OUTSIDE the claimed scope stays denied after a grant.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_child_write_outside_its_claim_is_denied() {
    let dir = tmp("lbo-outside", 82);
    std::fs::write(dir.join("a.txt"), "keep\n").unwrap();
    std::fs::write(dir.join("b.txt"), "old\n").unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());
    let runtime = Arc::new(RoutedRuntime::new(
        vec![
            assistant_with(
                vec![spawn_call(
                    "s1",
                    serde_json::json!({
                        "task": format!("{CHILD_MARKER}: edit files")
                    }),
                )],
                FinishReason::ToolCalls,
            ),
            assistant_text("done"),
        ],
        vec![
            assistant_with(
                vec![tool_call_part(
                    "c1",
                    "claim_write_scope",
                    serde_json::json!({"paths": ["b.txt"]}),
                )],
                FinishReason::ToolCalls,
            ),
            // Outside the claim: must be denied.
            assistant_with(
                vec![patch_call("w1", "a.txt", "keep", "stolen")],
                FinishReason::ToolCalls,
            ),
            assistant_text("child done"),
        ],
        CHILD_MARKER,
        Duration::from_millis(10),
    ));
    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        8,
    );
    executor
        .run("edit", &mut |_| {}, &mut NoopSink, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).unwrap(),
        "keep\n",
        "a claim on b.txt must not authorize writes to a.txt"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Settlement releases the claim: a second child can then claim the same path
/// and be granted (no zombie ownership).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settlement_releases_the_claimed_scope_for_the_next_child() {
    let dir = tmp("lbo-release", 83);
    std::fs::write(dir.join("b.txt"), "one\n").unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());
    let runtime = Arc::new(RoutedRuntime::new(
        vec![
            assistant_with(
                vec![spawn_call(
                    "s1",
                    serde_json::json!({
                        "task": format!("{CHILD_MARKER}: step one")
                    }),
                )],
                FinishReason::ToolCalls,
            ),
            assistant_with(
                vec![spawn_call(
                    "s2",
                    serde_json::json!({
                        "task": format!("{CHILD_MARKER}: step two")
                    }),
                )],
                FinishReason::ToolCalls,
            ),
            assistant_text("done"),
        ],
        vec![
            // Child 1: claim + write + settle.
            assistant_with(
                vec![tool_call_part(
                    "c1",
                    "claim_write_scope",
                    serde_json::json!({"paths": ["b.txt"]}),
                )],
                FinishReason::ToolCalls,
            ),
            assistant_with(
                vec![patch_call("w1", "b.txt", "one", "two")],
                FinishReason::ToolCalls,
            ),
            assistant_text("first done"),
            // Child 2: the SAME path must be claimable after child 1 settled.
            assistant_with(
                vec![tool_call_part(
                    "c2",
                    "claim_write_scope",
                    serde_json::json!({"paths": ["b.txt"]}),
                )],
                FinishReason::ToolCalls,
            ),
            assistant_with(
                vec![patch_call("w2", "b.txt", "two", "three")],
                FinishReason::ToolCalls,
            ),
            assistant_text("second done"),
        ],
        CHILD_MARKER,
        Duration::from_millis(10),
    ));
    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        12,
    );
    executor
        .run(
            "two steps",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.join("b.txt")).unwrap(),
        "three\n",
        "the second child must be able to claim and write the released scope"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Marker-routed runtime with TWO independent child queues (and per-queue
/// delays), so two concurrent children are fully deterministic.
struct DualChildRuntime {
    parent: Mutex<VecDeque<ModelResponse>>,
    child1: Mutex<VecDeque<ModelResponse>>,
    child2: Mutex<VecDeque<ModelResponse>>,
    marker1: &'static str,
    marker2: &'static str,
    delay1: Duration,
    delay2: Duration,
}

#[async_trait]
impl ModelRuntime for DualChildRuntime {
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
        use leveler_model::ModelEvent;
        let text: String = request
            .messages
            .iter()
            .map(|m| m.text_content())
            .collect::<Vec<_>>()
            .join("\n");
        let (queue, delay) = if text.contains(self.marker2) {
            (&self.child2, self.delay2)
        } else if text.contains(self.marker1) {
            (&self.child1, self.delay1)
        } else {
            (&self.parent, Duration::ZERO)
        };
        tokio::time::sleep(delay).await;
        let response =
            queue.lock().unwrap().pop_front().ok_or_else(|| {
                ModelError::new(leveler_model::ModelErrorKind::Other, "queue empty")
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

    async fn profile(&self, _m: &ModelRef) -> Result<ModelProfile, ModelError> {
        unimplemented!()
    }
}

/// A denied claim is a coordination result, not a child failure: the child
/// keeps running, and the parent's edit inside the live claim is fenced too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_denied_claim_does_not_kill_the_child() {
    let dir = tmp("lbo-denied", 84);
    std::fs::write(dir.join("b.txt"), "old\n").unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());
    let runtime = Arc::new(DualChildRuntime {
        parent: Mutex::new(VecDeque::from(vec![
            // r1: legacy background worker pre-claims b.txt SYNCHRONOUSLY at
            // spawn admission (deterministic — no child round needed).
            assistant_with(
                vec![spawn_call_default(
                    "s1",
                    serde_json::json!({
                        "task": "HOLDER-CHILD: slow-edit b.txt",
                        "role": "worker",
                        "files": ["b.txt"]
                    }),
                )],
                FinishReason::ToolCalls,
            ),
            // r2: foreground default child that tries to claim the same path.
            assistant_with(
                vec![spawn_call(
                    "s2",
                    serde_json::json!({
                        "task": "CLAIMER-CHILD: also wants b.txt"
                    }),
                )],
                FinishReason::ToolCalls,
            ),
            assistant_text("waiting for the worker"),
            assistant_text("done"),
        ])),
        child1: Mutex::new(VecDeque::from(vec![
            assistant_with(
                vec![patch_call("w1", "b.txt", "old", "new")],
                FinishReason::ToolCalls,
            ),
            assistant_text("worker done"),
        ])),
        child2: Mutex::new(VecDeque::from(vec![
            assistant_with(
                vec![tool_call_part(
                    "c1",
                    "claim_write_scope",
                    serde_json::json!({"paths": ["b.txt"]}),
                )],
                FinishReason::ToolCalls,
            ),
            assistant_text("scope busy; narrowed my plan and reported instead"),
        ])),
        marker1: "HOLDER-CHILD",
        marker2: "CLAIMER-CHILD",
        delay1: Duration::from_millis(1200),
        delay2: Duration::from_millis(30),
    });
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
            "two children contend",
            &mut |e| events.push(e),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    // Child 2 finished normally (ok=true settlement), not a failure.
    let child2_ok = events.iter().any(|e| {
        matches!(e, AgentEvent::SubAgentFinished { nickname, ok: true, .. } if nickname == "Newton")
    });
    assert!(
        child2_ok,
        "a denied claim must leave the child alive to finish honestly"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// The debt resets once the parent ACTS after the settlement notice became
/// model-visible — a consumed settlement must not read as debt (and must not
/// buy a spurious continuation window upstream).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_settlement_the_parent_acts_on_is_consumed() {
    let dir = tmp("bg-debt-consume", 70);
    std::fs::write(dir.join("a.txt"), "parent\n").unwrap();
    std::fs::write(dir.join("b.txt"), "old\n").unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());
    let runtime = Arc::new(RoutedRuntime::new(
        vec![
            // r1: delegate.
            assistant_with(
                vec![spawn_call_default(
                    "s1",
                    serde_json::json!({
                        "task": format!("{CHILD_MARKER}: change old to new in b.txt"),
                        "role": "worker",
                        "files": ["b.txt"]
                    }),
                )],
                FinishReason::ToolCalls,
            ),
            // r2: quiet — the harness waits for the settlement notice.
            assistant_text("waiting on my delegation"),
            // r3: informed by the notice, the parent integrates (non-observe
            // success) — this consumes the settlement.
            assistant_with(
                vec![patch_call("e1", "a.txt", "parent", "parent-integrated")],
                FinishReason::ToolCalls,
            ),
            // r4: finish.
            assistant_text("integrated the worker result"),
        ],
        vec![
            assistant_with(
                vec![patch_call("w1", "b.txt", "old", "new")],
                FinishReason::ToolCalls,
            ),
            assistant_text("scoped edit done"),
        ],
        CHILD_MARKER,
        Duration::from_millis(150),
    ));
    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        12,
    );
    let outcome = executor
        .run(
            "delegate, then integrate the result",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("b.txt")).unwrap(), "new\n");
    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).unwrap(),
        "parent-integrated\n"
    );
    assert_eq!(
        outcome.progress.unconsumed_child_settlements, 0,
        "a settlement the parent acted on must not read as debt"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Review 必改A: a FOREGROUND spawn folds mid-batch, so its report is not
/// model-visible until the NEXT round — a successful sibling tool call in the
/// same batch must not consume that debt. Only settlements the model already
/// saw when the round's actions ran are consumable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_foreground_settlement_is_not_consumed_by_same_round_siblings() {
    let dir = tmp("bg-debt-foreground", 71);
    std::fs::write(dir.join("a.txt"), "parent\n").unwrap();
    std::fs::write(dir.join("b.txt"), "old\n").unwrap();
    let workspace = Workspace::new(&dir).unwrap();
    let tool_context = ToolContext::new(workspace, PermissionProfile::Assisted);
    let registry = Arc::new(default_registry());
    let runtime = Arc::new(RoutedRuntime::new(
        vec![
            // r1: foreground spawn AND a sibling patch in the SAME batch. The
            // child's report lands as this round's tool result — the model has
            // not seen it yet; the sibling patch is a non-observe success that
            // must NOT consume the just-folded settlement.
            assistant_with(
                vec![
                    spawn_call(
                        "s1",
                        serde_json::json!({
                            "task": format!("{CHILD_MARKER}: change old to new in b.txt"),
                            "role": "worker",
                            "files": ["b.txt"],
                            "run_in_background": false
                        }),
                    ),
                    patch_call("e1", "a.txt", "parent", "parent-edited"),
                ],
                FinishReason::ToolCalls,
            ),
            // r2/r3: observe-only rounds run out the window — the parent never
            // acts WITH the report in context.
            assistant_with(
                vec![tool_call_part(
                    "r1",
                    "read_file",
                    serde_json::json!({"path": "a.txt"}),
                )],
                FinishReason::ToolCalls,
            ),
            assistant_with(
                vec![tool_call_part(
                    "r2",
                    "read_file",
                    serde_json::json!({"path": "a.txt"}),
                )],
                FinishReason::ToolCalls,
            ),
        ],
        vec![
            assistant_with(
                vec![patch_call("w1", "b.txt", "old", "new")],
                FinishReason::ToolCalls,
            ),
            assistant_text("child done"),
        ],
        CHILD_MARKER,
        Duration::from_millis(20),
    ));
    let executor = Executor::new(
        runtime,
        registry,
        tool_context,
        ModelRef::new("mock", "m"),
        3,
    );
    let outcome = executor
        .run(
            "delegate in the foreground, then run out of window",
            &mut |_| {},
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("b.txt")).unwrap(), "new\n");
    assert_eq!(
        outcome.progress.unconsumed_child_settlements, 1,
        "a settlement the model has not seen must survive same-round consumption"
    );
    std::fs::remove_dir_all(&dir).ok();
}
