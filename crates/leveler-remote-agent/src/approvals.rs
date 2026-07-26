//! The remote approval timeout: the agent's answer when nobody can answer.
//!
//! A phone can leave an approval prompt unanswered forever — screen off, app
//! backgrounded, phone in a pocket — and the turn behind it waits. The design
//! makes the *agent* the authority for that timeout, not the relay (which must
//! never author a command) and not the app (which is the thing that went away).
//!
//! The rule it enforces is deliberately narrow. The countdown runs only while a
//! remote stream is the sole way to answer:
//!
//! | attached | countdown |
//! | --- | --- |
//! | remote only | yes — nobody else can answer |
//! | local only | no — this is not a remote decision |
//! | both | no — a phone's policy must not cut off the person at the keyboard |
//! | both, then the local UI leaves | yes, from that moment, with a full window |
//!
//! [`ApprovalTimeoutState`] holds that logic; this module is the part that
//! watches, waits, and acts.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use leveler_client_protocol::{
    ApprovalDecision, ApprovalId, ClientCommand, ClientOrigin, CommandEnvelope, CommandId,
    ProtocolEnvelope, RuntimeEvent, SessionId,
};
use leveler_local_transport::LocalRuntimeService;
use leveler_remote_protocol::policy::{ApprovalTimeoutState, TimerTransition, Waiters};
use tokio::sync::broadcast;
use tokio::time::Instant;

/// How often the watcher asks the daemon who is attached.
///
/// Only while an approval is actually pending, so an idle host does no polling
/// at all. A second's granularity on a two-minute window costs nothing and
/// avoids a push mechanism the daemon does not have.
pub const WAITER_POLL: Duration = Duration::from_secs(1);

/// What the watcher needs to know about one project's remote presence.
///
/// Shared with the tunnel: it opens and closes the streams, and those are what
/// "a remote waiter" means.
#[derive(Clone, Default)]
pub(crate) struct RemotePresence {
    /// Open interactive streams on this project.
    streams: Arc<AtomicUsize>,
    /// The session the phone is looking at, from the last frame it sent.
    session: Arc<Mutex<Option<String>>>,
}

impl RemotePresence {
    pub(crate) fn attach(&self) {
        self.streams.fetch_add(1, Ordering::SeqCst);
    }

    pub(crate) fn detach(&self) {
        // Saturating, so a double close cannot wrap the count to a huge number
        // and keep the timeout armed forever.
        let _ = self
            .streams
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                Some(count.saturating_sub(1))
            });
    }

    pub(crate) fn count(&self) -> usize {
        self.streams.load(Ordering::SeqCst)
    }

    /// Record which session a device's frame targeted.
    pub(crate) fn note_session(&self, session_id: &str) {
        *self.session.lock().unwrap() = Some(session_id.to_string());
    }

    fn session(&self) -> Option<String> {
        self.session.lock().unwrap().clone()
    }
}

/// One pending approval and the state of its countdown.
struct Countdown {
    state: ApprovalTimeoutState,
    /// When the auto-deny is due. `None` while disarmed.
    deadline: Option<Instant>,
}

/// Watch one project's approvals until the task is aborted.
///
/// `device_id` names the phone this watch was started for; it only ever reaches
/// the audit line, as [`ClientOrigin::RemoteTimeout`] — the runtime is told a
/// `Deny`, and a `Deny` from a timeout must not read in the log as though the
/// phone's user pressed it.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn watch_approvals(
    runtime: Arc<dyn LocalRuntimeService>,
    project_id: String,
    device_id: String,
    presence: RemotePresence,
    timeout: Duration,
    poll: Duration,
    audit: Option<Arc<crate::audit::AuditLog>>,
) {
    let mut events = runtime.subscribe();
    let mut ticker = tokio::time::interval(poll);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut pending: HashMap<ApprovalId, Countdown> = HashMap::new();

    loop {
        tokio::select! {
            event = events.recv() => match event {
                Ok(RuntimeEvent::ApprovalRequested { request }) => {
                    pending.insert(
                        request.id,
                        Countdown { state: ApprovalTimeoutState::new(), deadline: None },
                    );
                }
                // Answered by a person on either side, or by the runtime itself.
                Ok(RuntimeEvent::ApprovalResolved { id }) => {
                    pending.remove(&id);
                }
                Ok(_) => {}
                // A missed event could hide a resolution. The stale countdown
                // then fires a `Deny` the runtime refuses, because the request
                // is gone — noisy, never wrong.
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::warn!(skipped, %project_id, "approval watch lagged");
                }
                Err(broadcast::error::RecvError::Closed) => return,
            },
            _ = ticker.tick() => {
                if pending.is_empty() {
                    continue;
                }
                // Asked fresh each tick: the answer changes when a terminal
                // opens or closes, and that is exactly what the rule turns on.
                let local = match runtime.local_waiter_count().await {
                    Ok(count) => count,
                    // Unknown means "assume someone is watching": suppressing an
                    // auto-deny is recoverable, expiring a prompt a person was
                    // answering is not.
                    Err(error) => {
                        tracing::warn!(%error, "could not read the local waiter count");
                        1
                    }
                };
                let waiters = Waiters { local, remote: presence.count() };

                let mut due = Vec::new();
                for (id, countdown) in pending.iter_mut() {
                    match countdown.state.observe(waiters) {
                        TimerTransition::Arm => {
                            // From now, not from when the approval was raised: a
                            // prompt somebody watched for an hour gets the whole
                            // window once they close their terminal.
                            countdown.deadline = Some(Instant::now() + timeout);
                        }
                        TimerTransition::Disarm => countdown.deadline = None,
                        TimerTransition::Unchanged => {}
                    }
                    if countdown.deadline.is_some_and(|at| Instant::now() >= at) {
                        due.push(id.clone());
                    }
                }

                for id in due {
                    if let Some(mut countdown) = pending.remove(&id) {
                        countdown.state.resolved();
                        if let Some(audit) = &audit {
                            audit.record(crate::audit::AuditEvent::ApprovalTimeout {
                                device: crate::audit::hashed(&device_id),
                                project: project_id.clone(),
                                approval: id.to_string(),
                            });
                        }
                        deny(&runtime, &presence, &project_id, &device_id, id).await;
                    }
                }
            }
        }
    }
}

/// Deny one approval on the host's behalf.
///
/// The session comes from the last frame the phone sent, which is the session it
/// is looking at. A wrong guess is not a wrong decision: the runtime binds each
/// pending request to its own session and refuses a decision aimed elsewhere, so
/// the failure mode is a logged miss rather than a denial in the wrong place.
async fn deny(
    runtime: &Arc<dyn LocalRuntimeService>,
    presence: &RemotePresence,
    project_id: &str,
    device_id: &str,
    request_id: ApprovalId,
) {
    let origin = ClientOrigin::RemoteTimeout {
        device_id: device_id.to_string(),
    };
    let Some(session_id) = presence.session() else {
        tracing::warn!(
            origin = origin.label(),
            %project_id,
            approval = %request_id,
            "a remote-only approval timed out, but no session is known to answer in"
        );
        return;
    };

    // Derived from the approval, so a retry cannot deny twice.
    let command_id = CommandId::new(format!("remote-timeout-{}", request_id.as_str()));
    let envelope = CommandEnvelope {
        command_id,
        session_id: SessionId::new(session_id),
        expected_version: None,
        issued_at: chrono::Utc::now().to_rfc3339(),
        command: ClientCommand::ApprovalDecision {
            request_id: request_id.clone(),
            decision: ApprovalDecision::Deny,
        },
    };

    match runtime
        .deliver_protocol(ProtocolEnvelope::wrap(envelope))
        .await
    {
        Ok(()) => tracing::info!(
            origin = origin.label(),
            device_id = origin.device_id(),
            %project_id,
            approval = %request_id,
            "auto-denied a remote-only approval that timed out"
        ),
        Err(error) => tracing::warn!(
            origin = origin.label(),
            %project_id,
            approval = %request_id,
            %error,
            "the timed-out approval could not be denied"
        ),
    }
}
