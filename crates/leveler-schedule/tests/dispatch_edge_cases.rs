//! The dispatcher's terminal-state and observation edges.
//!
//! Correlation is a delivery identity, not an event order: the dispatcher
//! polls the runtime's snapshot and selects the execution that carries its own
//! `CommandId`. These tests pin the consequences — an execution that starts
//! after the first observation is still found, a foreign look-alike seen
//! first never wins, and every uncertain observation settles `Unknown`
//! without sending the action a second time.

#![cfg(unix)]

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use leveler_client_protocol::{
    ClientCommand, ClientError, CommandEnvelope, CommandId, InteractiveRuntimeClient,
    PermissionProfile, RuntimeEvent, SessionId, UiSessionSnapshot, UiUserShell,
};
use leveler_local_transport::{CreateSessionRequest, LocalRuntimeService, SessionBootstrap};
use leveler_schedule::{
    Dispatcher, NewScheduleDefinition, RunState, RuntimeConnector, ScheduleStore, ScheduleTrigger,
};
use tokio::sync::broadcast;

const COMMAND: &str = "printf 'edge\\n'";

type SnapshotFn = Box<
    dyn FnMut(&[CommandEnvelope]) -> Result<Vec<UiUserShell>, ClientError> + Send + Sync + 'static,
>;

struct ScriptedRuntime {
    session: SessionId,
    delivered: Mutex<Vec<CommandEnvelope>>,
    snapshots: Mutex<SnapshotFn>,
    /// The first delivery is accepted but its acknowledgement is lost.
    lose_ack_once: AtomicBool,
    events: broadcast::Sender<RuntimeEvent>,
}

impl ScriptedRuntime {
    fn new<F>(snapshots: F) -> Self
    where
        F: FnMut(&[CommandEnvelope]) -> Result<Vec<UiUserShell>, ClientError>
            + Send
            + Sync
            + 'static,
    {
        let (events, _) = broadcast::channel(8);
        Self {
            session: SessionId::new("sess-edge"),
            delivered: Mutex::new(Vec::new()),
            snapshots: Mutex::new(Box::new(snapshots)),
            lose_ack_once: AtomicBool::new(false),
            events,
        }
    }

    fn with_lost_ack(self) -> Self {
        self.lose_ack_once.store(true, Ordering::SeqCst);
        self
    }

    fn shell(origin: &CommandId, status: &str, exit_code: Option<i32>) -> UiUserShell {
        UiUserShell {
            id: leveler_core::UserShellId::new("ush-scripted"),
            origin_command_id: Some(origin.clone()),
            command: COMMAND.into(),
            cwd: "/tmp".into(),
            status: status.into(),
            elapsed_secs: 1,
            exit_code,
            output_tail: String::new(),
            output_truncated: false,
            full_output: None,
            task_id: None,
        }
    }

    fn snapshot_with(session: &SessionId, shells: Vec<UiUserShell>) -> UiSessionSnapshot {
        UiSessionSnapshot {
            history: None,
            id: session.clone(),
            repository: None,
            goal: "Scheduled action: edge".into(),
            task_status: None,
            task_terminal: None,
            model: None,
            mode: PermissionProfile::Assisted,
            branch: None,
            status: "idle".into(),
            finalization_stage: None,
            messages: Vec::new(),
            pending_interactions: Vec::new(),
            available_models: Vec::new(),
            vision: false,
            last_sequence: None,
            active_tools: Vec::new(),
            interrupted_tool_calls: Vec::new(),
            active_background_tasks: Vec::new(),
            background_tasks: Vec::new(),
            plan: None,
            diff: None,
            checkpoints: Vec::new(),
            recaps: Vec::new(),
            user_shells: shells,
            completion_report: None,
            thinking: None,
            work_profile: None,
            collaboration: None,
            execution_task: None,
            children: Vec::new(),
        }
    }

    fn run_envelopes(&self) -> Vec<CommandEnvelope> {
        self.delivered
            .lock()
            .unwrap()
            .iter()
            .filter(|envelope| matches!(envelope.command, ClientCommand::RunUserShell { .. }))
            .cloned()
            .collect()
    }

    fn cancel_envelopes(&self) -> Vec<CommandEnvelope> {
        self.delivered
            .lock()
            .unwrap()
            .iter()
            .filter(|envelope| matches!(envelope.command, ClientCommand::CancelUserShell { .. }))
            .cloned()
            .collect()
    }
}

#[async_trait]
impl InteractiveRuntimeClient for ScriptedRuntime {
    async fn send(&self, _command: ClientCommand) -> Result<(), ClientError> {
        Ok(())
    }

    async fn deliver(&self, envelope: CommandEnvelope) -> Result<(), ClientError> {
        let is_run = matches!(envelope.command, ClientCommand::RunUserShell { .. });
        self.delivered.lock().unwrap().push(envelope);
        if is_run && self.lose_ack_once.swap(false, Ordering::SeqCst) {
            // The runtime accepted and started the action; only the
            // acknowledgement back to the dispatcher was lost.
            return Err(ClientError::OutcomeUnknown(
                "the connection closed before the acknowledgement".into(),
            ));
        }
        Ok(())
    }

    fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
        self.events.subscribe()
    }

    async fn snapshot(&self, session_id: &SessionId) -> Result<UiSessionSnapshot, ClientError> {
        if session_id != &self.session {
            return Err(ClientError::SessionNotFound(session_id.clone()));
        }
        let delivered = self.delivered.lock().unwrap().clone();
        let shells = (self.snapshots.lock().unwrap())(&delivered)?;
        Ok(Self::snapshot_with(session_id, shells))
    }
}

#[async_trait]
impl LocalRuntimeService for ScriptedRuntime {
    async fn create_session(
        &self,
        _request: CreateSessionRequest,
    ) -> Result<SessionBootstrap, ClientError> {
        Ok(SessionBootstrap {
            session: Self::snapshot_with(&self.session, Vec::new()),
            context_window: 0,
            notice: None,
        })
    }
}

struct StaticConnector {
    runtime: Arc<ScriptedRuntime>,
}

#[async_trait]
impl RuntimeConnector for StaticConnector {
    async fn connect(&self, _workspace: &Path) -> Result<Arc<dyn LocalRuntimeService>, String> {
        Ok(self.runtime.clone())
    }
}

struct Fixture {
    store: ScheduleStore,
    definition_id: leveler_schedule::ScheduleId,
    _tmp: tempfile::TempDir,
}

async fn fixture(runtime: &ScriptedRuntime, timeout: Duration) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let store = ScheduleStore::open(&tmp.path().join("schedules.db"))
        .await
        .unwrap();
    let definition = store
        .create_definition(NewScheduleDefinition {
            name: "edge".into(),
            // The runtime connector ignores the workspace; the definition's
            // bound session is the one the dispatcher verifies.
            workspace,
            command: COMMAND.into(),
            trigger: ScheduleTrigger::Once {
                at: leveler_core::now() - chrono::Duration::seconds(1),
            },
            mode: PermissionProfile::Assisted,
            model: None,
            timeout,
            misfire: leveler_schedule::MisfirePolicy::Skip,
            misfire_grace: Duration::from_secs(300),
        })
        .await
        .unwrap();
    store
        .set_session(&definition.id, &runtime.session)
        .await
        .unwrap();
    Fixture {
        definition_id: definition.id,
        store,
        _tmp: tmp,
    }
}

fn dispatcher(runtime: &Arc<ScriptedRuntime>, store: ScheduleStore) -> Dispatcher {
    Dispatcher::new(
        store,
        Arc::new(StaticConnector {
            runtime: runtime.clone(),
        }),
    )
    .with_lease(Duration::from_secs(30))
    .with_poll_interval(Duration::from_millis(10))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_shell_settles_failed_with_its_exit_code() {
    let runtime = Arc::new(ScriptedRuntime::new(|delivered: &[CommandEnvelope]| {
        let Some(envelope) = delivered
            .iter()
            .find(|envelope| matches!(envelope.command, ClientCommand::RunUserShell { .. }))
        else {
            // The session verification read happens before delivery.
            return Ok(Vec::new());
        };
        Ok(vec![ScriptedRuntime::shell(
            &envelope.command_id,
            "failed",
            Some(7),
        )])
    }));
    let fixture = fixture(&runtime, Duration::from_secs(5)).await;

    let summary = dispatcher(&runtime, fixture.store.clone())
        .dispatch_due(1)
        .await
        .unwrap();
    assert_eq!(summary.failed, 1, "{summary:?}");
    let run = &fixture
        .store
        .runs(Some(&fixture.definition_id), 10)
        .await
        .unwrap()[0];
    assert_eq!(run.state, RunState::Failed);
    assert_eq!(run.exit_code, Some(7));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_shell_settles_failed() {
    let runtime = Arc::new(ScriptedRuntime::new(|delivered: &[CommandEnvelope]| {
        let Some(envelope) = delivered
            .iter()
            .find(|envelope| matches!(envelope.command, ClientCommand::RunUserShell { .. }))
        else {
            return Ok(Vec::new());
        };
        Ok(vec![ScriptedRuntime::shell(
            &envelope.command_id,
            "cancelled",
            None,
        )])
    }));
    let fixture = fixture(&runtime, Duration::from_secs(5)).await;

    let summary = dispatcher(&runtime, fixture.store.clone())
        .dispatch_due(1)
        .await
        .unwrap();
    assert_eq!(summary.failed, 1, "{summary:?}");
    let run = &fixture
        .store
        .runs(Some(&fixture.definition_id), 10)
        .await
        .unwrap()[0];
    assert_eq!(run.state, RunState::Failed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shell_that_outlives_its_timeout_is_cancelled_and_unknown() {
    let runtime = Arc::new(ScriptedRuntime::new(|delivered: &[CommandEnvelope]| {
        let Some(envelope) = delivered
            .iter()
            .find(|envelope| matches!(envelope.command, ClientCommand::RunUserShell { .. }))
        else {
            return Ok(Vec::new());
        };
        Ok(vec![ScriptedRuntime::shell(
            &envelope.command_id,
            "running",
            None,
        )])
    }));
    let fixture = fixture(&runtime, Duration::from_secs(1)).await;

    let summary = dispatcher(&runtime, fixture.store.clone())
        .dispatch_due(1)
        .await
        .unwrap();
    assert_eq!(summary.unknown, 1, "{summary:?}");

    let cancels = runtime.cancel_envelopes();
    assert_eq!(cancels.len(), 1, "a timed-out shell must be cancelled once");
    match &cancels[0].command {
        ClientCommand::CancelUserShell { execution_id, .. } => {
            assert_eq!(execution_id.as_str(), "ush-scripted");
        }
        other => panic!("unexpected cancel shape: {other:?}"),
    }
    let run = &fixture
        .store
        .runs(Some(&fixture.definition_id), 10)
        .await
        .unwrap()[0];
    assert_eq!(run.state, RunState::Unknown);
    assert_eq!(
        runtime.run_envelopes().len(),
        1,
        "an uncertain run must not be sent a second time"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lost_acknowledgement_is_unknown_even_when_the_shell_completed() {
    let runtime = Arc::new(
        ScriptedRuntime::new(|delivered: &[CommandEnvelope]| {
            let Some(envelope) = delivered
                .iter()
                .find(|envelope| matches!(envelope.command, ClientCommand::RunUserShell { .. }))
            else {
                return Ok(Vec::new());
            };
            Ok(vec![ScriptedRuntime::shell(
                &envelope.command_id,
                "success",
                Some(0),
            )])
        })
        .with_lost_ack(),
    );
    let fixture = fixture(&runtime, Duration::from_secs(5)).await;
    let dispatcher = dispatcher(&runtime, fixture.store.clone());

    let summary = dispatcher.dispatch_due(1).await.unwrap();
    assert_eq!(summary.unknown, 1, "{summary:?}");
    let run = &fixture
        .store
        .runs(Some(&fixture.definition_id), 10)
        .await
        .unwrap()[0];
    assert_eq!(
        run.state,
        RunState::Unknown,
        "a lost acknowledgement must never become a success claim"
    );

    // The action really did run; a later dispatch must not run it again.
    let summary = dispatcher.dispatch_due(1).await.unwrap();
    assert_eq!(summary.claimed, 0);
    assert_eq!(runtime.run_envelopes().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn losing_the_connection_after_delivery_is_unknown_and_not_retried() {
    let delivered_seen = Arc::new(AtomicBool::new(false));
    let seen = delivered_seen.clone();
    let runtime = Arc::new(ScriptedRuntime::new(
        move |delivered: &[CommandEnvelope]| {
            let has_run = delivered
                .iter()
                .any(|envelope| matches!(envelope.command, ClientCommand::RunUserShell { .. }));
            if has_run {
                seen.store(true, Ordering::SeqCst);
                return Err(ClientError::Runtime("connection reset".into()));
            }
            Ok(Vec::new())
        },
    ));
    let fixture = fixture(&runtime, Duration::from_secs(5)).await;
    let dispatcher = dispatcher(&runtime, fixture.store.clone());

    let summary = dispatcher.dispatch_due(1).await.unwrap();
    assert!(delivered_seen.load(Ordering::SeqCst));
    assert_eq!(summary.unknown, 1, "{summary:?}");
    let run = &fixture
        .store
        .runs(Some(&fixture.definition_id), 10)
        .await
        .unwrap()[0];
    assert_eq!(run.state, RunState::Unknown);

    let summary = dispatcher.dispatch_due(1).await.unwrap();
    assert_eq!(summary.claimed, 0, "an unknown occurrence was re-claimed");
    assert_eq!(runtime.run_envelopes().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_execution_starting_after_the_first_observation_is_still_found() {
    let polls = Arc::new(Mutex::new(0usize));
    let counter = polls.clone();
    let runtime = Arc::new(ScriptedRuntime::new(
        move |delivered: &[CommandEnvelope]| {
            // The verification read happens first, then one empty poll while the
            // runtime has not emitted the start yet. Correlation must not depend
            // on having subscribed before the start.
            let mut polls = counter.lock().unwrap();
            *polls += 1;
            if *polls <= 2 {
                return Ok(Vec::new());
            }
            let origin = delivered
                .iter()
                .find(|envelope| matches!(envelope.command, ClientCommand::RunUserShell { .. }))
                .expect("delivery precedes polling")
                .command_id
                .clone();
            Ok(vec![ScriptedRuntime::shell(&origin, "success", Some(0))])
        },
    ));
    let fixture = fixture(&runtime, Duration::from_secs(5)).await;

    let summary = dispatcher(&runtime, fixture.store.clone())
        .dispatch_due(1)
        .await
        .unwrap();
    assert_eq!(summary.succeeded, 1, "{summary:?}");
    let run = &fixture
        .store
        .runs(Some(&fixture.definition_id), 10)
        .await
        .unwrap()[0];
    assert_eq!(run.state, RunState::Succeeded);
    assert_eq!(run.execution_id.as_deref(), Some("ush-scripted"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_foreign_look_alike_seen_first_never_wins_over_our_own_execution() {
    let polls = Arc::new(Mutex::new(0usize));
    let counter = polls.clone();
    let runtime = Arc::new(ScriptedRuntime::new(
        move |delivered: &[CommandEnvelope]| {
            let mut polls = counter.lock().unwrap();
            *polls += 1;
            // Verification, then the foreign client's execution with identical
            // text, then our own. The dispatcher must skip the look-alike.
            if *polls <= 2 {
                return Ok(vec![UiUserShell {
                    id: leveler_core::UserShellId::new("ush-foreign"),
                    origin_command_id: Some(CommandId::new("another-client-command")),
                    command: COMMAND.into(),
                    cwd: "/tmp".into(),
                    status: "success".into(),
                    elapsed_secs: 1,
                    exit_code: Some(0),
                    output_tail: String::new(),
                    output_truncated: false,
                    full_output: None,
                    task_id: None,
                }]);
            }
            let origin = delivered
                .iter()
                .find(|envelope| matches!(envelope.command, ClientCommand::RunUserShell { .. }))
                .expect("delivery precedes polling")
                .command_id
                .clone();
            Ok(vec![ScriptedRuntime::shell(&origin, "success", Some(0))])
        },
    ));
    let fixture = fixture(&runtime, Duration::from_secs(5)).await;

    let summary = dispatcher(&runtime, fixture.store.clone())
        .dispatch_due(1)
        .await
        .unwrap();
    assert_eq!(summary.succeeded, 1, "{summary:?}");
    let run = &fixture
        .store
        .runs(Some(&fixture.definition_id), 10)
        .await
        .unwrap()[0];
    assert_eq!(
        run.execution_id.as_deref(),
        Some("ush-scripted"),
        "the run adopted the foreign execution"
    );
}
