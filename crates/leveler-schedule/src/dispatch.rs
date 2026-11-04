//! Turns due occurrences into one runtime user-shell execution each.
//!
//! The dispatcher is an ordinary local client. It creates (or reuses) one
//! session per definition through the runtime's own `CreateSessionRequest`,
//! delivers exactly one `RunUserShell` command, observes the runtime's own
//! user-shell record, settles the run, and drops the client so the daemon's
//! idle eviction is not held open.
//!
//! Outcome observation deliberately reads the session snapshot instead of a
//! best-effort event subscription. A socket subscription attaches
//! asynchronously; a fast command can start and finish before it is live.
//! The snapshot carries the runtime's live user-shell records plus the ones
//! rebuilt from the durable event log, so the same code observes a fast
//! command, a slow one, and one whose runtime restarted mid-run.
//!
//! Correlation is by delivery identity, never by command text. The envelope's
//! `CommandId` is the idempotency key the runtime already deduplicates on;
//! the runtime records it on the execution it admitted, and this dispatcher
//! selects exactly the execution carrying this run's identity. Two clients
//! running identical text in one session therefore stay distinguishable.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use leveler_client_protocol::{
    ApprovalPolicy, ClientCommand, ClientError, CommandEnvelope, CommandId, SessionId,
};
use leveler_local_transport::{
    CollaborationMode, CreateSessionRequest, CreateWorkspaceSelection, LocalRuntimeService,
};
use tokio::time::Instant;

use crate::{
    RunOutcome, RunState, ScheduleDefinition, ScheduleError, ScheduleStore, WakeSource,
};

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

/// What one dispatch pass did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DispatchSummary {
    pub recovered: usize,
    /// Overdue slots classified as missed by policy. They were not executed.
    pub missed: usize,
    pub claimed: usize,
    pub succeeded: usize,
    pub failed: usize,
    pub unknown: usize,
}

/// Short-lived or long-lived driver for due occurrences.
pub struct Dispatcher {
    store: ScheduleStore,
    connector: Arc<dyn RuntimeConnector>,
    lease: Duration,
    poll_interval: Duration,
}

impl Dispatcher {
    pub fn new(store: ScheduleStore, connector: Arc<dyn RuntimeConnector>) -> Self {
        Self {
            store,
            connector,
            lease: Duration::from_secs(120),
            poll_interval: Duration::from_millis(250),
        }
    }

    pub fn with_lease(mut self, lease: Duration) -> Self {
        self.lease = lease;
        self
    }

    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// Recover expired claims, apply the misfire policy, then execute up to
    /// `max` due occurrences. Occurrences are handled one at a time: a busy
    /// runtime must not turn one dispatch pass into uncontrolled concurrency.
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
        let missed = self.store.reconcile_misfires(now).await?;
        let recovered = self.store.recover_expired(now).await?;
        let claims = self.store.claim_due(now, self.lease, max).await?;
        let mut summary = DispatchSummary {
            recovered: recovered.len(),
            missed,
            claimed: claims.len(),
            ..DispatchSummary::default()
        };
        for claim in claims {
            match self.run_claim(claim).await? {
                RunState::Succeeded => summary.succeeded += 1,
                RunState::Failed => summary.failed += 1,
                RunState::Unknown => summary.unknown += 1,
                RunState::Dispatching | RunState::Running => summary.unknown += 1,
            }
        }
        Ok(summary)
    }

    /// The earliest pending occurrence, for a long-lived driver's sleep.
    pub async fn next_due(
        &self,
    ) -> Result<Option<chrono::DateTime<chrono::Utc>>, ScheduleError> {
        self.store.next_due().await
    }

    async fn run_claim(&self, claim: crate::Claim) -> Result<RunState, ScheduleError> {
        let definition = &claim.definition;
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

        let session = match resolve_session(&self.store, &*client, definition).await {
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
                command: definition.command.clone(),
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

        let deadline = Instant::now() + definition.timeout;
        let renew_every = (self.lease / 3).max(Duration::from_secs(1));
        let mut last_renew = Instant::now();
        let mut execution: Option<leveler_core::UserShellId> = None;
        loop {
            let snapshot = match client.snapshot(&session).await {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    return self
                        .settle(
                            &claim.run_id,
                            RunOutcome::Unknown {
                                error: format!(
                                    "the runtime could not report the action's state; \
                                     the action may have run: {error}"
                                ),
                            },
                        )
                        .await;
                }
            };
            let shell = snapshot
                .user_shells
                .into_iter()
                .find(|shell| shell.origin_command_id.as_ref() == Some(&origin));
            if let Some(shell) = shell {
                if execution.as_ref() != Some(&shell.id) {
                    execution = Some(shell.id.clone());
                    self.store
                        .mark_running(&claim.run_id, shell.id.as_str())
                        .await?;
                }
                match shell.status.as_str() {
                    "success" => {
                        return self
                            .settle(
                                &claim.run_id,
                                RunOutcome::Succeeded {
                                    exit_code: shell.exit_code,
                                },
                            )
                            .await;
                    }
                    "failed" | "cancelled" => {
                        return self
                            .settle(
                                &claim.run_id,
                                RunOutcome::Failed {
                                    exit_code: shell.exit_code,
                                    error: format!(
                                        "the scheduled action ended with status `{}`",
                                        shell.status
                                    ),
                                },
                            )
                            .await;
                    }
                    "unknown" => {
                        return self
                            .settle(
                                &claim.run_id,
                                RunOutcome::Unknown {
                                    error: "the runtime lost this action's outcome while it \
                                            was running"
                                        .into(),
                                },
                            )
                            .await;
                    }
                    // `running` and `background` are still in flight. A
                    // background action is awaited to its terminal record:
                    // a schedule run is not done when its process is handed
                    // over, because the schedule must report one outcome.
                    "running" | "background" => {}
                    other => {
                        return self
                            .settle(
                                &claim.run_id,
                                RunOutcome::Unknown {
                                    error: format!("the runtime reported an unknown action \
                                                   status `{other}`"),
                                },
                            )
                            .await;
                    }
                }
            }

            if Instant::now() >= deadline {
                if let Some(execution) = &execution {
                    if let Err(error) = client
                        .issue(
                            session.clone(),
                            ClientCommand::CancelUserShell {
                                session_id: session.clone(),
                                execution_id: execution.clone(),
                            },
                        )
                        .await
                    {
                        tracing::warn!(
                            %error,
                            run = claim.run_id.as_str(),
                            "could not cancel a timed-out scheduled action"
                        );
                    }
                }
                let error = if execution.is_some() {
                    format!(
                        "the action did not report a terminal state within {}s; \
                         its effect is unknown and it was not retried",
                        definition.timeout.as_secs()
                    )
                } else {
                    format!(
                        "the runtime never reported an execution carrying this run's \
                         delivery identity within {}s; its effect is unknown and it \
                         was not retried",
                        definition.timeout.as_secs()
                    )
                };
                return self
                    .settle(&claim.run_id, RunOutcome::Unknown { error })
                    .await;
            }

            if last_renew.elapsed() >= renew_every {
                let renewed = self
                    .store
                    .renew_lease(&claim.occurrence.id, leveler_core::now(), self.lease)
                    .await?;
                if !renewed {
                    // Another dispatcher recovered this occurrence and its
                    // verdict stands. This attempt must not overwrite it.
                    let state = self
                        .store
                        .run(&claim.run_id)
                        .await?
                        .map(|run| run.state)
                        .unwrap_or(RunState::Unknown);
                    return Ok(state);
                }
                last_renew = Instant::now();
            }

            tokio::time::sleep(self.poll_interval).await;
        }
    }

    async fn settle(&self, run_id: &crate::RunId, outcome: RunOutcome) -> Result<RunState, ScheduleError> {
        Ok(self
            .store
            .finish_run(run_id, outcome)
            .await?
            .map(|run| run.state)
            .unwrap_or(RunState::Unknown))
    }
}

/// Resolve the definition's one durable session.
///
/// A definition is never silently rebound: if its stored session is gone, the
/// run fails and says so instead of starting a new history behind the user's
/// back. The only creation race handled here is two dispatchers creating the
/// first session at the same time; the loser adopts the winner's session and
/// removes the empty session it created.
async fn resolve_session(
    store: &ScheduleStore,
    client: &dyn LocalRuntimeService,
    definition: &ScheduleDefinition,
) -> Result<SessionId, ScheduleError> {
    if let Some(session) = &definition.session_id {
        return verify_session(client, session).await;
    }
    let created = client
        .create_session(CreateSessionRequest {
            request_id: Some(CommandId::new(format!(
                "schedule-session-{}",
                definition.id
            ))),
            workspace: CreateWorkspaceSelection::Workspace {
                path: definition.workspace.display().to_string(),
            },
            goal: definition.session_goal(),
            model: definition.model.clone(),
            mode: definition.mode,
            approval_policy: ApprovalPolicy::Interactive,
            collaboration: CollaborationMode::Chat,
        })
        .await
        .map_err(|error| {
            ScheduleError::Runtime(format!(
                "could not create the scheduled session: {error}"
            ))
        })?
        .session
        .id;

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
