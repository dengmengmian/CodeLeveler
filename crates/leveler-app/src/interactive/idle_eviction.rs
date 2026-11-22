use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::FutureExt;
use leveler_client_protocol::{
    RuntimeIdleEviction, RuntimeIdleEvictionError, RuntimeIdleEvictionState,
};
use leveler_execution::BackgroundTaskRegistry;
use leveler_local_transport::LocalWaiters;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::{ActiveTurns, RetireAdmission, runtime_quiescence};

#[derive(Clone, Default)]
pub(super) struct IdleEviction(Arc<Mutex<Option<Observation>>>);

struct Observation {
    timeout: Duration,
    state: RuntimeIdleEvictionState,
    idle_since: Option<Instant>,
    last_error: Option<RuntimeIdleEvictionError>,
}

impl IdleEviction {
    pub(super) fn status(&self) -> Option<RuntimeIdleEviction> {
        self.0
            .lock()
            .unwrap()
            .as_ref()
            .map(|observation| RuntimeIdleEviction {
                effective_idle_timeout_secs: observation.timeout.as_secs(),
                idle_for_ms: observation
                    .idle_since
                    .map(|since| since.elapsed().as_millis().try_into().unwrap_or(u64::MAX)),
                state: observation.state,
                last_error: observation.last_error,
            })
    }

    pub(super) fn spawn(
        &self,
        shutdown: CancellationToken,
        clients: LocalWaiters,
        active: Arc<ActiveTurns>,
        background: Arc<BackgroundTaskRegistry>,
        timeout: Duration,
    ) {
        {
            let mut observation = self.0.lock().unwrap();
            if observation.is_some() {
                return;
            }
            *observation = Some(Observation {
                timeout,
                state: RuntimeIdleEvictionState::Observing,
                idle_since: None,
                last_error: None,
            });
        }
        tracing::info!(
            target: "leveler_runtime_lifecycle",
            event = "runtime_idle_supervisor_started",
            effective_idle_timeout_secs = timeout.as_secs(),
            "monitoring daemon inactivity"
        );
        let observer = self.clone();
        tokio::spawn(async move {
            let result =
                AssertUnwindSafe(observer.run(shutdown, clients, active, background, timeout))
                    .catch_unwind()
                    .await;
            if result.is_err() {
                observer.observe(
                    RuntimeIdleEvictionState::ObservationFailed,
                    None,
                    Some(RuntimeIdleEvictionError::SupervisorPanicked),
                );
                tracing::error!(
                    target: "leveler_runtime_lifecycle",
                    event = "runtime_idle_supervisor_panicked",
                    "idle eviction supervisor stopped"
                );
            }
        });
    }

    fn observe(
        &self,
        state: RuntimeIdleEvictionState,
        idle_since: Option<Instant>,
        error: Option<RuntimeIdleEvictionError>,
    ) -> bool {
        let mut guard = self.0.lock().unwrap();
        let observation = guard.as_mut().expect("idle supervisor was initialized");
        let changed = observation.state != state;
        observation.state = state;
        observation.idle_since = idle_since;
        if let Some(error) = error {
            observation.last_error = Some(error);
        }
        changed
    }

    async fn run(
        &self,
        shutdown: CancellationToken,
        clients: LocalWaiters,
        active: Arc<ActiveTurns>,
        background: Arc<BackgroundTaskRegistry>,
        timeout: Duration,
    ) {
        use RuntimeIdleEvictionState::{Busy, Counting, ObservationFailed, Retiring};

        let poll = (timeout / 5).clamp(Duration::from_millis(20), Duration::from_secs(5));
        let epochs = || clients.idle_epoch().zip(active.idle_epoch());
        let mut window = None;
        loop {
            if shutdown.is_cancelled() || active.is_retiring() {
                self.observe(Retiring, None, None);
                return;
            }
            let mut delay = poll;
            // A short attachment or turn changes its epoch even when both
            // counts are zero at the next sample. Recheck after the registry
            // await in case either source changed while it was observed.
            let before = epochs();
            if let Some((client_epoch, turn_epoch)) = before {
                match runtime_quiescence(&active, &background).await {
                    Ok(quiescence) if !quiescence.quiescent() || epochs() != before => {
                        window = None;
                        self.observe(Busy, None, None);
                    }
                    Ok(_) => {
                        let since = match window {
                            Some((previous, since)) if previous == (client_epoch, turn_epoch) => {
                                since
                            }
                            _ => Instant::now(),
                        };
                        window = Some(((client_epoch, turn_epoch), since));
                        self.observe(Counting, Some(since), None);
                        let remaining = timeout.saturating_sub(since.elapsed());
                        if remaining.is_zero() {
                            let committed = clients
                                .with_idle_epoch(client_epoch, || {
                                    if shutdown.is_cancelled()
                                        || active.retire_if_idle(Some(turn_epoch))
                                            != RetireAdmission::Accepted
                                    {
                                        return false;
                                    }
                                    self.observe(Retiring, Some(since), None);
                                    shutdown.cancel();
                                    true
                                })
                                .unwrap_or(false);
                            if committed {
                                tracing::info!(
                                    target: "leveler_runtime_lifecycle",
                                    event = "runtime_idle_eviction_committed",
                                    idle_for_ms = since.elapsed().as_millis() as u64,
                                    effective_idle_timeout_secs = timeout.as_secs(),
                                    "retiring an idle daemon"
                                );
                                return;
                            }
                            window = None;
                            self.observe(Busy, None, None);
                        } else {
                            delay = delay.min(remaining);
                        }
                    }
                    Err(error) => {
                        window = None;
                        if self.observe(
                            ObservationFailed,
                            None,
                            Some(RuntimeIdleEvictionError::BlockerObservationFailed),
                        ) {
                            tracing::error!(
                                target: "leveler_runtime_lifecycle",
                                event = "runtime_idle_observation_failed",
                                %error,
                                "idle blockers could not be observed"
                            );
                        }
                    }
                }
            } else {
                window = None;
                self.observe(Busy, None, None);
            }
            tokio::select! {
                _ = shutdown.cancelled() => {
                    self.observe(Retiring, None, None);
                    return;
                }
                _ = tokio::time::sleep(delay) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_client_and_turn_between_polls_restart_the_idle_window() {
        let observer = IdleEviction::default();
        let shutdown = CancellationToken::new();
        let clients = LocalWaiters::new();
        let active = Arc::new(ActiveTurns::default());
        let idle_timeout = Duration::from_secs(1);
        observer.spawn(
            shutdown.clone(),
            clients.clone(),
            active.clone(),
            Arc::new(BackgroundTaskRegistry::default()),
            idle_timeout,
        );
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert_eq!(
            observer.status().unwrap().state,
            RuntimeIdleEvictionState::Counting
        );
        drop(clients.attach());
        tokio::time::sleep(Duration::from_millis(950)).await;
        assert!(
            !shutdown.is_cancelled(),
            "the short attachment reset the window"
        );
        let lease = active
            .admit(&leveler_core::SessionId::new("brief-turn"))
            .unwrap();
        assert!(active.finish(&lease));
        tokio::time::sleep(Duration::from_millis(950)).await;
        assert!(!shutdown.is_cancelled(), "the short turn reset the window");
        tokio::time::timeout(idle_timeout, shutdown.cancelled())
            .await
            .expect("an uninterrupted idle window expires");
    }

    #[tokio::test(start_paused = true)]
    async fn an_unreadable_background_registry_restarts_the_idle_window_after_recovery() {
        use leveler_execution::execution_host::ExecutionHostConfig;

        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("execution-host");
        std::fs::create_dir(&state).unwrap();
        let ready = state.join("ready.json");
        std::fs::write(&ready, "broken readiness").unwrap();
        let registry = BackgroundTaskRegistry::default().with_execution_host(ExecutionHostConfig {
            state_dir: state,
            repo_root: dir.path().into(),
            executable: std::path::PathBuf::from("unused-test-executable"),
        });
        let observer = IdleEviction::default();
        let shutdown = CancellationToken::new();
        let idle_timeout = Duration::from_secs(1);
        observer.spawn(
            shutdown.clone(),
            LocalWaiters::new(),
            Arc::new(ActiveTurns::default()),
            Arc::new(registry),
            idle_timeout,
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
        let status = observer.status().unwrap();
        assert_eq!(status.state, RuntimeIdleEvictionState::ObservationFailed);
        assert_eq!(
            status.last_error,
            Some(RuntimeIdleEvictionError::BlockerObservationFailed)
        );
        assert_eq!(status.idle_for_ms, None);
        assert!(!shutdown.is_cancelled());
        std::fs::remove_file(ready).unwrap();
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert_eq!(
            observer.status().unwrap().state,
            RuntimeIdleEvictionState::Counting
        );
        assert!(
            !shutdown.is_cancelled(),
            "failed probes did not contribute idle time"
        );
        tokio::time::timeout(idle_timeout, shutdown.cancelled())
            .await
            .expect("successful observations establish a new idle window");
    }
}
