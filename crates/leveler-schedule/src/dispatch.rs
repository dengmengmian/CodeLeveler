//! Turns due occurrences into one execution each.
//!
//! A shell occurrence is delivered to the project runtime as one
//! `RunUserShell`; a native occurrence goes through the injected
//! [`NativeActionExecutor`], which is a thin adapter over the same host
//! execution authority the interactive tool uses; an agent occurrence is
//! handed to the runtime as one `RunGoal` and observed until the runtime's own
//! terminal record appears. The dispatcher never runs an agent loop, never
//! decides what the model should do next, and never starts a second runtime.
//!
//! Outcome observation deliberately reads the session snapshot instead of a
//! best-effort event subscription. A socket subscription attaches
//! asynchronously; a fast action can start and finish before it is live. The
//! snapshot carries the runtime's live records plus the ones rebuilt from the
//! durable event log, so the same code observes a fast command, a slow one,
//! and one whose runtime restarted mid-run.
//!
//! Correlation is by delivery identity, never by text. The envelope's
//! `CommandId` is the idempotency key the runtime already deduplicates on.
//! Agent runs additionally persist the event-log sequence observed *before*
//! submission; only a terminal record with a greater sequence can settle the
//! run, so a previous turn can never be mistaken for this one.
//!
//! Delivery is at-most-once. An occurrence is claimed atomically under a
//! lease. After a shell is accepted, the dispatch pass returns so the next
//! natural wake can claim a later slot; the run stays observable until its
//! own deadline. A snapshot that does not answer is not a terminal outcome.
//! The action is never delivered again. Past the deadline the run is
//! `unknown`. A missed run is visible in history; a silently duplicated one
//! is not.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use leveler_client_protocol::{
    ApprovalPolicy, ClientCommand, ClientError, CommandEnvelope, CommandId, SessionId,
    UiSessionSnapshot, UiTaskStatus,
};
use leveler_local_transport::{
    CollaborationMode, CreateSessionRequest, CreateWorkspaceSelection, LocalRuntimeService,
};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// How long one dispatch pass keeps polling a shell that has not shown a
/// terminal record yet. A pass that is still inside a long sleep cannot
/// return to the OS scheduler, so the next natural occurrence never starts.
/// Fast commands still settle in this window. A shell that is visibly
/// running is released immediately.
const SHELL_ADMIT_WINDOW: Duration = Duration::from_secs(2);

use crate::{
    ActionKind, Claim, HandedOffRun, OccurrenceId, OverlapPolicy, RunFacts, RunId, RunOutcome,
    RunState, ScheduleAction, ScheduleDefinition, ScheduleError, ScheduleRun, ScheduleStore,
    SessionPolicy, UsageSnapshot, WakeSource,
};

/// How long a cancellation is awaited before an agent run is recorded as
/// unknown rather than cancelled. The runtime may need to unwind tools.
const AGENT_CANCEL_GRACE: Duration = Duration::from_secs(30);

/// How long observation may keep failing after the deadline before the run is
/// recorded as unknown instead of staying "running" forever.
const OBSERVE_RETRY_GRACE: Duration = Duration::from_secs(300);

/// Opens the runtime that owns a schedule's workspace.
///
/// Production connects to (or starts) the project's default daemon and must
/// return a client that is dropped after the call, so idle eviction still
/// works. Tests substitute a connector around a local socket or an in-process
/// runtime.
#[async_trait]
pub trait RuntimeConnector: Send + Sync {
    async fn connect(&self, workspace: &Path) -> Result<Arc<dyn LocalRuntimeService>, String>;
}

/// How one native action behaves, observed before any process starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeActionClass {
    /// The action can change something outside the workspace.
    pub external_side_effects: bool,
}

/// One native action for the host execution authority.
#[derive(Debug, Clone)]
pub struct NativeExecutionRequest {
    pub action: String,
    pub input: serde_json::Value,
    pub timeout: Duration,
    pub cancellation: CancellationToken,
}

/// The provider's observed action status, mapped onto the schedule domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeRunStatus {
    Completed,
    Submitted,
    Unknown,
    RequiresPermission,
    RequiresConfiguration,
    Failed,
    Unsupported,
    /// The structured input is invalid. Nothing was started; this is a
    /// configuration error, not an unknown effect.
    InvalidInput,
    /// Preparation failed before any process could start. Nothing was
    /// started; the effect is known to be none.
    NotStarted,
}

/// What the native executor observed. `executed` is false only when no
/// process was started; that distinction decides whether a retry is even
/// conceivable.
#[derive(Debug, Clone)]
pub struct NativeExecutionReport {
    pub status: NativeRunStatus,
    pub provider: Option<String>,
    pub execution_id: Option<String>,
    pub exit_code: Option<i32>,
    pub detail: String,
    pub capability_state: Option<String>,
    pub permission_state: Option<String>,
    pub executed: bool,
}

/// The host execution authority for scheduled native actions.
#[async_trait]
pub trait NativeActionExecutor: Send + Sync {
    /// Classify before execution so authorization and desktop leasing happen
    /// before a process can start.
    fn classify(&self, action: &str) -> Result<NativeActionClass, String>;

    /// Execute one action. `Err` means the adapter failed before it could
    /// establish what happened; the caller records the effect as unknown.
    async fn execute(
        &self,
        request: NativeExecutionRequest,
    ) -> Result<NativeExecutionReport, String>;
}

/// Per-run model usage, read from the authoritative request ledger.
#[async_trait]
pub trait UsageReader: Send + Sync {
    async fn usage(&self, workspace: &Path, session: &SessionId) -> Result<UsageSnapshot, String>;
}

/// What one dispatch pass did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DispatchSummary {
    pub recovered: usize,
    /// Slots suppressed by the overlap policy before any delivery.
    pub skipped: usize,
    /// Overdue slots classified as missed by policy. They were not executed.
    pub missed: usize,
    pub claimed: usize,
    pub succeeded: usize,
    /// Native actions whose provider accepted the request but whose final
    /// effect is unconfirmed. Never counted as success.
    pub submitted: usize,
    pub failed: usize,
    pub unknown: usize,
    /// Agent runs handed to the runtime during this pass. Their outcome is
    /// observed by a later pass.
    pub handed_off: usize,
    pub cancelled: usize,
    pub waiting_approval: usize,
    pub blocked: usize,
}

impl DispatchSummary {
    fn record(&mut self, state: RunState) {
        match state {
            RunState::Succeeded => self.succeeded += 1,
            RunState::Submitted => self.submitted += 1,
            RunState::Failed => self.failed += 1,
            RunState::Unknown | RunState::Dispatching => self.unknown += 1,
            RunState::Skipped => self.skipped += 1,
            RunState::WaitingApproval => self.waiting_approval += 1,
            RunState::Blocked => self.blocked += 1,
            RunState::Cancelled => self.cancelled += 1,
            RunState::Running => self.handed_off += 1,
        }
    }
}

/// Short-lived or long-lived driver for due occurrences.
pub struct Dispatcher {
    store: ScheduleStore,
    connector: Arc<dyn RuntimeConnector>,
    native: Option<Arc<dyn NativeActionExecutor>>,
    usage: Option<Arc<dyn UsageReader>>,
    lease: Duration,
    poll_interval: Duration,
}

impl Dispatcher {
    pub fn new(store: ScheduleStore, connector: Arc<dyn RuntimeConnector>) -> Self {
        Self {
            store,
            connector,
            native: None,
            usage: None,
            lease: Duration::from_secs(120),
            poll_interval: Duration::from_millis(250),
        }
    }

    /// Install the host execution authority used by native occurrences.
    pub fn with_native_executor(mut self, native: Arc<dyn NativeActionExecutor>) -> Self {
        self.native = Some(native);
        self
    }

    /// Install the ledger reader used to attribute model usage to a run.
    pub fn with_usage_reader(mut self, usage: Arc<dyn UsageReader>) -> Self {
        self.usage = Some(usage);
        self
    }

    pub fn with_lease(mut self, lease: Duration) -> Self {
        self.lease = lease;
        self
    }

    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// Recover expired claims, settle handed-off agent runs, apply overlap and
    /// misfire policy, then execute up to `max` due occurrences.
    ///
    /// Claims run concurrently, bounded by this pass's `max`. A long agent run
    /// is not held by the pass at all: it is handed to the runtime and observed
    /// on later passes.
    ///
    /// This is a manual pass for tests and for `schedule dispatch` without an
    /// explicit source. A pass records itself as a manual wake.
    pub async fn dispatch_due(&self, max: usize) -> Result<DispatchSummary, ScheduleError> {
        self.dispatch_due_with_source(max, WakeSource::Manual).await
    }

    /// A dispatch pass with an explicit wake source. The wake record is an
    /// observation of the driver, not execution authority: it is written even
    /// when nothing is due and stays unfinished if this process is killed.
    pub async fn dispatch_due_with_source(
        &self,
        max: usize,
        source: WakeSource,
    ) -> Result<DispatchSummary, ScheduleError> {
        let wake = self.store.begin_wake(source).await?;
        match self.dispatch_pass(max).await {
            Ok(summary) => {
                self.store.finish_wake(wake, &summary).await?;
                Ok(summary)
            }
            Err(error) => {
                if let Err(record_error) = self.store.fail_wake(wake, &error.to_string()).await {
                    tracing::warn!(
                        %record_error,
                        wake,
                        "could not record a failed dispatch pass"
                    );
                }
                Err(error)
            }
        }
    }

    async fn dispatch_pass(&self, max: usize) -> Result<DispatchSummary, ScheduleError> {
        let now = leveler_core::now();
        let recovered = self.store.recover_expired(now).await?;
        let mut summary = DispatchSummary {
            recovered: recovered.len(),
            ..DispatchSummary::default()
        };
        // Settle observed runs first: a queue-one slot whose blocker just
        // finished must become claimable in this same pass.
        for state in self.reconcile_agent_runs(now).await? {
            summary.record(state);
        }
        for state in self.reconcile_shell_runs(now).await? {
            summary.record(state);
        }
        // Settlement stamps its own clock. Reusing the pass-open instant
        // would record a queue claim as having started before the run it
        // waited for had finished, even though the claim happens after.
        let now = leveler_core::now();
        // Materialize the next grid slot while the previous run is still
        // active. The pass can then claim it, or record that it was skipped,
        // without waiting for the previous shell to finish.
        self.ensure_horizon(now).await?;
        summary.skipped += self.store.reconcile_overlaps(now).await?;
        summary.missed = self.store.reconcile_misfires(now).await?;
        let claims = self.store.claim_due(now, self.lease, max).await?;
        summary.claimed = claims.len();
        // A due pass starts every claimed action at once. Claims are already
        // bounded by `max`, and the store's CAS keeps each occurrence
        // at-most-once. A long shell or native action must not delay an agent
        // handoff (or another due schedule) that was claimed by the same pass.
        let results =
            futures::future::join_all(claims.into_iter().map(|claim| self.run_claim(claim))).await;
        for result in results {
            summary.record(result?);
        }
        Ok(summary)
    }

    /// Execute one explicit run-now occurrence without letting unrelated
    /// overdue slots consume the pass.
    ///
    /// The overlap policy is applied first, so a slot suppressed by an active
    /// run is recorded as skipped exactly as it would be in a normal pass.
    /// `None` means the slot was no longer pending (another dispatcher claimed
    /// or settled it); the caller must read history rather than assume it ran.
    pub async fn dispatch_run_now(
        &self,
        occurrence_id: &OccurrenceId,
    ) -> Result<Option<RunState>, ScheduleError> {
        let wake = self.store.begin_wake(WakeSource::Manual).await?;
        let now = leveler_core::now();
        let result: Result<Option<RunState>, ScheduleError> = async {
            let skipped = self.store.reconcile_overlaps(now).await?;
            let mut summary = DispatchSummary {
                skipped,
                ..DispatchSummary::default()
            };
            if let Some(claim) = self
                .store
                .claim_run_now(occurrence_id, now, self.lease)
                .await?
            {
                summary.claimed = 1;
                let state = self.run_claim(claim).await?;
                summary.record(state);
                self.store.finish_wake(wake, &summary).await?;
                Ok(Some(state))
            } else {
                self.store.finish_wake(wake, &summary).await?;
                Ok(None)
            }
        }
        .await;
        if let Err(error) = &result
            && let Err(record_error) = self.store.fail_wake(wake, &error.to_string()).await
        {
            tracing::warn!(
                %record_error,
                wake,
                "could not record a failed run-now pass"
            );
        }
        result
    }

    /// The earliest pending occurrence, for a long-lived driver's sleep.
    pub async fn next_due(&self) -> Result<Option<chrono::DateTime<chrono::Utc>>, ScheduleError> {
        self.store.next_due().await
    }

    async fn run_claim(&self, claim: Claim) -> Result<RunState, ScheduleError> {
        match &claim.definition.action {
            ScheduleAction::Shell { .. } => self.run_shell(claim).await,
            ScheduleAction::Native { .. } => self.run_native(claim).await,
            ScheduleAction::Agent { .. } => self.handoff_agent(claim).await,
        }
    }

    async fn run_shell(&self, claim: Claim) -> Result<RunState, ScheduleError> {
        let definition = &claim.definition;
        let command = match &definition.action {
            ScheduleAction::Shell { command } => command.clone(),
            _ => {
                return self
                    .settle(
                        &claim.run_id,
                        RunOutcome::Blocked {
                            error: "the occurrence's action is not a shell action".into(),
                        },
                    )
                    .await;
            }
        };
        // The claim carries the definition as it was when the slot was
        // generated. Re-read it so neither an edit made after the claim nor a
        // missing grant can turn into unattended execution. Authorization is
        // decided before any runtime connection exists.
        let current = match self.store.definition(&definition.id).await {
            Ok(Some(current)) => current,
            Ok(None) => {
                return self
                    .settle(
                        &claim.run_id,
                        RunOutcome::Blocked {
                            error: "the schedule was removed after this occurrence was claimed"
                                .into(),
                        },
                    )
                    .await;
            }
            Err(error) => {
                return self
                    .settle(
                        &claim.run_id,
                        RunOutcome::Failed {
                            exit_code: None,
                            error: format!(
                                "could not re-read the schedule before executing it: {error}"
                            ),
                        },
                    )
                    .await;
            }
        };
        if current.action != definition.action {
            return self
                .settle(
                    &claim.run_id,
                    RunOutcome::Blocked {
                        error: "the schedule's action changed after this occurrence was claimed"
                            .into(),
                    },
                )
                .await;
        }
        if current.workspace != definition.workspace {
            return self
                .settle(
                    &claim.run_id,
                    RunOutcome::Blocked {
                        error: "the schedule's workspace changed after this occurrence was claimed"
                            .into(),
                    },
                )
                .await;
        }
        if let Err(reason) = current.unattended_authorization(ActionKind::Shell, true) {
            return self
                .settle(&claim.run_id, RunOutcome::Blocked { error: reason })
                .await;
        }
        // Concurrent runs need one execution context each. A `continue`
        // session is one turn at a time by runtime contract, so a definition
        // that pairs overlap with session reuse can never honour its own
        // policy. Definitions are validated on write; this guard also covers
        // rows created before that rule existed and never silently degrades
        // the second run into a timeout.
        if definition.overlap == OverlapPolicy::AllowConcurrent
            && definition.session_policy == SessionPolicy::Continue
        {
            return self
                .settle(
                    &claim.run_id,
                    RunOutcome::Blocked {
                        error: "this definition allows concurrent runs but reuses one \
                                session; a session admits one turn at a time. Update it to \
                                a fresh session per run before dispatching it again; the \
                                action was not started"
                            .into(),
                    },
                )
                .await;
        }
        let client = match self.connector.connect(&definition.workspace).await {
            Ok(client) => client,
            Err(error) => {
                return self
                    .settle(
                        &claim.run_id,
                        RunOutcome::Failed {
                            exit_code: None,
                            error: format!(
                                "could not connect to the runtime that owns this schedule: {error}"
                            ),
                        },
                    )
                    .await;
            }
        };

        let session = match resolve_session(
            &self.store,
            &*client,
            definition,
            &claim.run_id,
            definition.session_policy,
            ApprovalPolicy::Interactive,
        )
        .await
        {
            Ok(session) => session,
            Err(error) => {
                return self
                    .settle(
                        &claim.run_id,
                        RunOutcome::Failed {
                            exit_code: None,
                            error: error.to_string(),
                        },
                    )
                    .await;
            }
        };
        self.store.mark_session(&claim.run_id, &session).await?;

        // The delivery identity is the run's identity: it is what the
        // runtime's command receipts deduplicate on, what the execution
        // records, and the only thing this dispatcher matches against.
        let origin = CommandId::new(format!("schedule-run-{}", claim.run_id));
        let envelope = CommandEnvelope {
            command_id: origin.clone(),
            session_id: session.clone(),
            expected_version: None,
            issued_at: leveler_core::now().to_rfc3339(),
            command: ClientCommand::RunUserShell {
                session_id: session.clone(),
                command,
            },
        };
        if let Err(error) = client.deliver(envelope).await {
            let outcome = delivery_failure(error);
            return self.settle(&claim.run_id, outcome).await;
        }
        // The runtime accepted the envelope. Record that fact before waiting:
        // if this process dies now, lease recovery can distinguish "delivered,
        // outcome lost" from "never delivered".
        self.store.mark_delivered(&claim.run_id).await?;

        // The run deadline is the schedule's timeout, measured from delivery.
        // A single snapshot miss inside that window is an observation failure,
        // not a terminal outcome, and it must not deliver the shell again.
        let deadline_at = leveler_core::now()
            + chrono::Duration::seconds(current.timeout.as_secs().min(i64::MAX as u64) as i64);
        let monotonic_deadline = Instant::now() + current.timeout;
        let admit_until = Instant::now() + SHELL_ADMIT_WINDOW.min(current.timeout);
        // A shell that is already running is handed off before this loop can
        // see a cancel. Until then there is no execution identity to cancel,
        // so a deadline reached while the shell is still absent stays Unknown
        // and is not delivered again.
        loop {
            if Instant::now() >= monotonic_deadline {
                self.cancel_shell_execution(&client, &session, &claim.run_id, None)
                    .await;
                return self
                    .settle(&claim.run_id, shell_unconfirmed(current.timeout, false))
                    .await;
            }
            let started = Instant::now();
            match client.snapshot(&session).await {
                Ok(snapshot) => {
                    tracing::debug!(
                        run = claim.run_id.as_str(),
                        session = session.as_str(),
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        "shell snapshot answered"
                    );
                    match interpret_shell(&snapshot, &origin) {
                        ShellView::Terminal {
                            execution_id,
                            outcome,
                        } => {
                            let _ = self
                                .store
                                .mark_running(&claim.run_id, execution_id.as_str())
                                .await?;
                            let outcome = self
                                .attribute_shell_terminal(&claim.run_id, outcome)
                                .await?;
                            return self.settle(&claim.run_id, outcome).await;
                        }
                        ShellView::Running(id) => {
                            let _ = self.store.mark_running(&claim.run_id, id.as_str()).await?;
                            // The process is alive. Return so the next natural
                            // wake can claim a later slot; this command is not
                            // delivered again.
                            break;
                        }
                        ShellView::Absent => {
                            if Instant::now() >= admit_until {
                                break;
                            }
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        %error,
                        run = claim.run_id.as_str(),
                        session = session.as_str(),
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        "shell snapshot was not answered; the delivered run stays observable"
                    );
                    if Instant::now() >= admit_until {
                        break;
                    }
                }
            }
            tokio::time::sleep(self.poll_interval).await;
        }

        let released = self
            .store
            .release_shell_observation(&claim.run_id, deadline_at)
            .await?;
        if !released {
            let state = self
                .store
                .run(&claim.run_id)
                .await?
                .map(|run| run.state)
                .unwrap_or(RunState::Unknown);
            return Ok(state);
        }
        if let Some(definition) = self.store.definition(&current.id).await? {
            self.store
                .ensure_next_occurrence(&definition, leveler_core::now())
                .await?;
        }
        Ok(RunState::Running)
    }

    async fn run_native(&self, claim: Claim) -> Result<RunState, ScheduleError> {
        let definition = &claim.definition;
        let (action, input) = match &definition.action {
            ScheduleAction::Native { action, input } => (action.clone(), input.clone()),
            _ => {
                return self
                    .settle(
                        &claim.run_id,
                        RunOutcome::Blocked {
                            error: "the occurrence's action is not a native action".into(),
                        },
                    )
                    .await;
            }
        };
        let Some(executor) = &self.native else {
            return self
                .settle(
                    &claim.run_id,
                    RunOutcome::Blocked {
                        error: "this dispatcher has no native action executor; the action was \
                                not started"
                            .into(),
                    },
                )
                .await;
        };
        let class = match executor.classify(&action) {
            Ok(class) => class,
            Err(error) => {
                return self
                    .settle(
                        &claim.run_id,
                        RunOutcome::Blocked {
                            error: format!("the native action is not schedulable: {error}"),
                        },
                    )
                    .await;
            }
        };
        // The claim carries a snapshot of the definition taken when the
        // occurrence was acquired. Authorization is a decision about the
        // current persisted state, so re-read it immediately before the
        // decision: a grant revoked after the claim must not authorize an
        // action that has not started yet.
        let current = match self.store.definition(&claim.definition.id).await {
            Ok(Some(current)) => current,
            Ok(None) => {
                return self
                    .settle(
                        &claim.run_id,
                        RunOutcome::Blocked {
                            error: "the schedule definition was removed after this \
                                    occurrence was claimed; the action was not started"
                                .into(),
                        },
                    )
                    .await;
            }
            Err(error) => {
                return self
                    .settle(
                        &claim.run_id,
                        RunOutcome::Failed {
                            exit_code: None,
                            error: format!(
                                "the schedule could not be re-read before its authorization \
                                 decision; the action was not started: {error}"
                            ),
                        },
                    )
                    .await;
            }
        };
        if current.action != definition.action {
            return self
                .settle(
                    &claim.run_id,
                    RunOutcome::Blocked {
                        error: "the schedule's action changed after this occurrence was \
                                claimed; the claimed action was not started"
                            .into(),
                    },
                )
                .await;
        }
        if let Err(reason) =
            current.unattended_authorization(ActionKind::Native, class.external_side_effects)
        {
            return self
                .settle(&claim.run_id, RunOutcome::Blocked { error: reason })
                .await;
        }

        let token = CancellationToken::new();
        let watcher = self.watch_cancel(&claim.run_id, token.clone());
        let report = executor
            .execute(NativeExecutionRequest {
                action: action.clone(),
                input,
                timeout: current.timeout,
                cancellation: token.clone(),
            })
            .await;
        watcher.abort();

        let report = match report {
            Ok(report) => report,
            Err(error) => {
                return self
                    .settle(
                        &claim.run_id,
                        RunOutcome::Unknown {
                            error: format!(
                                "the native executor failed before it could report what \
                                 happened: {error}; the effect is unknown and it was not retried"
                            ),
                        },
                    )
                    .await;
            }
        };
        let facts = RunFacts {
            provider: report.provider.clone(),
            native_outcome: Some(native_status(report.status).to_string()),
            capability_state: report.capability_state.clone(),
            permission_state: report.permission_state.clone(),
            ..RunFacts::default()
        };
        let cancelled = self.store.cancel_requested(&claim.run_id).await?;
        let outcome = if cancelled {
            RunOutcome::Cancelled {
                error: Some(format!(
                    "cancellation was requested; the native action reported `{}`: {}",
                    native_status(report.status),
                    report.detail
                )),
            }
        } else {
            match report.status {
                NativeRunStatus::Completed => RunOutcome::Succeeded {
                    exit_code: report.exit_code,
                },
                NativeRunStatus::Submitted => RunOutcome::Submitted {
                    error: Some(report.detail),
                },
                NativeRunStatus::Unknown => RunOutcome::Unknown {
                    error: format!(
                        "{}; the effect is unknown and it was not retried",
                        report.detail
                    ),
                },
                NativeRunStatus::RequiresPermission => RunOutcome::WaitingApproval {
                    error: report.detail,
                },
                NativeRunStatus::RequiresConfiguration
                | NativeRunStatus::Unsupported
                | NativeRunStatus::InvalidInput
                | NativeRunStatus::NotStarted => RunOutcome::Blocked {
                    error: report.detail,
                },
                NativeRunStatus::Failed => RunOutcome::Failed {
                    exit_code: report.exit_code,
                    error: report.detail,
                },
            }
        };
        Ok(self
            .store
            .finish_run_with_facts(&claim.run_id, outcome, facts)
            .await?
            .map(|run| run.state)
            .unwrap_or(RunState::Unknown))
    }

    /// Cancel a native action while `CommandRunner` owns the child process.
    /// The watcher is aborted when execution returns; a failure to observe the
    /// request is logged, never turned into a fake success.
    fn watch_cancel(
        &self,
        run_id: &RunId,
        token: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        let store = self.store.clone();
        let run_id = run_id.clone();
        tokio::spawn(async move {
            loop {
                if token.is_cancelled() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
                match store.cancel_requested(&run_id).await {
                    Ok(true) => {
                        if let Err(error) = store.mark_cancel_issued(&run_id).await {
                            tracing::warn!(%error, run = run_id.as_str(), "could not record cancellation");
                        }
                        token.cancel();
                        return;
                    }
                    Ok(false) => {}
                    Err(error) => {
                        tracing::warn!(%error, run = run_id.as_str(), "could not observe cancellation");
                    }
                }
            }
        })
    }

    async fn handoff_agent(&self, claim: Claim) -> Result<RunState, ScheduleError> {
        // The claim's definition is a snapshot from acquisition time. The
        // authorization decision belongs to the persisted state observed now:
        // a grant revoked or narrowed after the claim must not start work.
        let definition = match self.store.definition(&claim.definition.id).await {
            Ok(Some(current)) => current,
            Ok(None) => {
                return self
                    .settle(
                        &claim.run_id,
                        RunOutcome::Blocked {
                            error: "the schedule definition was removed after this \
                                    occurrence was claimed; no goal was submitted"
                                .into(),
                        },
                    )
                    .await;
            }
            Err(error) => {
                return self
                    .settle(
                        &claim.run_id,
                        RunOutcome::Failed {
                            exit_code: None,
                            error: format!(
                                "the schedule could not be re-read before its authorization \
                                 decision; no goal was submitted: {error}"
                            ),
                        },
                    )
                    .await;
            }
        };
        if definition.action != claim.definition.action {
            return self
                .settle(
                    &claim.run_id,
                    RunOutcome::Blocked {
                        error: "the schedule's action changed after this occurrence was \
                                claimed; the claimed goal was not submitted"
                            .into(),
                    },
                )
                .await;
        }
        let goal = match &definition.action {
            ScheduleAction::Agent { goal } => goal.clone(),
            _ => {
                return self
                    .settle(
                        &claim.run_id,
                        RunOutcome::Blocked {
                            error: "the occurrence's action is not an agent action".into(),
                        },
                    )
                    .await;
            }
        };
        if definition.model.is_none() {
            return self
                .settle(
                    &claim.run_id,
                    RunOutcome::Blocked {
                        error: "an agent schedule needs a model; none was recorded".into(),
                    },
                )
                .await;
        }
        // An agent run can change things outside the workspace through its
        // tools. That is the classification the grant has to cover.
        if let Err(reason) = definition.unattended_authorization(ActionKind::Agent, true) {
            return self
                .settle(&claim.run_id, RunOutcome::Blocked { error: reason })
                .await;
        }

        let client = match self.connector.connect(&definition.workspace).await {
            Ok(client) => client,
            Err(error) => {
                return self
                    .settle(
                        &claim.run_id,
                        RunOutcome::Failed {
                            exit_code: None,
                            error: format!(
                                "could not connect to the runtime that owns this schedule: {error}"
                            ),
                        },
                    )
                    .await;
            }
        };
        let session = match resolve_session(
            &self.store,
            &*client,
            &definition,
            &claim.run_id,
            definition.session_policy,
            ApprovalPolicy::AutoApprove,
        )
        .await
        {
            Ok(session) => session,
            Err(error) => {
                return self
                    .settle(
                        &claim.run_id,
                        RunOutcome::Failed {
                            exit_code: None,
                            error: error.to_string(),
                        },
                    )
                    .await;
            }
        };
        self.store.mark_session(&claim.run_id, &session).await?;

        // An existing session loses its in-memory approval policy when the
        // daemon restarts. Re-assert it on every run so the authority is the
        // persisted grant evaluated now, not a flag captured when the session
        // was first created.
        if definition.session_policy == SessionPolicy::Continue
            && let Err(error) = client
                .attach_session_policy(&session, ApprovalPolicy::AutoApprove)
                .await
        {
            return self
                .settle(
                    &claim.run_id,
                    RunOutcome::Blocked {
                        error: format!(
                            "the runtime could not re-assert this schedule's unattended \
                             approval policy; the run was not started: {error}"
                        ),
                    },
                )
                .await;
        }

        let snapshot = match client.snapshot(&session).await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return self
                    .settle(
                        &claim.run_id,
                        RunOutcome::Failed {
                            exit_code: None,
                            error: format!(
                                "the runtime could not report the session state before the \
                                 agent run was submitted: {error}"
                            ),
                        },
                    )
                    .await;
            }
        };
        if !snapshot.interrupted_tool_calls.is_empty() {
            return self
                .settle(
                    &claim.run_id,
                    RunOutcome::Blocked {
                        error: "the session is blocked behind the crash-recovery gate; an \
                                ordinary person must acknowledge the interrupted tool calls \
                                before the next turn can start"
                            .into(),
                    },
                )
                .await;
        }
        if let Some(UiTaskStatus::Running | UiTaskStatus::WaitingUser | UiTaskStatus::Answered) =
            snapshot.task_status
        {
            return self
                .settle(
                    &claim.run_id,
                    RunOutcome::Skipped {
                        error: "the session already has an active turn; an unattended \
                                schedule does not interrupt or share it"
                            .into(),
                    },
                )
                .await;
        }
        let baseline_sequence = snapshot.last_sequence.unwrap_or(0);
        let baseline_turn_ordinal = snapshot.latest_turn_ordinal;
        let usage_baseline = self.read_usage(&definition, &session).await;
        if usage_baseline.is_none() && self.usage.is_some() {
            tracing::warn!(
                run = claim.run_id.as_str(),
                session = session.as_str(),
                "no usage baseline was observed; this run's token and cost attribution will be absent"
            );
        }

        let origin = CommandId::new(format!("schedule-goal-{}", claim.run_id));
        let envelope = CommandEnvelope {
            command_id: origin,
            session_id: session.clone(),
            expected_version: None,
            issued_at: leveler_core::now().to_rfc3339(),
            command: ClientCommand::RunGoal {
                session_id: session.clone(),
                content: goal,
                // The schedule must own the turn it settles. Steering into a
                // turn another client is running would attribute that client's
                // work (and usage) to this run.
                require_new_turn: true,
            },
        };
        let delivered = client.deliver(envelope).await;
        // The running turn captured its approver when the runtime spawned it;
        // the session-level policy exists only to admit that one turn. Leaving
        // it at AutoApprove would auto-approve every later turn a person runs
        // in this same session. Restore the interactive default as soon as the
        // admission is known, on both success and failure (a failed admission
        // left no turn that could still need the policy).
        if definition.session_policy == SessionPolicy::Continue
            && let Err(error) = client
                .attach_session_policy(&session, ApprovalPolicy::Interactive)
                .await
        {
            tracing::warn!(
                %error,
                session = session.as_str(),
                run = claim.run_id.as_str(),
                "could not restore the session's interactive approval policy after handoff"
            );
        }
        if let Err(error) = delivered {
            let outcome = delivery_failure(error);
            return self.settle(&claim.run_id, outcome).await;
        }
        // The goal was admitted as a fresh turn. Capture that turn's ordinal
        // now, while it is still the session's newest: the terminal belongs to
        // the newest turn, so a later ordinal means another client started
        // work after this delivery. If the identity cannot be established
        // (the snapshot fails, or the ordinal did not advance exactly once),
        // record `None`; reconciliation then never settles or cancels from a
        // terminal it cannot attribute.
        let turn_ordinal = match client.snapshot(&session).await {
            Ok(after) => delivered_turn_ordinal(baseline_turn_ordinal, after.latest_turn_ordinal),
            Err(error) => {
                tracing::warn!(
                    %error,
                    session = session.as_str(),
                    run = claim.run_id.as_str(),
                    "could not observe the admitted turn; this run's terminal cannot be \
                     attributed and will settle as unknown if it outlives the dispatcher"
                );
                None
            }
        };
        if turn_ordinal.is_none() {
            tracing::warn!(
                session = session.as_str(),
                run = claim.run_id.as_str(),
                before = ?baseline_turn_ordinal,
                "the delivered goal's turn identity could not be established; no terminal \
                 will be attributed to this run"
            );
        }
        self.store.mark_delivered(&claim.run_id).await?;

        let deadline = leveler_core::now()
            + chrono::Duration::seconds(definition.timeout.as_secs().min(i64::MAX as u64) as i64);
        let handed_off = self
            .store
            .mark_handed_off(
                &claim.run_id,
                baseline_sequence,
                turn_ordinal,
                usage_baseline.as_ref(),
                deadline,
            )
            .await?;
        if !handed_off {
            // Lease recovery settled this run while it was being handed off.
            // Its verdict is the record; do not overwrite it.
            return Ok(self
                .store
                .run(&claim.run_id)
                .await?
                .map(|run| run.state)
                .unwrap_or(RunState::Unknown));
        }
        if let Some(definition) = self.store.definition(&definition.id).await? {
            self.store
                .ensure_next_occurrence(&definition, leveler_core::now())
                .await?;
        }
        Ok(RunState::Running)
    }

    async fn ensure_horizon(&self, now: DateTime<Utc>) -> Result<(), ScheduleError> {
        for definition in self.store.definitions().await? {
            if definition.enabled {
                self.store.ensure_next_occurrence(&definition, now).await?;
            }
        }
        Ok(())
    }

    async fn attribute_shell_terminal(
        &self,
        run_id: &RunId,
        outcome: RunOutcome,
    ) -> Result<RunOutcome, ScheduleError> {
        match outcome {
            RunOutcome::Failed { error, exit_code } => {
                if self.store.cancel_requested(run_id).await? {
                    Ok(RunOutcome::Cancelled { error: Some(error) })
                } else {
                    Ok(RunOutcome::Failed { exit_code, error })
                }
            }
            other => Ok(other),
        }
    }

    async fn cancel_shell_execution(
        &self,
        client: &Arc<dyn LocalRuntimeService>,
        session: &SessionId,
        run_id: &RunId,
        execution: Option<&leveler_core::UserShellId>,
    ) {
        let Some(execution) = execution else {
            return;
        };
        if self
            .store
            .cancel_requested(run_id)
            .await
            .ok()
            .is_some_and(|requested| requested)
            && self
                .store
                .run(run_id)
                .await
                .ok()
                .flatten()
                .is_some_and(|run| run.cancel_issued_at.is_some())
        {
            return;
        }
        match client
            .issue(
                session.clone(),
                ClientCommand::CancelUserShell {
                    session_id: session.clone(),
                    execution_id: execution.clone(),
                },
            )
            .await
        {
            Ok(()) => {
                if let Err(error) = self.store.mark_cancel_issued(run_id).await {
                    tracing::warn!(%error, run = run_id.as_str(), "could not record shell cancellation");
                }
            }
            Err(error) => {
                tracing::warn!(%error, run = run_id.as_str(), "could not cancel the scheduled shell");
            }
        }
    }

    /// Read shells the runtime already accepted. A snapshot miss stays open
    /// until the run deadline. The command is not delivered again. A readable
    /// terminal record wins over the deadline: a late snapshot that says the
    /// shell finished is that run's outcome.
    async fn reconcile_shell_runs(
        &self,
        now: DateTime<Utc>,
    ) -> Result<Vec<RunState>, ScheduleError> {
        let mut settled = Vec::new();
        for observed in self.store.observable_shell_runs().await? {
            let run_id = observed.run.id.clone();
            let Some(session) = observed.run.session_id.clone() else {
                settled.push(
                    self.settle(
                        &run_id,
                        RunOutcome::Unknown {
                            error: "the delivered shell run lost its session identity; its \
                                    effect cannot be observed and it was not retried"
                                .into(),
                        },
                    )
                    .await?,
                );
                continue;
            };
            let origin = CommandId::new(format!("schedule-run-{run_id}"));
            let past_deadline = observed
                .run
                .deadline_at
                .is_some_and(|deadline| now >= deadline);
            let client = match self.connector.connect(&observed.definition.workspace).await {
                Ok(client) => client,
                Err(error) => {
                    if past_deadline {
                        tracing::warn!(
                            %error,
                            run = run_id.as_str(),
                            "shell observation could not reach the runtime at the deadline"
                        );
                        settled.push(
                            self.settle(
                                &run_id,
                                shell_unconfirmed(
                                    observed.definition.timeout,
                                    observed.run.execution_id.is_some(),
                                ),
                            )
                            .await?,
                        );
                    } else {
                        tracing::warn!(
                            %error,
                            run = run_id.as_str(),
                            "shell observation could not reach the runtime; the run stays open"
                        );
                    }
                    continue;
                }
            };
            let started = Instant::now();
            let snapshot = match client.snapshot(&session).await {
                Ok(snapshot) => {
                    tracing::debug!(
                        run = run_id.as_str(),
                        session = session.as_str(),
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        past_deadline,
                        "shell snapshot answered"
                    );
                    snapshot
                }
                Err(error) => {
                    tracing::warn!(
                        %error,
                        run = run_id.as_str(),
                        session = session.as_str(),
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        past_deadline,
                        "shell snapshot was not answered; the delivered run stays observable"
                    );
                    if past_deadline {
                        let execution = observed
                            .run
                            .execution_id
                            .clone()
                            .map(leveler_core::UserShellId::new);
                        self.cancel_shell_execution(&client, &session, &run_id, execution.as_ref())
                            .await;
                        settled.push(
                            self.settle(
                                &run_id,
                                shell_unconfirmed(
                                    observed.definition.timeout,
                                    observed.run.execution_id.is_some(),
                                ),
                            )
                            .await?,
                        );
                    }
                    continue;
                }
            };
            match interpret_shell(&snapshot, &origin) {
                ShellView::Terminal {
                    execution_id,
                    outcome,
                } => {
                    let _ = self
                        .store
                        .mark_running(&run_id, execution_id.as_str())
                        .await?;
                    let outcome = self.attribute_shell_terminal(&run_id, outcome).await?;
                    settled.push(self.settle(&run_id, outcome).await?);
                }
                ShellView::Running(id) => {
                    let _ = self.store.mark_running(&run_id, id.as_str()).await?;
                    if past_deadline {
                        self.cancel_shell_execution(&client, &session, &run_id, Some(&id))
                            .await;
                        settled.push(
                            self.settle(
                                &run_id,
                                shell_unconfirmed(observed.definition.timeout, true),
                            )
                            .await?,
                        );
                    } else if self.store.cancel_requested(&run_id).await? {
                        self.cancel_shell_execution(&client, &session, &run_id, Some(&id))
                            .await;
                    }
                }
                ShellView::Absent => {
                    if past_deadline {
                        settled.push(
                            self.settle(
                                &run_id,
                                shell_unconfirmed(observed.definition.timeout, false),
                            )
                            .await?,
                        );
                    }
                }
            }
        }
        Ok(settled)
    }

    /// Observe every agent run the runtime owns and settle the ones whose
    /// terminal record is now present.
    async fn reconcile_agent_runs(
        &self,
        now: DateTime<Utc>,
    ) -> Result<Vec<RunState>, ScheduleError> {
        let mut settled = Vec::new();
        for HandedOffRun { run, definition } in self.store.handed_off_runs().await? {
            if !matches!(definition.action, ScheduleAction::Agent { .. }) {
                settled.push(
                    self.settle(
                        &run.id,
                        RunOutcome::Blocked {
                            error: "a handed-off run belongs to a definition that is no longer \
                                    an agent action"
                                .into(),
                        },
                    )
                    .await?,
                );
                continue;
            }
            let Some(session) = run.session_id.clone() else {
                settled.push(
                    self.settle(
                        &run.id,
                        RunOutcome::Unknown {
                            error: "the handed-off run lost its session identity; its effect \
                                    cannot be observed"
                                .into(),
                        },
                    )
                    .await?,
                );
                continue;
            };
            let client = match self.connector.connect(&definition.workspace).await {
                Ok(client) => client,
                Err(error) => {
                    if let Some(state) = self
                        .observation_lost(&run, now, &format!("could not connect: {error}"))
                        .await?
                    {
                        settled.push(state);
                    }
                    continue;
                }
            };
            let snapshot = match client.snapshot(&session).await {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    if let Some(state) = self
                        .observation_lost(&run, now, &format!("snapshot failed: {error}"))
                        .await?
                    {
                        settled.push(state);
                    }
                    continue;
                }
            };

            // The terminal record settles the run only when it is newer than
            // the turn boundary captured before submission AND the session's
            // newest turn is the one this run started. `baseline_sequence`
            // alone cannot distinguish a later turn another client started;
            // the captured ordinal can. A run without an established ordinal
            // fails closed: it is never settled from a terminal it cannot own.
            let owns_turn = owns_newest_turn(&run, &snapshot);
            if let Some(terminal) = snapshot.task_terminal.as_ref()
                && terminal.sequence > run.baseline_sequence.unwrap_or(0)
                && owns_turn
            {
                let outcome = agent_outcome(terminal);
                let usage = self.read_usage(&definition, &session).await;
                let usage_delta = usage
                    .zip(run.usage_baseline)
                    .map(|(final_usage, baseline)| final_usage.since(&baseline));
                let facts = RunFacts {
                    task_sequence: Some(terminal.sequence),
                    task_outcome: Some(terminal.outcome.clone()),
                    provider: definition
                        .model
                        .as_ref()
                        .map(|model| model.provider.clone()),
                    usage,
                    usage_delta,
                    ..RunFacts::default()
                };
                settled.push(
                    self.store
                        .finish_run_with_facts(&run.id, outcome, facts)
                        .await?
                        .map(|run| run.state)
                        .unwrap_or(RunState::Unknown),
                );
                continue;
            }

            // Nobody can answer an approval or a clarification in an
            // unattended run. Do not wait forever and do not re-prompt: stop
            // the turn, record the reason, and let a later explicit attempt
            // decide. Cancellation is session-scoped, so it is sent only when
            // the newest turn is provably this run's own: a person's turn in
            // the same session must never be cancelled by the schedule.
            if owns_turn
                && (!snapshot.pending_interactions.is_empty()
                    || snapshot.task_status == Some(UiTaskStatus::WaitingUser))
            {
                let detail = describe_interactions(&snapshot.pending_interactions);
                let issued = issue_cancel(&*client, &session, &run.id).await;
                if issued {
                    self.store.mark_cancel_issued(&run.id).await?;
                }
                let state = self
                    .store
                    .finish_run_with_facts(
                        &run.id,
                        RunOutcome::WaitingApproval {
                            error: format!(
                                "the runtime is waiting for a person{detail}; an unattended \
                                 run cannot answer, so it was cancelled and nothing was \
                                 retried{}",
                                if issued {
                                    ""
                                } else {
                                    " (the cancellation could not be delivered)"
                                }
                            ),
                        },
                        RunFacts::default(),
                    )
                    .await?
                    .map(|run| run.state)
                    .unwrap_or(RunState::Unknown);
                settled.push(state);
                continue;
            }

            let cancel_requested = self.store.cancel_requested(&run.id).await?;
            if owns_turn && cancel_requested && run.cancel_issued_at.is_none() {
                // Stay running: the terminal record is the cancellation
                // result, and it is observed on a later pass. Record that the
                // runtime accepted the request so later passes observe the
                // turn instead of sending the same cancellation again.
                if issue_cancel(&*client, &session, &run.id).await {
                    self.store.mark_cancel_issued(&run.id).await?;
                }
                continue;
            }

            let deadline = run.deadline_at.unwrap_or(now);
            if now >= deadline {
                if owns_turn && run.cancel_issued_at.is_none() {
                    if issue_cancel(&*client, &session, &run.id).await {
                        self.store.mark_cancel_issued(&run.id).await?;
                    }
                    continue;
                }
                let grace = chrono::Duration::seconds(AGENT_CANCEL_GRACE.as_secs() as i64);
                if now - deadline >= grace {
                    let error = if owns_turn {
                        format!(
                            "the agent run outlived its {}s deadline and its cancellation \
                             was not confirmed; the task may still be running and its \
                             effect is unknown",
                            definition.timeout.as_secs()
                        )
                    } else {
                        "the session's newest turn is no longer the one this run started \
                         (or its identity was never established); this run's terminal \
                         cannot be attributed to it and its effect is unknown"
                            .to_string()
                    };
                    settled.push(
                        self.store
                            .finish_run_with_facts(
                                &run.id,
                                RunOutcome::Unknown { error },
                                RunFacts {
                                    provider: definition
                                        .model
                                        .as_ref()
                                        .map(|model| model.provider.clone()),
                                    ..RunFacts::default()
                                },
                            )
                            .await?
                            .map(|run| run.state)
                            .unwrap_or(RunState::Unknown),
                    );
                }
            }
        }
        Ok(settled)
    }

    /// Keep an unobservable agent run `running` until its observation grace
    /// expires; then record that the outcome could not be established.
    async fn observation_lost(
        &self,
        run: &crate::ScheduleRun,
        now: DateTime<Utc>,
        detail: &str,
    ) -> Result<Option<RunState>, ScheduleError> {
        let deadline = run.deadline_at.unwrap_or(run.started_at);
        let grace = chrono::Duration::seconds(OBSERVE_RETRY_GRACE.as_secs() as i64);
        if now < deadline + grace {
            tracing::warn!(
                run = run.id.as_str(),
                %detail,
                "agent run observation failed; it stays running"
            );
            return Ok(None);
        }
        Ok(Some(
            self.store
                .finish_run_with_facts(
                    &run.id,
                    RunOutcome::Unknown {
                        error: format!(
                            "the schedule could not observe this agent run within its \
                             deadline and {detail}; the task may have run and its effect is \
                             unknown"
                        ),
                    },
                    RunFacts::default(),
                )
                .await?
                .map(|run| run.state)
                .unwrap_or(RunState::Unknown),
        ))
    }

    async fn read_usage(
        &self,
        definition: &ScheduleDefinition,
        session: &SessionId,
    ) -> Option<UsageSnapshot> {
        let reader = self.usage.as_ref()?;
        match reader.usage(&definition.workspace, session).await {
            Ok(usage) => Some(usage),
            Err(error) => {
                tracing::warn!(
                    %error,
                    session = session.as_str(),
                    "could not read the session's model usage"
                );
                None
            }
        }
    }

    async fn settle(&self, run_id: &RunId, outcome: RunOutcome) -> Result<RunState, ScheduleError> {
        Ok(self
            .store
            .finish_run(run_id, outcome)
            .await?
            .map(|run| run.state)
            .unwrap_or(RunState::Unknown))
    }
}

/// Resolve or create the session a run uses.
///
/// `Continue` reuses the definition's one durable session and never silently
/// rebinds it: if its stored session is gone, the run fails and says so.
/// `Fresh` creates one session per run and deliberately does not bind the
/// definition, so two concurrent runs cannot share context. Creation is
/// idempotent through the runtime's identified-creation receipt; the request
/// id is stable for the thing being created (the definition for `Continue`,
/// the run for `Fresh`).
async fn resolve_session(
    store: &ScheduleStore,
    client: &dyn LocalRuntimeService,
    definition: &ScheduleDefinition,
    run_id: &RunId,
    policy: SessionPolicy,
    approval_policy: ApprovalPolicy,
) -> Result<SessionId, ScheduleError> {
    if policy == SessionPolicy::Fresh {
        return create_session(
            client,
            definition,
            format!("schedule-session-{}-{run_id}", definition.id),
            approval_policy,
        )
        .await;
    }
    if let Some(session) = &definition.session_id {
        return verify_session(client, session).await;
    }
    let created = create_session(
        client,
        definition,
        format!("schedule-session-{}", definition.id),
        approval_policy,
    )
    .await?;

    match store.set_session(&definition.id, &created).await {
        Ok(_) => Ok(created),
        Err(ScheduleError::SessionMismatch(_)) => {
            let winner = store
                .definition(&definition.id)
                .await?
                .and_then(|definition| definition.session_id)
                .ok_or_else(|| {
                    ScheduleError::SessionMismatch(format!(
                        "definition {} lost its session binding during creation",
                        definition.id
                    ))
                })?;
            // The created session is empty and unreferenced. Removing it keeps
            // the session list honest; failure here is not a run failure.
            if let Err(error) = client
                .issue(
                    created.clone(),
                    ClientCommand::DeleteSession {
                        session_id: created.clone(),
                    },
                )
                .await
            {
                tracing::warn!(
                    %error,
                    session = created.as_str(),
                    "could not remove a redundant scheduled session"
                );
            }
            verify_session(client, &winner).await
        }
        Err(error) => Err(error),
    }
}

async fn create_session(
    client: &dyn LocalRuntimeService,
    definition: &ScheduleDefinition,
    request_id: String,
    approval_policy: ApprovalPolicy,
) -> Result<SessionId, ScheduleError> {
    Ok(client
        .create_session(CreateSessionRequest {
            request_id: Some(CommandId::new(request_id)),
            workspace: CreateWorkspaceSelection::Workspace {
                path: definition.workspace.display().to_string(),
            },
            goal: definition.session_goal(),
            model: definition.model.clone(),
            mode: definition.mode,
            approval_policy,
            collaboration: CollaborationMode::Chat,
        })
        .await
        .map_err(|error| {
            ScheduleError::Runtime(format!("could not create the scheduled session: {error}"))
        })?
        .session
        .id)
}

async fn verify_session(
    client: &dyn LocalRuntimeService,
    session: &SessionId,
) -> Result<SessionId, ScheduleError> {
    match client.snapshot(session).await {
        Ok(_) => Ok(session.clone()),
        Err(ClientError::SessionNotFound(_)) => Err(ScheduleError::SessionMismatch(format!(
            "the definition is bound to session {session}, which no longer exists; \
             it was not replaced"
        ))),
        Err(error) => Err(ScheduleError::Runtime(format!(
            "could not verify the definition's session {session}: {error}"
        ))),
    }
}

/// Ask the runtime to stop the current turn. The result is recorded on the
/// run, not assumed: `cancel_issued_at` is set only when the envelope was
/// accepted.
async fn issue_cancel(
    client: &dyn LocalRuntimeService,
    session: &SessionId,
    run_id: &RunId,
) -> bool {
    let result = client
        .issue(
            session.clone(),
            ClientCommand::CancelCurrentTurn {
                session_id: session.clone(),
            },
        )
        .await;
    match result {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(%error, run = run_id.as_str(), "could not cancel the agent turn");
            false
        }
    }
}

fn describe_interactions(interactions: &[leveler_client_protocol::UiPendingInteraction]) -> String {
    if interactions.is_empty() {
        return String::new();
    }
    let kinds: Vec<&str> = interactions
        .iter()
        .map(|interaction| match interaction {
            leveler_client_protocol::UiPendingInteraction::Approval(_) => "approval",
            leveler_client_protocol::UiPendingInteraction::Clarification(_) => "clarification",
        })
        .collect();
    format!(" ({})", kinds.join(", "))
}

fn agent_outcome(terminal: &leveler_client_protocol::UiTaskTerminal) -> RunOutcome {
    let detail = terminal
        .reason
        .clone()
        .or_else(|| terminal.stop.clone())
        .unwrap_or_else(|| format!("the runtime reported task outcome `{}`", terminal.outcome));
    match terminal.outcome.as_str() {
        "completed" => RunOutcome::Succeeded { exit_code: None },
        "cancelled" => RunOutcome::Cancelled {
            error: Some(detail),
        },
        "blocked" => RunOutcome::Blocked { error: detail },
        "failed" | "interrupted" | "budget_limited" => RunOutcome::Failed {
            exit_code: None,
            error: detail,
        },
        other => RunOutcome::Unknown {
            error: format!("the runtime reported an unrecognized task outcome `{other}`: {detail}"),
        },
    }
}

fn native_status(status: NativeRunStatus) -> &'static str {
    match status {
        NativeRunStatus::Completed => "completed",
        NativeRunStatus::Submitted => "submitted",
        NativeRunStatus::Unknown => "unknown",
        NativeRunStatus::RequiresPermission => "requires_permission",
        NativeRunStatus::RequiresConfiguration => "requires_configuration",
        NativeRunStatus::Failed => "failed",
        NativeRunStatus::Unsupported => "unsupported",
        NativeRunStatus::InvalidInput => "invalid_input",
        NativeRunStatus::NotStarted => "not_started",
    }
}

/// What one snapshot says about the shell this run delivered.
enum ShellView {
    /// The runtime's record for this delivery identity is terminal.
    Terminal {
        execution_id: leveler_core::UserShellId,
        outcome: RunOutcome,
    },
    /// The runtime has an execution and it is still in flight.
    Running(leveler_core::UserShellId),
    /// No execution carries this delivery identity yet.
    Absent,
}

/// Select the execution whose origin is this run's delivery identity.
/// A foreign shell with the same command text is not this run.
fn interpret_shell(snapshot: &UiSessionSnapshot, origin: &CommandId) -> ShellView {
    let Some(shell) = snapshot
        .user_shells
        .iter()
        .find(|shell| shell.origin_command_id.as_ref() == Some(origin))
    else {
        return ShellView::Absent;
    };
    let execution_id = shell.id.clone();
    match shell.status.as_str() {
        "success" => ShellView::Terminal {
            execution_id,
            outcome: RunOutcome::Succeeded {
                exit_code: shell.exit_code,
            },
        },
        "failed" => ShellView::Terminal {
            execution_id,
            outcome: RunOutcome::Failed {
                exit_code: shell.exit_code,
                error: "the scheduled action ended with status `failed`".into(),
            },
        },
        "cancelled" => ShellView::Terminal {
            execution_id,
            outcome: RunOutcome::Cancelled {
                error: Some("the scheduled action ended with status `cancelled`".into()),
            },
        },
        "unknown" => ShellView::Terminal {
            execution_id,
            outcome: RunOutcome::Unknown {
                error: "the runtime lost this action's outcome while it was running".into(),
            },
        },
        "running" | "background" => ShellView::Running(execution_id),
        other => ShellView::Terminal {
            execution_id,
            outcome: RunOutcome::Unknown {
                error: format!("the runtime reported an unknown action status `{other}`"),
            },
        },
    }
}

/// The run deadline elapsed without an authoritative terminal record.
/// The command was not delivered again.
fn shell_unconfirmed(timeout: Duration, saw_execution: bool) -> RunOutcome {
    let error = if saw_execution {
        format!(
            "the action did not report a terminal state within {}s; \
             its effect is unknown and it was not retried",
            timeout.as_secs()
        )
    } else {
        format!(
            "the runtime never reported an execution carrying this run's \
             delivery identity within {}s; its effect is unknown and it \
             was not retried",
            timeout.as_secs()
        )
    };
    RunOutcome::Unknown { error }
}

fn delivery_failure(error: ClientError) -> RunOutcome {
    match error {
        ClientError::OutcomeUnknown(message) | ClientError::Unresolvable(message) => {
            RunOutcome::Unknown {
                error: format!("delivery did not settle; the action may have run: {message}"),
            }
        }
        other => RunOutcome::Failed {
            exit_code: None,
            error: format!("the runtime did not accept the scheduled action: {other}"),
        },
    }
}

/// The turn a just-delivered goal started is the ordinal immediately after
/// the pre-delivery one. A different result (no new turn, or a newer turn
/// already started) means the identity is not provable, so callers fail
/// closed instead of attributing a terminal to this run.
fn delivered_turn_ordinal(before: Option<i64>, after: Option<i64>) -> Option<i64> {
    let expected = before.map_or(1, |ordinal| ordinal.saturating_add(1));
    (after == Some(expected)).then_some(expected)
}

/// A snapshot's projected terminal belongs to the newest turn. It is this
/// run's only when the newest turn is exactly the one delivery captured.
fn owns_newest_turn(run: &ScheduleRun, snapshot: &UiSessionSnapshot) -> bool {
    matches!(
        (run.turn_ordinal, snapshot.latest_turn_ordinal),
        (Some(owned), Some(newest)) if owned == newest
    )
}
