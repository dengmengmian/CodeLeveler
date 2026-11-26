//! Structured state of recovery work that must not delay the first client frame.
//!
//! Local start-up has two kinds of work. Canonical, cheap reads (sessions, turns,
//! boot leases) belong before the runtime serves clients. Execution-service
//! reconciliation and owed continuation starts call out of process — an
//! Execution Host that accepts a connection but answers slowly must not leave the
//! terminal blank. This ledger lets the runtime start that work and still tell
//! every client the truth: running, finished, or failed at a named stage.
//!
//! The ledger is runtime state, not a second recovery engine. The work itself
//! stays in [`crate::Application::reconcile_execution_services`] and
//! [`crate::InProcessRuntimeClient::reconcile_owed_clarification_continuations`];
//! this only records what happened so projections can be honest about it.

use std::collections::BTreeSet;
use std::sync::Mutex;
use std::time::Duration;

use leveler_client_protocol::{UiStartupRecovery, UiStartupRecoveryFailure};
use tokio::sync::watch;

/// Recover hosted process settlements and bind this boot to the task registry.
pub(crate) const EXECUTION_SERVICES: &str = "execution_services";
/// Start goal windows whose durable clarification answer has no continuation.
pub(crate) const OWED_CONTINUATIONS: &str = "owed_continuations";

/// One immutable reading of the ledger.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RecoveryReadout {
    pub(crate) started: bool,
    pub(crate) running: bool,
    pub(crate) failures: Vec<UiStartupRecoveryFailure>,
    pub(crate) recovered_tasks: u32,
    pub(crate) unresolved_intents: u32,
}

impl RecoveryReadout {
    /// Whether the Execution Host can be asked for live tasks on a projection
    /// path. False while that stage is still running or after it failed: a
    /// snapshot must use the durable records it already owns instead of making
    /// the client wait for a host that may never answer.
    pub(crate) fn permits_host_reads(&self) -> bool {
        !self.running
            && !self
                .failures
                .iter()
                .any(|failure| failure.stage == EXECUTION_SERVICES)
    }

    pub(crate) fn to_wire(&self) -> Option<UiStartupRecovery> {
        self.started.then(|| UiStartupRecovery {
            running: self.running,
            failures: self.failures.clone(),
            recovered_tasks: self.recovered_tasks,
            unresolved_intents: self.unresolved_intents,
        })
    }
}

#[derive(Debug, Default)]
struct State {
    started: bool,
    running: BTreeSet<&'static str>,
    failures: Vec<UiStartupRecoveryFailure>,
    recovered_tasks: u32,
    unresolved_intents: u32,
}

impl State {
    fn readout(&self) -> RecoveryReadout {
        RecoveryReadout {
            started: self.started,
            running: !self.running.is_empty(),
            failures: self.failures.clone(),
            recovered_tasks: self.recovered_tasks,
            unresolved_intents: self.unresolved_intents,
        }
    }
}

/// The runtime's startup-recovery state, observable without blocking.
pub(crate) struct StartupRecovery {
    state: Mutex<State>,
    changes: watch::Sender<RecoveryReadout>,
}

impl StartupRecovery {
    pub(crate) fn new() -> Self {
        let (changes, _) = watch::channel(RecoveryReadout::default());
        Self {
            state: Mutex::new(State::default()),
            changes,
        }
    }

    fn update(&self, change: impl FnOnce(&mut State)) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        change(&mut state);
        let readout = state.readout();
        self.changes.send_replace(readout);
    }

    /// Mark a stage as in flight. Idempotent for an already-running stage.
    pub(crate) fn begin(&self, stage: &'static str) {
        self.update(|state| {
            state.started = true;
            state.running.insert(stage);
        });
    }

    /// A stage finished without recovering any host residue.
    pub(crate) fn finish(&self, stage: &'static str, result: Result<(), String>) {
        self.finish_with_recovery(stage, result.map(|()| None));
    }

    /// A stage finished; `recovery` is the host residue it converged, if any.
    pub(crate) fn finish_with_recovery(
        &self,
        stage: &'static str,
        result: Result<Option<leveler_execution::HostRecovery>, String>,
    ) {
        self.update(|state| {
            state.running.remove(stage);
            match result {
                Ok(recovery) => {
                    if let Some(recovery) = recovery {
                        state.recovered_tasks = state.recovered_tasks.saturating_add(
                            u32::try_from(recovery.tasks.len()).unwrap_or(u32::MAX),
                        );
                        state.unresolved_intents = state.unresolved_intents.saturating_add(
                            u32::try_from(recovery.unresolved_intents.len()).unwrap_or(u32::MAX),
                        );
                    }
                }
                Err(message) => {
                    // One row per distinct failure. A repeated boot of the same
                    // broken host names the same fact; it must not flood the
                    // projection with duplicates.
                    if !state
                        .failures
                        .iter()
                        .any(|failure| failure.stage == stage && failure.message == message)
                    {
                        state.failures.push(UiStartupRecoveryFailure {
                            stage: stage.to_string(),
                            message,
                        });
                    }
                }
            }
        });
    }

    pub(crate) fn read(&self) -> RecoveryReadout {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .readout()
    }

    /// Wait a bounded grace for all stages to settle, so the common healthy
    /// host still produces a complete first snapshot. The grace never becomes
    /// the reason startup completes: the caller proceeds with durable facts
    /// when it expires.
    pub(crate) async fn wait_settled(&self, grace: Duration) -> RecoveryReadout {
        let readout = self.read();
        if !readout.running {
            return readout;
        }
        let mut changes = self.changes.subscribe();
        let _ = tokio::time::timeout(grace, changes.wait_for(|state| !state.running)).await;
        self.read()
    }

    /// A receiver for the runtime's own recovery consumer (tests and CLI boot
    /// notices), if one is ever needed.
    #[allow(dead_code)]
    pub(crate) fn subscribe(&self) -> watch::Receiver<RecoveryReadout> {
        self.changes.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_execution::execution_host::{HostRecovery, ProcessPresence, RecoveredTask};

    #[test]
    fn no_work_reads_as_unstarted_and_permits_host_reads() {
        let ledger = StartupRecovery::new();
        let readout = ledger.read();
        assert!(!readout.started);
        assert!(readout.to_wire().is_none());
        assert!(readout.permits_host_reads());
    }

    #[test]
    fn a_failed_stage_is_structured_and_blocks_host_reads() {
        let ledger = StartupRecovery::new();
        ledger.begin(EXECUTION_SERVICES);
        assert!(!ledger.read().permits_host_reads());
        ledger.finish(EXECUTION_SERVICES, Err("host timed out".into()));
        let wire = ledger.read().to_wire().expect("started");
        assert!(!wire.running);
        assert_eq!(wire.failures.len(), 1);
        assert_eq!(wire.failures[0].stage, EXECUTION_SERVICES);
        assert!(!ledger.read().permits_host_reads());
    }

    #[test]
    fn a_recovered_host_is_a_count_not_a_success_claim() {
        let ledger = StartupRecovery::new();
        ledger.begin(EXECUTION_SERVICES);
        ledger.finish_with_recovery(
            EXECUTION_SERVICES,
            Ok(Some(HostRecovery {
                observed_at_ms: 1,
                tasks: vec![RecoveredTask {
                    task_id: "t1".into(),
                    program: "make".into(),
                    args: vec!["up".into()],
                    previous: leveler_execution::BackgroundTaskStatus::Running,
                    process: ProcessPresence::Gone,
                }],
                unresolved_intents: vec!["i1".into()],
            })),
        );
        let wire = ledger.read().to_wire().expect("started");
        assert!(!wire.running);
        assert!(wire.failures.is_empty());
        assert_eq!(wire.recovered_tasks, 1);
        assert_eq!(wire.unresolved_intents, 1);
        assert!(ledger.read().permits_host_reads());
    }

    #[tokio::test]
    async fn a_healthy_stage_settles_within_the_grace() {
        let ledger = StartupRecovery::new();
        ledger.begin(EXECUTION_SERVICES);
        let readout = ledger.wait_settled(Duration::from_secs(1)).await;
        assert!(readout.running, "the stage is still in flight");
    }

    #[tokio::test]
    async fn waiting_returns_early_once_the_stage_settles() {
        let ledger = std::sync::Arc::new(StartupRecovery::new());
        ledger.begin(EXECUTION_SERVICES);
        let other = ledger.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            other.finish(EXECUTION_SERVICES, Ok(()));
        });
        let readout = ledger.wait_settled(Duration::from_secs(2)).await;
        assert!(!readout.running);
    }
}
