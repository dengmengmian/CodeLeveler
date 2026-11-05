//! Scheduled agent runs and native actions.
//!
//! An agent occurrence is handed to the real runtime as one `RunGoal`; the
//! schedule never runs an agent loop. Its outcome is the runtime's own
//! terminal record, recognized only when it is newer than the event-log
//! sequence observed before submission. Native occurrences go through the
//! injected host execution authority, and the schedule distinguishes what the
//! provider confirmed from what it merely accepted.
//!
//! These tests pin those contracts against a fake runtime at the transport
//! seam. A fake provider or executor is sufficient here because the property
//! under test is schedule-side settlement, correlation and authorization —
//! never that a model or the OS performed real work.

#![cfg(unix)]

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use leveler_client_protocol::{
    ApprovalPolicy, ClientCommand, ClientError, CommandEnvelope, InteractiveRuntimeClient,
    ModelRef, PermissionProfile, RuntimeEvent, SessionId, UiClarificationRequest,
    UiPendingInteraction, UiSessionSnapshot, UiTaskStatus, UiTaskTerminal,
};
use leveler_local_transport::{CreateSessionRequest, LocalRuntimeService, SessionBootstrap};
use leveler_schedule::{
    ActionKind, DispatchSummary, Dispatcher, NativeActionClass, NativeActionExecutor,
    NativeExecutionReport, NativeExecutionRequest, NativeRunStatus, NewScheduleDefinition,
    OverlapPolicy, RunState, RuntimeConnector, ScheduleAction, ScheduleStore, ScheduleTrigger,
    SessionPolicy, UnattendedRequest, UsageReader, UsageSnapshot,
};
use tokio::sync::broadcast;

const GOAL: &str = "Read the repository and summarize the last change";
const PROVIDER: &str = "deepseek";

#[derive(Clone)]
struct RuntimeState {
    task_status: Option<UiTaskStatus>,
    task_terminal: Option<UiTaskTerminal>,
    last_sequence: Option<i64>,
    turn_ordinal: i64,
    pending_interactions: Vec<UiPendingInteraction>,
}

impl Default for RuntimeState {
    fn default() -> Self {
        Self {
            task_status: None,
            task_terminal: None,
            last_sequence: None,
            turn_ordinal: 0,
            pending_interactions: Vec::new(),
        }
    }
}

struct AgentRuntime {
    session: SessionId,
    sessions: Mutex<Vec<SessionId>>,
    created: Mutex<Vec<CreateSessionRequest>>,
    delivered: Mutex<Vec<CommandEnvelope>>,
    policies: Mutex<Vec<(SessionId, ApprovalPolicy)>>,
    states: Mutex<HashMap<SessionId, RuntimeState>>,
    events: broadcast::Sender<RuntimeEvent>,
}

impl AgentRuntime {
    fn new(session: SessionId) -> Self {
        let (events, _) = broadcast::channel(8);
        Self {
            sessions: Mutex::new(vec![session.clone()]),
            session,
            created: Mutex::new(Vec::new()),
            delivered: Mutex::new(Vec::new()),
            policies: Mutex::new(Vec::new()),
            states: Mutex::new(HashMap::new()),
            events,
        }
    }

    fn snapshot_for(&self, session: &SessionId, state: &RuntimeState) -> UiSessionSnapshot {
        UiSessionSnapshot {
            history: None,
            id: session.clone(),
            repository: None,
            goal: GOAL.into(),
            task_status: state.task_status,
            task_terminal: state.task_terminal.clone(),
            model: None,
            mode: PermissionProfile::Assisted,
            branch: None,
            status: "idle".into(),
            finalization_stage: None,
            messages: Vec::new(),
            pending_interactions: state.pending_interactions.clone(),
            available_models: Vec::new(),
            vision: false,
            last_sequence: state.last_sequence,
            latest_turn_ordinal: (state.turn_ordinal > 0).then_some(state.turn_ordinal),
            active_tools: Vec::new(),
            interrupted_tool_calls: Vec::new(),
            active_background_tasks: Vec::new(),
            background_tasks: Vec::new(),
            plan: None,
            diff: None,
            checkpoints: Vec::new(),
            recaps: Vec::new(),
            user_shells: Vec::new(),
            completion_report: None,
            thinking: None,
            work_profile: None,
            collaboration: None,
            execution_task: None,
            children: Vec::new(),
        }
    }

    fn set_terminal(&self, sequence: i64, outcome: &str) {
        self.set_terminal_for(&self.session, sequence, outcome);
    }

    fn set_terminal_for(&self, session: &SessionId, sequence: i64, outcome: &str) {
        let mut states = self.states.lock().unwrap();
        let state = states.entry(session.clone()).or_default();
        state.last_sequence = Some(sequence);
        state.task_terminal = Some(UiTaskTerminal {
            sequence,
            outcome: outcome.into(),
            stop: None,
            reason: None,
            warnings: Vec::new(),
        });
        state.task_status = None;
    }

    fn set_pending_clarification(&self) {
        let mut states = self.states.lock().unwrap();
        let state = states.entry(self.session.clone()).or_default();
        state.task_status = Some(UiTaskStatus::WaitingUser);
        state.pending_interactions = vec![UiPendingInteraction::Clarification(
            UiClarificationRequest::single(
                leveler_client_protocol::ClarificationId::new("clarify-1"),
                "which branch?",
                Vec::new(),
            ),
        )];
    }

    /// Simulate another client starting a turn in the same session while no
    /// turn runs here. Returns the new turn's ordinal.
    fn start_foreign_turn(&self) -> i64 {
        self.start_foreign_turn_for(&self.session)
    }

    fn start_foreign_turn_for(&self, session: &SessionId) -> i64 {
        let mut states = self.states.lock().unwrap();
        let state = states.entry(session.clone()).or_default();
        state.turn_ordinal += 1;
        state.task_status = Some(UiTaskStatus::Running);
        state.task_terminal = None;
        state.pending_interactions.clear();
        state.turn_ordinal
    }

    fn goals(&self) -> Vec<CommandEnvelope> {
        self.delivered
            .lock()
            .unwrap()
            .iter()
            .filter(|envelope| matches!(envelope.command, ClientCommand::RunGoal { .. }))
            .cloned()
            .collect()
    }

    fn cancels(&self) -> Vec<CommandEnvelope> {
        self.delivered
            .lock()
            .unwrap()
            .iter()
            .filter(|envelope| matches!(envelope.command, ClientCommand::CancelCurrentTurn { .. }))
            .cloned()
            .collect()
    }

    fn created(&self) -> Vec<CreateSessionRequest> {
        self.created.lock().unwrap().clone()
    }
}

#[async_trait]
impl InteractiveRuntimeClient for AgentRuntime {
    async fn send(&self, _command: ClientCommand) -> Result<(), ClientError> {
        Ok(())
    }

    async fn deliver(&self, envelope: CommandEnvelope) -> Result<(), ClientError> {
        if let ClientCommand::RunGoal {
            require_new_turn, ..
        } = &envelope.command
        {
            let mut states = self.states.lock().unwrap();
            let state = states.entry(envelope.session_id.clone()).or_default();
            let busy = matches!(
                state.task_status,
                Some(UiTaskStatus::Running | UiTaskStatus::WaitingUser | UiTaskStatus::Answered)
            );
            if busy {
                if *require_new_turn {
                    return Err(ClientError::Runtime(
                        "the session already has an active turn; this delivery requires a \
                         new turn and does not join another client's work"
                            .into(),
                    ));
                }
            } else {
                state.turn_ordinal += 1;
                state.task_status = Some(UiTaskStatus::Running);
                state.task_terminal = None;
            }
        }
        self.delivered.lock().unwrap().push(envelope);
        Ok(())
    }

    fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
        self.events.subscribe()
    }

    async fn snapshot(&self, session_id: &SessionId) -> Result<UiSessionSnapshot, ClientError> {
        if !self.sessions.lock().unwrap().contains(session_id) {
            return Err(ClientError::SessionNotFound(session_id.clone()));
        }
        let state = self
            .states
            .lock()
            .unwrap()
            .get(session_id)
            .cloned()
            .unwrap_or_default();
        Ok(self.snapshot_for(session_id, &state))
    }
}

#[async_trait]
impl LocalRuntimeService for AgentRuntime {
    async fn create_session(
        &self,
        request: CreateSessionRequest,
    ) -> Result<SessionBootstrap, ClientError> {
        let mut created = self.created.lock().unwrap();
        created.push(request);
        let session = SessionId::new(format!("created-{}", created.len()));
        self.sessions.lock().unwrap().push(session.clone());
        let state = self
            .states
            .lock()
            .unwrap()
            .get(&session)
            .cloned()
            .unwrap_or_default();
        Ok(SessionBootstrap {
            session: self.snapshot_for(&session, &state),
            context_window: 0,
            notice: None,
        })
    }

    async fn attach_session_policy(
        &self,
        session_id: &SessionId,
        policy: ApprovalPolicy,
    ) -> Result<(), ClientError> {
        self.policies
            .lock()
            .unwrap()
            .push((session_id.clone(), policy));
        Ok(())
    }
}

struct StaticConnector {
    runtime: Arc<AgentRuntime>,
}

#[async_trait]
impl RuntimeConnector for StaticConnector {
    async fn connect(&self, _workspace: &Path) -> Result<Arc<dyn LocalRuntimeService>, String> {
        Ok(self.runtime.clone())
    }
}

type ClassifyHook = Box<dyn Fn(&str) + Send + Sync>;

struct ScriptedNative {
    external_side_effects: bool,
    reports: Mutex<VecDeque<NativeExecutionReport>>,
    executions: AtomicUsize,
    classify_hook: Mutex<Option<ClassifyHook>>,
}

impl ScriptedNative {
    fn new(external_side_effects: bool, reports: Vec<NativeExecutionReport>) -> Self {
        Self {
            external_side_effects,
            reports: Mutex::new(reports.into()),
            executions: AtomicUsize::new(0),
            classify_hook: Mutex::new(None),
        }
    }

    fn set_classify_hook(&self, hook: ClassifyHook) {
        *self.classify_hook.lock().unwrap() = Some(hook);
    }

    fn report(status: NativeRunStatus, detail: &str) -> NativeExecutionReport {
        NativeExecutionReport {
            status,
            provider: Some("fake-native".into()),
            execution_id: Some("exec-1".into()),
            exit_code: None,
            detail: detail.into(),
            capability_state: Some("available".into()),
            permission_state: Some(
                if status == NativeRunStatus::RequiresPermission {
                    "not_determined"
                } else {
                    "granted"
                }
                .into(),
            ),
            executed: status != NativeRunStatus::RequiresPermission,
        }
    }
}

#[async_trait]
impl NativeActionExecutor for ScriptedNative {
    fn classify(&self, action: &str) -> Result<NativeActionClass, String> {
        if let Some(hook) = self.classify_hook.lock().unwrap().as_ref() {
            hook(action);
        }
        Ok(NativeActionClass {
            external_side_effects: self.external_side_effects,
        })
    }

    async fn execute(
        &self,
        _request: NativeExecutionRequest,
    ) -> Result<NativeExecutionReport, String> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        self.reports
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| "no scripted report".to_string())
    }
}

struct ScriptedUsage {
    readings: Mutex<VecDeque<UsageSnapshot>>,
}

#[async_trait]
impl UsageReader for ScriptedUsage {
    async fn usage(
        &self,
        _workspace: &Path,
        _session: &SessionId,
    ) -> Result<UsageSnapshot, String> {
        self.readings
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| "no scripted usage reading".to_string())
    }
}

struct Fixture {
    store: ScheduleStore,
    definition_id: leveler_schedule::ScheduleId,
    _tmp: tempfile::TempDir,
}

async fn fixture(
    runtime: &Arc<AgentRuntime>,
    action: ScheduleAction,
    session_policy: SessionPolicy,
    overlap: OverlapPolicy,
    mode: PermissionProfile,
    unattended: Option<UnattendedRequest>,
) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let store = ScheduleStore::open(&tmp.path().join("schedules.db"))
        .await
        .unwrap();
    let definition = store
        .create_definition(NewScheduleDefinition {
            name: "scheduled work".into(),
            workspace,
            action,
            trigger: ScheduleTrigger::Once {
                at: leveler_core::now() - chrono::Duration::seconds(1),
            },
            session_policy,
            overlap,
            mode,
            model: Some(ModelRef::new(PROVIDER, "deepseek-chat")),
            timeout: Duration::from_secs(30),
            misfire: leveler_schedule::MisfirePolicy::Skip,
            misfire_grace: Duration::from_secs(300),
            unattended,
        })
        .await
        .unwrap();
    if session_policy == SessionPolicy::Continue {
        store
            .set_session(&definition.id, &runtime.session)
            .await
            .unwrap();
    }
    Fixture {
        definition_id: definition.id,
        store,
        _tmp: tmp,
    }
}

fn dispatcher(runtime: &Arc<AgentRuntime>, store: ScheduleStore) -> Dispatcher {
    Dispatcher::new(
        store,
        Arc::new(StaticConnector {
            runtime: runtime.clone(),
        }),
    )
    .with_lease(Duration::from_secs(30))
    .with_poll_interval(Duration::from_millis(10))
}

fn agent_action() -> ScheduleAction {
    ScheduleAction::Agent { goal: GOAL.into() }
}

fn grant() -> Option<UnattendedRequest> {
    Some(UnattendedRequest {
        allow_external_side_effects: true,
    })
}

async fn only_run(fixture: &Fixture) -> leveler_schedule::ScheduleRun {
    let runs = fixture
        .store
        .runs(Some(&fixture.definition_id), 10)
        .await
        .unwrap();
    assert_eq!(runs.len(), 1, "expected exactly one run: {runs:?}");
    runs[0].clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_goal_is_handed_off_once_and_settled_by_its_own_terminal() {
    let runtime = Arc::new(AgentRuntime::new(SessionId::new("sess-agent")));
    let fixture = fixture(
        &runtime,
        agent_action(),
        SessionPolicy::Continue,
        OverlapPolicy::SkipIfRunning,
        PermissionProfile::Assisted,
        grant(),
    )
    .await;

    let summary = dispatcher(&runtime, fixture.store.clone())
        .dispatch_due(1)
        .await
        .unwrap();
    assert_eq!(summary.claimed, 1, "{summary:?}");
    assert_eq!(summary.handed_off, 1, "{summary:?}");
    assert_eq!(summary.succeeded, 0, "handoff is not success: {summary:?}");

    let run = only_run(&fixture).await;
    assert_eq!(run.state, RunState::Running);
    assert!(run.delivered_at.is_some());
    assert!(run.handed_off_at.is_some());
    assert_eq!(run.baseline_sequence, Some(0));

    let goals = runtime.goals();
    assert_eq!(goals.len(), 1, "one occurrence delivers one goal");
    assert_eq!(
        goals[0].command_id.as_str(),
        format!("schedule-goal-{}", run.id)
    );
    match &goals[0].command {
        ClientCommand::RunGoal { content, .. } => assert_eq!(content, GOAL),
        other => panic!("expected RunGoal, got {other:?}"),
    }
    assert_eq!(
        runtime.policies.lock().unwrap().as_slice(),
        &[
            (runtime.session.clone(), ApprovalPolicy::AutoApprove),
            (runtime.session.clone(), ApprovalPolicy::Interactive),
        ],
        "an existing session is re-asserted unattended before the run and \
         returned to interactive once the turn owns its approver"
    );

    runtime.set_terminal(1, "completed");
    let summary = dispatcher(&runtime, fixture.store.clone())
        .dispatch_due(1)
        .await
        .unwrap();
    assert_eq!(summary.succeeded, 1, "{summary:?}");

    let run = only_run(&fixture).await;
    assert_eq!(run.state, RunState::Succeeded);
    assert_eq!(run.task_sequence, Some(1));
    assert_eq!(run.task_outcome.as_deref(), Some("completed"));
    assert_eq!(run.provider.as_deref(), Some(PROVIDER));
    assert!(run.finished_at.is_some());
    assert_eq!(
        runtime.goals().len(),
        1,
        "observation never re-delivers the goal"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_previous_terminal_record_does_not_settle_the_new_run() {
    let runtime = Arc::new(AgentRuntime::new(SessionId::new("sess-agent")));
    runtime.set_terminal(10, "completed");
    let fixture = fixture(
        &runtime,
        agent_action(),
        SessionPolicy::Continue,
        OverlapPolicy::SkipIfRunning,
        PermissionProfile::Assisted,
        grant(),
    )
    .await;

    let summary = dispatcher(&runtime, fixture.store.clone())
        .dispatch_due(1)
        .await
        .unwrap();
    assert_eq!(summary.handed_off, 1, "{summary:?}");

    let run = only_run(&fixture).await;
    assert_eq!(run.baseline_sequence, Some(10));

    // The previous turn's terminal is still visible in the session. It is not
    // this run's terminal, so it must not settle it.
    let summary = dispatcher(&runtime, fixture.store.clone())
        .dispatch_due(1)
        .await
        .unwrap();
    assert_eq!(summary.handed_off, 0, "{summary:?}");
    assert_eq!(summary.succeeded, 0, "{summary:?}");
    assert_eq!(only_run(&fixture).await.state, RunState::Running);

    runtime.set_terminal(11, "completed");
    let summary = dispatcher(&runtime, fixture.store.clone())
        .dispatch_due(1)
        .await
        .unwrap();
    assert_eq!(summary.succeeded, 1, "{summary:?}");
    let run = only_run(&fixture).await;
    assert_eq!(run.state, RunState::Succeeded);
    assert_eq!(run.task_sequence, Some(11));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_run_without_an_unattended_grant_is_blocked_before_any_session_work() {
    let runtime = Arc::new(AgentRuntime::new(SessionId::new("sess-agent")));
    let fixture = fixture(
        &runtime,
        agent_action(),
        SessionPolicy::Continue,
        OverlapPolicy::SkipIfRunning,
        PermissionProfile::Assisted,
        None,
    )
    .await;

    let summary = dispatcher(&runtime, fixture.store.clone())
        .dispatch_due(1)
        .await
        .unwrap();
    assert_eq!(summary.blocked, 1, "{summary:?}");

    let run = only_run(&fixture).await;
    assert_eq!(run.state, RunState::Blocked);
    assert!(
        run.error
            .as_deref()
            .is_some_and(|error| error.contains("unattended authorization")),
        "{run:?}"
    );
    assert!(runtime.goals().is_empty());
    assert!(runtime.created().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fresh_policy_creates_one_session_per_run_without_binding_the_definition() {
    let runtime = Arc::new(AgentRuntime::new(SessionId::new("sess-agent")));
    let fixture = fixture(
        &runtime,
        agent_action(),
        SessionPolicy::Fresh,
        OverlapPolicy::AllowConcurrent,
        PermissionProfile::Assisted,
        grant(),
    )
    .await;

    let first = dispatcher(&runtime, fixture.store.clone())
        .dispatch_due(1)
        .await
        .unwrap();
    assert_eq!(first.handed_off, 1, "{first:?}");

    let definition = fixture
        .store
        .definition(&fixture.definition_id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        definition.session_id.is_none(),
        "a fresh run must not bind its session to the definition"
    );

    fixture.store.create_run_now(&definition).await.unwrap();
    let second = dispatcher(&runtime, fixture.store.clone())
        .dispatch_due(1)
        .await
        .unwrap();
    assert_eq!(second.handed_off, 1, "{second:?}");

    let created = runtime.created();
    assert_eq!(created.len(), 2, "each fresh run creates its own session");
    assert!(
        created
            .iter()
            .all(|request| request.approval_policy == ApprovalPolicy::AutoApprove),
        "the persisted grant, not an interactive prompt, is the policy: {created:?}"
    );
    assert_ne!(
        created[0].request_id, created[1].request_id,
        "the creation identity is stable per run, not shared across runs"
    );
    assert_eq!(runtime.goals().len(), 2);

    let runs = fixture
        .store
        .runs(Some(&fixture.definition_id), 10)
        .await
        .unwrap();
    assert_eq!(runs.len(), 2);
    assert_ne!(runs[0].session_id, runs[1].session_id);
    assert!(
        runtime.policies.lock().unwrap().is_empty(),
        "fresh sessions get their policy at creation; no attach is needed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_interaction_settles_waiting_approval_and_cancels_the_turn() {
    let runtime = Arc::new(AgentRuntime::new(SessionId::new("sess-agent")));
    let fixture = fixture(
        &runtime,
        agent_action(),
        SessionPolicy::Continue,
        OverlapPolicy::SkipIfRunning,
        PermissionProfile::Assisted,
        grant(),
    )
    .await;

    dispatcher(&runtime, fixture.store.clone())
        .dispatch_due(1)
        .await
        .unwrap();
    runtime.set_pending_clarification();

    let summary = dispatcher(&runtime, fixture.store.clone())
        .dispatch_due(1)
        .await
        .unwrap();
    assert_eq!(summary.waiting_approval, 1, "{summary:?}");

    let run = only_run(&fixture).await;
    assert_eq!(run.state, RunState::WaitingApproval);
    assert!(
        run.error
            .as_deref()
            .is_some_and(|error| error.contains("waiting for a person (clarification)")),
        "{run:?}"
    );
    assert_eq!(runtime.cancels().len(), 1);
    assert!(run.cancel_issued_at.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_request_is_sent_once_and_the_runtime_terminal_settles_cancelled() {
    let runtime = Arc::new(AgentRuntime::new(SessionId::new("sess-agent")));
    let fixture = fixture(
        &runtime,
        agent_action(),
        SessionPolicy::Continue,
        OverlapPolicy::SkipIfRunning,
        PermissionProfile::Assisted,
        grant(),
    )
    .await;

    dispatcher(&runtime, fixture.store.clone())
        .dispatch_due(1)
        .await
        .unwrap();
    let requested = fixture
        .store
        .request_cancel_definition(&fixture.definition_id)
        .await
        .unwrap();
    assert_eq!(requested.len(), 1);

    // The first pass after the request tells the runtime; it does not invent
    // a cancelled result.
    dispatcher(&runtime, fixture.store.clone())
        .dispatch_due(1)
        .await
        .unwrap();
    let run = only_run(&fixture).await;
    assert_eq!(run.state, RunState::Running);
    assert!(run.cancel_requested_at.is_some());
    assert!(run.cancel_issued_at.is_some());
    assert_eq!(runtime.cancels().len(), 1);

    // A later pass must not repeat an already-issued cancellation.
    dispatcher(&runtime, fixture.store.clone())
        .dispatch_due(1)
        .await
        .unwrap();
    assert_eq!(runtime.cancels().len(), 1);

    runtime.set_terminal(3, "cancelled");
    let summary = dispatcher(&runtime, fixture.store.clone())
        .dispatch_due(1)
        .await
        .unwrap();
    assert_eq!(summary.cancelled, 1, "{summary:?}");
    let run = only_run(&fixture).await;
    assert_eq!(run.state, RunState::Cancelled);
    assert_eq!(run.task_outcome.as_deref(), Some("cancelled"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_delta_attributes_only_the_run() {
    let runtime = Arc::new(AgentRuntime::new(SessionId::new("sess-agent")));
    let fixture = fixture(
        &runtime,
        agent_action(),
        SessionPolicy::Continue,
        OverlapPolicy::SkipIfRunning,
        PermissionProfile::Assisted,
        grant(),
    )
    .await;
    let baseline = UsageSnapshot {
        requests: 1,
        input_tokens: 100,
        cached_input_tokens: 40,
        output_tokens: 10,
        reasoning_tokens: 2,
        cost_usd_micros: 1_000,
        rows_without_cost: 0,
    };
    let final_usage = UsageSnapshot {
        requests: 3,
        input_tokens: 250,
        cached_input_tokens: 90,
        output_tokens: 40,
        reasoning_tokens: 12,
        cost_usd_micros: 4_000,
        rows_without_cost: 0,
    };
    let usage = Arc::new(ScriptedUsage {
        readings: Mutex::new(VecDeque::from([baseline, final_usage])),
    });

    let dispatcher = dispatcher(&runtime, fixture.store.clone()).with_usage_reader(usage);
    dispatcher.dispatch_due(1).await.unwrap();
    assert_eq!(only_run(&fixture).await.usage_baseline, Some(baseline));

    runtime.set_terminal(1, "completed");
    dispatcher.dispatch_due(1).await.unwrap();

    let run = only_run(&fixture).await;
    assert_eq!(run.state, RunState::Succeeded);
    assert_eq!(run.usage_requests, Some(2));
    assert_eq!(run.input_tokens, Some(150));
    assert_eq!(run.cached_input_tokens, Some(50));
    assert_eq!(run.output_tokens, Some(30));
    assert_eq!(run.reasoning_tokens, Some(10));
    assert_eq!(run.cost_usd_micros, Some(3_000));
    assert_eq!(
        run.usage_baseline,
        Some(baseline),
        "the session's earlier turns remain visible as the baseline, not charged to this run"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_now_is_not_starved_by_an_unrelated_overdue_slot() {
    let runtime = Arc::new(AgentRuntime::new(SessionId::new("sess-agent")));
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let store = ScheduleStore::open(&tmp.path().join("schedules.db"))
        .await
        .unwrap();

    let create = |name: &'static str, trigger: ScheduleTrigger| {
        let store = store.clone();
        let workspace = workspace.clone();
        async move {
            store
                .create_definition(NewScheduleDefinition {
                    name: name.into(),
                    workspace,
                    action: agent_action(),
                    trigger,
                    session_policy: SessionPolicy::Continue,
                    overlap: OverlapPolicy::SkipIfRunning,
                    mode: PermissionProfile::Assisted,
                    model: Some(ModelRef::new(PROVIDER, "deepseek-chat")),
                    timeout: Duration::from_secs(30),
                    misfire: leveler_schedule::MisfirePolicy::Skip,
                    misfire_grace: Duration::from_secs(300),
                    unattended: grant(),
                })
                .await
                .unwrap()
        }
    };
    let overdue = create(
        "overdue",
        ScheduleTrigger::Once {
            at: leveler_core::now() - chrono::Duration::seconds(1),
        },
    )
    .await;
    let target = create(
        "target",
        ScheduleTrigger::Once {
            at: leveler_core::now() + chrono::Duration::hours(1),
        },
    )
    .await;
    store
        .set_session(&overdue.id, &runtime.session)
        .await
        .unwrap();
    store
        .set_session(&target.id, &runtime.session)
        .await
        .unwrap();

    let occurrence = store.create_run_now(&target).await.unwrap();
    let dispatcher = dispatcher(&runtime, store.clone());
    let state = dispatcher.dispatch_run_now(&occurrence.id).await.unwrap();
    assert_eq!(state, Some(RunState::Running));

    let target_runs = store.runs(Some(&target.id), 10).await.unwrap();
    assert_eq!(target_runs.len(), 1);
    assert_eq!(target_runs[0].state, RunState::Running);
    assert!(
        store.runs(Some(&overdue.id), 10).await.unwrap().is_empty(),
        "the unrelated overdue slot was not executed by the run-now command"
    );
    assert_eq!(runtime.goals().len(), 1);

    // The overlap policy still applies to an explicit slot: while that run is
    // active, a second run-now is recorded as skipped, not executed.
    let second = store.create_run_now(&target).await.unwrap();
    let state = dispatcher.dispatch_run_now(&second.id).await.unwrap();
    assert_eq!(state, None);
    let runs = store.runs(Some(&target.id), 10).await.unwrap();
    assert!(
        runs.iter().any(|run| run.state == RunState::Skipped),
        "{runs:?}"
    );
    assert_eq!(runtime.goals().len(), 1);
}

fn native_action() -> ScheduleAction {
    ScheduleAction::Native {
        action: "mail.send".into(),
        input: serde_json::json!({ "to": "controlled@example.test" }),
    }
}

async fn native_fixture(
    runtime: &Arc<AgentRuntime>,
    executor: Arc<ScriptedNative>,
    unattended: Option<UnattendedRequest>,
) -> (Fixture, Dispatcher) {
    let fixture = fixture(
        runtime,
        native_action(),
        SessionPolicy::Continue,
        OverlapPolicy::SkipIfRunning,
        PermissionProfile::Assisted,
        unattended,
    )
    .await;
    let dispatcher = dispatcher(runtime, fixture.store.clone()).with_native_executor(executor);
    (fixture, dispatcher)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_submitted_is_not_success() {
    let runtime = Arc::new(AgentRuntime::new(SessionId::new("sess-agent")));
    let executor = Arc::new(ScriptedNative::new(
        true,
        vec![ScriptedNative::report(
            NativeRunStatus::Submitted,
            "the provider accepted the message; delivery is not confirmable",
        )],
    ));
    let (fixture, dispatcher) = native_fixture(&runtime, executor.clone(), grant()).await;

    let summary: DispatchSummary = dispatcher.dispatch_due(1).await.unwrap();
    assert_eq!(summary.submitted, 1, "{summary:?}");
    assert_eq!(summary.succeeded, 0, "{summary:?}");
    assert_eq!(executor.executions.load(Ordering::SeqCst), 1);

    let run = only_run(&fixture).await;
    assert_eq!(run.state, RunState::Submitted);
    assert_eq!(run.native_outcome.as_deref(), Some("submitted"));
    assert_eq!(run.provider.as_deref(), Some("fake-native"));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_requiring_permission_settles_waiting_approval() {
    let runtime = Arc::new(AgentRuntime::new(SessionId::new("sess-agent")));
    let executor = Arc::new(ScriptedNative::new(
        true,
        vec![ScriptedNative::report(
            NativeRunStatus::RequiresPermission,
            "automation permission has not been granted",
        )],
    ));
    let (fixture, dispatcher) = native_fixture(&runtime, executor, grant()).await;

    let summary = dispatcher.dispatch_due(1).await.unwrap();
    assert_eq!(summary.waiting_approval, 1, "{summary:?}");

    let run = only_run(&fixture).await;
    assert_eq!(run.state, RunState::WaitingApproval);
    assert_eq!(run.permission_state.as_deref(), Some("not_determined"));
    assert!(
        run.error
            .as_deref()
            .is_some_and(|error| error.contains("automation permission")),
        "{run:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_without_side_effect_authorization_never_reaches_the_authority() {
    let runtime = Arc::new(AgentRuntime::new(SessionId::new("sess-agent")));
    let executor = Arc::new(ScriptedNative::new(
        true,
        vec![ScriptedNative::report(NativeRunStatus::Completed, "sent")],
    ));
    let (fixture, dispatcher) = native_fixture(
        &runtime,
        executor.clone(),
        Some(UnattendedRequest {
            allow_external_side_effects: false,
        }),
    )
    .await;

    let summary = dispatcher.dispatch_due(1).await.unwrap();
    assert_eq!(summary.blocked, 1, "{summary:?}");
    assert_eq!(
        executor.executions.load(Ordering::SeqCst),
        0,
        "authorization is checked before the host execution authority starts a process"
    );

    let run = only_run(&fixture).await;
    assert_eq!(run.state, RunState::Blocked);
    assert!(
        run.error
            .as_deref()
            .is_some_and(|error| error.contains("did not authorize")),
        "{run:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_grant_revoked_after_the_claim_cannot_start_the_action() {
    let runtime = Arc::new(AgentRuntime::new(SessionId::new("sess-agent")));
    let executor = Arc::new(ScriptedNative::new(
        true,
        vec![ScriptedNative::report(NativeRunStatus::Completed, "sent")],
    ));
    let (fixture, dispatcher) = native_fixture(&runtime, executor.clone(), grant()).await;

    // The occurrence is already claimed when classification runs. Revoking the
    // grant here is the last moment before the authorization decision; the
    // dispatcher must read the current grant, not the one it claimed with.
    let store = fixture.store.clone();
    let definition_id = fixture.definition_id.clone();
    executor.set_classify_hook(Box::new(move |_action| {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                store.revoke_unattended(&definition_id).await.unwrap();
            });
        });
    }));

    let summary = dispatcher.dispatch_due(1).await.unwrap();
    assert_eq!(summary.blocked, 1, "{summary:?}");
    assert_eq!(
        executor.executions.load(Ordering::SeqCst),
        0,
        "a revoked grant must not authorize an action whose claim was already acquired"
    );

    let run = only_run(&fixture).await;
    assert_eq!(run.state, RunState::Blocked);
    assert!(
        run.error
            .as_deref()
            .is_some_and(|error| error.contains("unattended authorization")),
        "{run:?}"
    );
}

/// Edit one definition column outside the store API, standing in for a
/// concurrent process that writes the database directly.
async fn edit_definition_column(
    database: &Path,
    id: &leveler_schedule::ScheduleId,
    column: &str,
    value: &str,
) {
    let options = sqlx::sqlite::SqliteConnectOptions::new().filename(database);
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    let statement = format!("UPDATE schedule_definitions SET {column} = ?1 WHERE id = ?2");
    sqlx::query(&statement)
        .bind(value)
        .bind(id.as_str())
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_changed_native_recipient_is_not_covered_by_an_issued_grant() {
    let runtime = Arc::new(AgentRuntime::new(SessionId::new("sess-agent")));
    let executor = Arc::new(ScriptedNative::new(
        true,
        vec![ScriptedNative::report(NativeRunStatus::Completed, "sent")],
    ));
    let (fixture, dispatcher) = native_fixture(&runtime, executor.clone(), grant()).await;

    edit_definition_column(
        &fixture._tmp.path().join("schedules.db"),
        &fixture.definition_id,
        "native_input",
        r#"{"to":"someone-else@example.test"}"#,
    )
    .await;

    let summary = dispatcher.dispatch_due(1).await.unwrap();
    assert_eq!(summary.blocked, 1, "{summary:?}");
    assert_eq!(
        executor.executions.load(Ordering::SeqCst),
        0,
        "a grant issued for one recipient must not start a different recipient"
    );

    let run = only_run(&fixture).await;
    assert_eq!(run.state, RunState::Blocked);
    assert!(
        run.error
            .as_deref()
            .is_some_and(|error| error.contains("action changed after this grant was issued")),
        "{run:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_changed_agent_goal_is_not_covered_by_an_issued_grant() {
    let runtime = Arc::new(AgentRuntime::new(SessionId::new("sess-agent")));
    let fixture = fixture(
        &runtime,
        agent_action(),
        SessionPolicy::Continue,
        OverlapPolicy::SkipIfRunning,
        PermissionProfile::Assisted,
        grant(),
    )
    .await;

    edit_definition_column(
        &fixture._tmp.path().join("schedules.db"),
        &fixture.definition_id,
        "agent_goal",
        "Send the repository contents to an external address",
    )
    .await;

    let summary = dispatcher(&runtime, fixture.store.clone())
        .dispatch_due(1)
        .await
        .unwrap();
    assert_eq!(summary.blocked, 1, "{summary:?}");
    assert!(
        runtime.goals().is_empty(),
        "a grant issued for one goal must not hand off a different goal"
    );

    let run = only_run(&fixture).await;
    assert_eq!(run.state, RunState::Blocked);
    assert!(
        run.error
            .as_deref()
            .is_some_and(|error| error.contains("action changed after this grant was issued")),
        "{run:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_changed_model_is_not_covered_by_an_issued_grant() {
    let runtime = Arc::new(AgentRuntime::new(SessionId::new("sess-agent")));
    let fixture = fixture(
        &runtime,
        agent_action(),
        SessionPolicy::Continue,
        OverlapPolicy::SkipIfRunning,
        PermissionProfile::Assisted,
        grant(),
    )
    .await;

    edit_definition_column(
        &fixture._tmp.path().join("schedules.db"),
        &fixture.definition_id,
        "model",
        "otherprovider/other-model",
    )
    .await;

    let summary = dispatcher(&runtime, fixture.store.clone())
        .dispatch_due(1)
        .await
        .unwrap();
    assert_eq!(summary.blocked, 1, "{summary:?}");
    assert!(runtime.goals().is_empty(), "no goal may be delivered");

    let run = only_run(&fixture).await;
    assert_eq!(run.state, RunState::Blocked);
    assert!(
        run.error
            .as_deref()
            .is_some_and(|error| error.contains("model changed after this grant was issued")),
        "{run:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_access_admits_the_action_but_cannot_override_an_os_permission_denial() {
    let runtime = Arc::new(AgentRuntime::new(SessionId::new("sess-agent")));
    let executor = Arc::new(ScriptedNative::new(
        true,
        vec![ScriptedNative::report(
            NativeRunStatus::RequiresPermission,
            "automation permission has not been granted",
        )],
    ));
    let fixture = fixture(
        &runtime,
        native_action(),
        SessionPolicy::Continue,
        OverlapPolicy::SkipIfRunning,
        PermissionProfile::FullAccess,
        None,
    )
    .await;
    let dispatcher = dispatcher(&runtime, fixture.store.clone()).with_native_executor(executor.clone());

    let summary = dispatcher.dispatch_due(1).await.unwrap();
    assert_eq!(
        executor.executions.load(Ordering::SeqCst),
        1,
        "Full access is the product's own admission decision; it does not refuse the action"
    );
    assert_eq!(
        summary.waiting_approval, 1,
        "the OS permission answer still decides the outcome: {summary:?}"
    );

    let run = only_run(&fixture).await;
    assert_eq!(run.state, RunState::WaitingApproval);
    assert_eq!(run.permission_state.as_deref(), Some("not_determined"));
    assert!(
        run.error
            .as_deref()
            .is_some_and(|error| error.contains("automation permission")),
        "a host-side permission boundary is reported, never converted into success: {run:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_continue_session_returns_to_interactive_policy_after_handoff() {
    let runtime = Arc::new(AgentRuntime::new(SessionId::new("sess-agent")));
    let fixture = fixture(
        &runtime,
        agent_action(),
        SessionPolicy::Continue,
        OverlapPolicy::SkipIfRunning,
        PermissionProfile::Assisted,
        grant(),
    )
    .await;

    dispatcher(&runtime, fixture.store.clone())
        .dispatch_due(1)
        .await
        .unwrap();

    assert_eq!(
        runtime.policies.lock().unwrap().as_slice(),
        &[
            (runtime.session.clone(), ApprovalPolicy::AutoApprove),
            (runtime.session.clone(), ApprovalPolicy::Interactive),
        ],
        "the unattended policy must not outlive the turn it was attached for; a \
         person resuming this session later must be asked again"
    );
}

struct SessionUsage {
    readings: Mutex<HashMap<SessionId, VecDeque<UsageSnapshot>>>,
}

impl SessionUsage {
    fn new(readings: Vec<(SessionId, Vec<UsageSnapshot>)>) -> Self {
        Self {
            readings: Mutex::new(
                readings
                    .into_iter()
                    .map(|(session, values)| (session, values.into()))
                    .collect(),
            ),
        }
    }
}

#[async_trait]
impl UsageReader for SessionUsage {
    async fn usage(
        &self,
        _workspace: &Path,
        session: &SessionId,
    ) -> Result<UsageSnapshot, String> {
        self.readings
            .lock()
            .unwrap()
            .get_mut(session)
            .and_then(|queue| queue.pop_front())
            .ok_or_else(|| format!("no scripted usage reading for {}", session.as_str()))
    }
}

async fn create_agent_definition(
    store: &ScheduleStore,
    workspace: &Path,
    goal: &str,
    session: &SessionId,
) -> leveler_schedule::ScheduleId {
    let definition = store
        .create_definition(NewScheduleDefinition {
            name: format!("scheduled {goal}"),
            workspace: workspace.to_path_buf(),
            action: ScheduleAction::Agent { goal: goal.into() },
            trigger: ScheduleTrigger::Once {
                at: leveler_core::now() - chrono::Duration::seconds(1),
            },
            session_policy: SessionPolicy::Continue,
            overlap: OverlapPolicy::SkipIfRunning,
            mode: PermissionProfile::Assisted,
            model: Some(ModelRef::new(PROVIDER, "deepseek-chat")),
            timeout: Duration::from_secs(30),
            misfire: leveler_schedule::MisfirePolicy::Skip,
            misfire_grace: Duration::from_secs(300),
            unattended: grant(),
        })
        .await
        .unwrap();
    store
        .set_session(&definition.id, session)
        .await
        .unwrap();
    definition.id
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_running_human_turn_is_skipped_not_steered_into_or_cancelled() {
    let runtime = Arc::new(AgentRuntime::new(SessionId::new("sess-agent")));
    runtime.start_foreign_turn();
    let fixture = fixture(
        &runtime,
        agent_action(),
        SessionPolicy::Continue,
        OverlapPolicy::SkipIfRunning,
        PermissionProfile::Assisted,
        grant(),
    )
    .await;

    let summary = dispatcher(&runtime, fixture.store.clone())
        .dispatch_due(1)
        .await
        .unwrap();
    assert_eq!(summary.skipped, 1, "{summary:?}");
    assert_eq!(summary.handed_off, 0, "{summary:?}");
    assert!(
        runtime.goals().is_empty(),
        "an unattended run must not join a person's turn"
    );
    assert!(
        runtime.cancels().is_empty(),
        "skipping must not stop the person's work either"
    );

    let run = only_run(&fixture).await;
    assert_eq!(run.state, RunState::Skipped);
    let states = runtime.states.lock().unwrap();
    let human = states.get(&runtime.session).unwrap();
    assert_eq!(human.task_status, Some(UiTaskStatus::Running));
    assert_eq!(human.turn_ordinal, 1, "the person's turn is untouched");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_later_turn_in_the_same_session_cannot_settle_the_run() {
    let runtime = Arc::new(AgentRuntime::new(SessionId::new("sess-agent")));
    let fixture = fixture(
        &runtime,
        agent_action(),
        SessionPolicy::Continue,
        OverlapPolicy::SkipIfRunning,
        PermissionProfile::Assisted,
        grant(),
    )
    .await;

    dispatcher(&runtime, fixture.store.clone())
        .dispatch_due(1)
        .await
        .unwrap();
    let run = only_run(&fixture).await;
    assert_eq!(run.turn_ordinal, Some(1), "the delivered turn is recorded");

    // This run's turn finishes while no dispatcher observes it, then a person
    // starts and finishes their own turn in the same session.
    runtime.set_terminal(11, "completed");
    let foreign = runtime.start_foreign_turn();
    assert_eq!(foreign, 2);
    runtime.set_terminal(20, "failed");

    let summary = dispatcher(&runtime, fixture.store.clone())
        .dispatch_due(1)
        .await
        .unwrap();
    assert_eq!(summary.succeeded, 0, "{summary:?}");
    assert_eq!(summary.failed, 0, "{summary:?}");

    let run = only_run(&fixture).await;
    assert_eq!(
        run.state,
        RunState::Running,
        "a terminal from another turn cannot settle this run: {run:?}"
    );
    assert_eq!(run.task_sequence, None);
    assert_eq!(run.task_outcome, None);
    assert_eq!(run.input_tokens, None, "no usage is attributed");
    assert!(
        runtime.cancels().is_empty(),
        "the person's turn is never cancelled on behalf of this run"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restarted_dispatcher_settles_a_long_run_without_redelivering() {
    let runtime = Arc::new(AgentRuntime::new(SessionId::new("sess-agent")));
    let fixture = fixture(
        &runtime,
        agent_action(),
        SessionPolicy::Continue,
        OverlapPolicy::SkipIfRunning,
        PermissionProfile::Assisted,
        grant(),
    )
    .await;

    // The dispatcher that delivered the goal exits; a new process opens the
    // same store and observes the still-running turn.
    dispatcher(&runtime, fixture.store.clone())
        .dispatch_due(1)
        .await
        .unwrap();
    let late = dispatcher(&runtime, fixture.store.clone());
    let summary = late.dispatch_due(1).await.unwrap();
    assert_eq!(summary.handed_off, 0, "{summary:?}");
    assert_eq!(
        runtime.goals().len(),
        1,
        "reconciliation never delivers the goal twice"
    );
    assert_eq!(only_run(&fixture).await.state, RunState::Running);

    runtime.set_terminal(7, "completed");
    let summary = late.dispatch_due(1).await.unwrap();
    assert_eq!(summary.succeeded, 1, "{summary:?}");
    let run = only_run(&fixture).await;
    assert_eq!(run.state, RunState::Succeeded);
    assert_eq!(run.task_sequence, Some(7));
    assert_eq!(run.turn_ordinal, Some(1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancel_after_the_dispatcher_exited_targets_the_runs_own_turn() {
    let runtime = Arc::new(AgentRuntime::new(SessionId::new("sess-agent")));
    let fixture = fixture(
        &runtime,
        agent_action(),
        SessionPolicy::Continue,
        OverlapPolicy::SkipIfRunning,
        PermissionProfile::Assisted,
        grant(),
    )
    .await;

    dispatcher(&runtime, fixture.store.clone())
        .dispatch_due(1)
        .await
        .unwrap();
    let run = only_run(&fixture).await;
    fixture.store.request_cancel(&run.id).await.unwrap();

    // A new dispatcher process carries out the request for this run's turn.
    let late = dispatcher(&runtime, fixture.store.clone());
    late.dispatch_due(1).await.unwrap();
    assert_eq!(
        runtime.cancels().len(),
        1,
        "the request is delivered once even though its process is gone"
    );
    assert!(only_run(&fixture).await.cancel_issued_at.is_some());

    runtime.set_terminal(12, "cancelled");
    late.dispatch_due(1).await.unwrap();
    let run = only_run(&fixture).await;
    assert_eq!(run.state, RunState::Cancelled);
    assert_eq!(run.task_sequence, Some(12));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancel_for_an_unowned_turn_never_cancels_the_session() {
    let runtime = Arc::new(AgentRuntime::new(SessionId::new("sess-agent")));
    let fixture = fixture(
        &runtime,
        agent_action(),
        SessionPolicy::Continue,
        OverlapPolicy::SkipIfRunning,
        PermissionProfile::Assisted,
        grant(),
    )
    .await;

    dispatcher(&runtime, fixture.store.clone())
        .dispatch_due(1)
        .await
        .unwrap();
    let run = only_run(&fixture).await;
    // This run's turn is already over; a person's newer turn now owns the
    // session. A session-scoped cancel would stop that person's work.
    runtime.set_terminal(11, "completed");
    runtime.start_foreign_turn();
    fixture.store.request_cancel(&run.id).await.unwrap();

    let summary = dispatcher(&runtime, fixture.store.clone())
        .dispatch_due(1)
        .await
        .unwrap();
    assert_eq!(summary.cancelled, 0, "{summary:?}");
    assert!(
        runtime.cancels().is_empty(),
        "an unowned session turn must never be cancelled"
    );
    assert_eq!(only_run(&fixture).await.state, RunState::Running);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_schedules_in_one_workspace_settle_their_own_terminals_and_usage() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let store = ScheduleStore::open(&tmp.path().join("schedules.db"))
        .await
        .unwrap();
    let session_a = SessionId::new("sess-a");
    let session_b = SessionId::new("sess-b");
    let runtime = Arc::new(AgentRuntime::new(session_a.clone()));
    runtime.sessions.lock().unwrap().push(session_b.clone());

    let a = create_agent_definition(&store, &workspace, "goal a", &session_a).await;
    let b = create_agent_definition(&store, &workspace, "goal b", &session_b).await;

    let baseline = |requests: u64, input: u64| UsageSnapshot {
        requests,
        input_tokens: input,
        ..UsageSnapshot::default()
    };
    let usage = Arc::new(SessionUsage::new(vec![
        (
            session_a.clone(),
            vec![baseline(1, 100), baseline(3, 250)],
        ),
        (
            session_b.clone(),
            vec![baseline(10, 1_000), baseline(14, 1_400)],
        ),
    ]));
    let dispatcher = dispatcher(&runtime, store.clone()).with_usage_reader(usage);

    let summary = dispatcher.dispatch_due(2).await.unwrap();
    assert_eq!(summary.handed_off, 2, "{summary:?}");
    let run_a = store.runs(Some(&a), 10).await.unwrap().remove(0);
    let run_b = store.runs(Some(&b), 10).await.unwrap().remove(0);
    assert_eq!(run_a.turn_ordinal, Some(1));
    assert_eq!(run_b.turn_ordinal, Some(1), "each session has its own ordinal");
    assert_ne!(run_a.session_id, run_b.session_id);

    runtime.set_terminal_for(&session_a, 5, "completed");
    dispatcher.dispatch_due(2).await.unwrap();
    let settled_a = store.run(&run_a.id).await.unwrap().unwrap();
    let still_b = store.run(&run_b.id).await.unwrap().unwrap();
    assert_eq!(settled_a.state, RunState::Succeeded);
    assert_eq!(settled_a.task_sequence, Some(5));
    assert_eq!(settled_a.input_tokens, Some(150));
    assert_eq!(
        still_b.state,
        RunState::Running,
        "the first schedule's terminal must not settle the second"
    );
    assert_eq!(still_b.input_tokens, None);

    runtime.set_terminal_for(&session_b, 9, "failed");
    dispatcher.dispatch_due(2).await.unwrap();
    let settled_b = store.run(&run_b.id).await.unwrap().unwrap();
    assert_eq!(settled_b.state, RunState::Failed);
    assert_eq!(settled_b.task_sequence, Some(9));
    assert_eq!(
        settled_b.input_tokens,
        Some(400),
        "each run's usage is its own session delta"
    );
    assert_eq!(
        store.run(&run_a.id).await.unwrap().unwrap().input_tokens,
        Some(150),
        "the second schedule's terminal never rewrites the first"
    );
}

#[test]
fn the_three_kinds_remain_distinct() {
    assert_eq!(agent_action().kind(), ActionKind::Agent);
    assert_eq!(native_action().kind(), ActionKind::Native);
    assert_eq!(
        ScheduleAction::Shell {
            command: "printf ok".into()
        }
        .kind(),
        ActionKind::Shell
    );
}
