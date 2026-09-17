use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::sync::{broadcast, oneshot};
use tokio_util::sync::CancellationToken;

use leveler_agent::{ClarificationRequest, Clarifier, ClarifyOutcome};
use leveler_core::{ApprovalId, ClarificationId, SessionId, TurnId};
use leveler_execution::{ApprovalDecision, ApprovalRequest, Approver, RiskLevel};

use leveler_client_protocol::{
    ClientError, RuntimeEvent, UiApprovalRequest, UiClarificationRequest,
};

/// How often a parked question or approval checks that somebody is still
/// there to answer it.
///
/// A deadline was the wrong shape: it denied commands and skipped questions
/// after five minutes, punishing the person who went to read the code the
/// prompt is about. What actually ends the wait is nobody being left to
/// answer — a client that disconnected, a runtime with no UI attached.
#[cfg(not(test))]
fn control_liveness_tick() -> std::time::Duration {
    std::time::Duration::from_secs(10)
}

#[cfg(test)]
fn control_liveness_tick() -> std::time::Duration {
    std::time::Duration::from_millis(10)
}

/// Pending approvals keyed by id: the approver parks a oneshot sender here and
/// the client resolves it when the UI answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingBinding {
    pub(crate) session_id: SessionId,
    pub(crate) turn_id: Option<TurnId>,
    pub(crate) tool: String,
    pub(crate) tool_call_id: String,
    pub(crate) action_fingerprint: String,
}

impl PendingBinding {
    fn for_approval(session_id: SessionId, request: &ApprovalRequest) -> Self {
        Self {
            session_id,
            turn_id: request.turn_id.clone(),
            tool: request.tool.clone(),
            tool_call_id: request.call_id.clone(),
            action_fingerprint: request.action_fingerprint.clone(),
        }
    }

    fn for_clarification(session_id: SessionId, request: &ClarificationRequest) -> Self {
        Self {
            session_id,
            turn_id: request.turn_id.clone(),
            tool: request.tool.clone(),
            tool_call_id: request.call_id.clone(),
            action_fingerprint: request.action_fingerprint.clone(),
        }
    }
}

pub(crate) struct PendingApproval {
    pub(crate) binding: PendingBinding,
    pub(crate) request: UiApprovalRequest,
    pub(crate) reply: oneshot::Sender<ApprovalDecision>,
}
pub(crate) type PendingApprovals = Arc<Mutex<HashMap<ApprovalId, PendingApproval>>>;

/// Pending clarifications keyed by id (spec §35).
pub(crate) struct PendingClarification {
    pub(crate) binding: PendingBinding,
    pub(crate) request: UiClarificationRequest,
    pub(crate) reply: oneshot::Sender<String>,
}
pub(crate) type PendingClarifications = Arc<Mutex<HashMap<ClarificationId, PendingClarification>>>;

pub(crate) fn resolve_approval(
    pending: &PendingApprovals,
    request_id: &ApprovalId,
    decision: ApprovalDecision,
) -> Result<(), ClientError> {
    let request = pending.lock().unwrap().remove(request_id).ok_or_else(|| {
        ClientError::Runtime("pending approval not found or already resolved".to_string())
    })?;
    request.reply.send(decision).map_err(|_| {
        ClientError::Runtime("pending approval is no longer waiting for a response".to_string())
    })
}

pub(crate) fn resolve_clarification(
    pending: &PendingClarifications,
    request_id: &ClarificationId,
    answer: String,
) -> Result<(), ClientError> {
    let request = pending.lock().unwrap().remove(request_id).ok_or_else(|| {
        ClientError::Runtime("pending clarification not found or already resolved".to_string())
    })?;
    request.reply.send(answer).map_err(|_| {
        ClientError::Runtime(
            "pending clarification is no longer waiting for a response".to_string(),
        )
    })
}

pub(crate) fn validate_pending_session(
    envelope_session: &SessionId,
    pending_session: Option<SessionId>,
) -> Result<(), ClientError> {
    let target = pending_session.ok_or_else(|| {
        ClientError::Runtime("pending request not found or already resolved".to_string())
    })?;
    if &target != envelope_session {
        return Err(ClientError::Runtime(format!(
            "envelope/pending-request session mismatch: envelope targets {}, request belongs to {}",
            envelope_session.as_str(),
            target.as_str()
        )));
    }
    Ok(())
}

/// A [`Clarifier`] that asks the UI over the protocol and awaits the answer.
pub(crate) struct ChannelClarifier {
    pub(crate) events: broadcast::Sender<RuntimeEvent>,
    pub(crate) pending: PendingClarifications,
    /// The current turn's cancel token, so a pending question is released (with
    /// the safe default) when the turn is cancelled instead of hanging forever.
    pub(crate) cancel: CancellationToken,
    pub(crate) session_id: SessionId,
}

#[async_trait]
impl Clarifier for ChannelClarifier {
    async fn clarify(&self, request: &ClarificationRequest) -> ClarifyOutcome {
        let ui = UiClarificationRequest {
            id: request.id.clone(),
            question: request.question.clone(),
            options: request.options.clone(),
        };
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(
            request.id.clone(),
            PendingClarification {
                binding: PendingBinding::for_clarification(self.session_id.clone(), request),
                request: ui.clone(),
                reply: tx,
            },
        );
        if self
            .events
            .send(RuntimeEvent::ClarificationRequested { request: ui })
            .is_err()
        {
            self.pending.lock().unwrap().remove(&request.id);
            // Nobody is subscribed: the question was never delivered. This is
            // an unattended run, not a user reply (R004 F2).
            return ClarifyOutcome::Unattended;
        }
        // The wait ends when the question is answered, the turn is cancelled,
        // or nobody is left to answer — each keeping its own meaning instead
        // of collapsing to a fake empty "answer".
        let mut rx = rx;
        let outcome = loop {
            tokio::select! {
                answer = &mut rx => break match answer {
                    // The wire keeps `answer: String` with "" meaning the user
                    // explicitly skipped (Esc / empty submit).
                    Ok(text) if text.trim().is_empty() => ClarifyOutcome::Skipped,
                    Ok(text) => ClarifyOutcome::Answered(text),
                    Err(_) => ClarifyOutcome::Unattended,
                },
                _ = self.cancel.cancelled() => break ClarifyOutcome::Cancelled,
                _ = tokio::time::sleep(control_liveness_tick()) => {
                    if self.events.receiver_count() == 0 {
                        break ClarifyOutcome::Unattended;
                    }
                }
            }
        };
        self.pending.lock().unwrap().remove(&request.id);
        // However it resolved (answered, cancelled, or timed out), tell every
        // connected client to dismiss the prompt so a second client can't answer
        // a clarification that no longer exists.
        let _ = self.events.send(RuntimeEvent::ClarificationResolved {
            id: request.id.clone(),
        });
        outcome
    }
}

/// An [`Approver`] that asks the UI over the protocol and awaits the answer.
pub(crate) struct ChannelApprover {
    pub(crate) events: broadcast::Sender<RuntimeEvent>,
    pub(crate) pending: PendingApprovals,
    /// The current turn's cancel token, so a pending approval is released (as a
    /// Deny) when the turn is cancelled instead of hanging the blocking thread.
    pub(crate) cancel: CancellationToken,
    pub(crate) session_id: SessionId,
}

#[async_trait]
impl Approver for ChannelApprover {
    async fn decide(&self, request: &ApprovalRequest) -> ApprovalDecision {
        let ui = ui_approval_request(request);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(
            request.id.clone(),
            PendingApproval {
                binding: PendingBinding::for_approval(self.session_id.clone(), request),
                request: ui.clone(),
                reply: tx,
            },
        );
        if self
            .events
            .send(RuntimeEvent::ApprovalRequested { request: ui })
            .is_err()
        {
            self.pending.lock().unwrap().remove(&request.id);
            return ApprovalDecision::Deny;
        }
        // If the turn is cancelled, or the UI that could answer is gone,
        // default to the safe (Deny) decision instead of waiting forever.
        let mut rx = rx;
        let decision = loop {
            tokio::select! {
                decision = &mut rx => break decision.unwrap_or(ApprovalDecision::Deny),
                _ = self.cancel.cancelled() => break ApprovalDecision::Deny,
                _ = tokio::time::sleep(control_liveness_tick()) => {
                    if self.events.receiver_count() == 0 {
                        break ApprovalDecision::Deny;
                    }
                }
            }
        };
        self.pending.lock().unwrap().remove(&request.id);
        // However it resolved (answered, cancelled, or timed out), tell every
        // connected client to dismiss the prompt so a second client can't answer
        // an approval that no longer exists.
        let _ = self.events.send(RuntimeEvent::ApprovalResolved {
            id: request.id.clone(),
        });
        decision
    }
}

/// The approval as a client shows it.
fn ui_approval_request(request: &ApprovalRequest) -> UiApprovalRequest {
    UiApprovalRequest {
        id: request.id.clone(),
        tool: request.tool.clone(),
        summary: request.description.clone(),
        command: request.command.clone(),
        risks: risk_bullets(request),
        // The call this decision is holding, so the UI can stop painting it
        // as work in progress while it waits on the human.
        call_id: Some(request.call_id.clone()),
        always_persists: request.always_persists(),
    }
}

/// Build human-readable risk bullets from the request's risk level and paths.
fn risk_bullets(request: &ApprovalRequest) -> Vec<String> {
    let mut risks = Vec::new();
    match request.risk {
        RiskLevel::Network => risks.push("将访问网络".to_string()),
        RiskLevel::Destructive => risks.push("可能造成破坏性变更".to_string()),
        RiskLevel::Privileged => risks.push("需要提升权限".to_string()),
        RiskLevel::Safe | RiskLevel::WorkspaceWrite => {}
    }
    for path in &request.paths {
        risks.push(format!("涉及路径 {}", path.display()));
    }
    risks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approval_request() -> ApprovalRequest {
        ApprovalRequest {
            id: ApprovalId::new("approval-disconnect"),
            turn_id: Some(leveler_core::TurnId::new("turn-a")),
            agent_id: None,
            call_id: "call-a".to_string(),
            action_fingerprint: "fingerprint-a".to_string(),
            tool: "run_command".to_string(),
            risk: RiskLevel::Destructive,
            description: "dangerous".to_string(),
            command: Some("rm file".to_string()),
            paths: Vec::new(),
        }
    }

    /// "Always allow" is offered only where the runtime would write a rule for
    /// it: a client must not show a choice whose promised effect never happens.
    #[test]
    fn always_is_offered_only_when_the_runtime_would_persist_a_rule() {
        let request = |tool: &str, command: Option<&str>, paths: &[&str]| ApprovalRequest {
            tool: tool.to_string(),
            command: command.map(str::to_string),
            paths: paths.iter().map(std::path::PathBuf::from).collect(),
            ..approval_request()
        };
        for (tool, command, paths, persists) in [
            ("run_command", Some("cargo test"), &[][..], true),
            ("apply_patch", None, &["src/lib.rs"][..], true),
            ("apply_patch", None, &[][..], false),
            ("write_file", None, &["src/lib.rs"][..], false),
            ("save_agent", None, &[][..], false),
            ("delete_agent", None, &[][..], false),
            ("remember", None, &[][..], false),
            ("forget", None, &[][..], false),
        ] {
            assert_eq!(
                ui_approval_request(&request(tool, command, paths)).always_persists,
                persists,
                "{tool} {command:?} {paths:?}"
            );
        }
    }

    #[test]
    fn pending_binding_captures_turn_tool_call_and_action() {
        let request = approval_request();
        let binding = PendingBinding::for_approval(SessionId::new("session-a"), &request);
        assert_eq!(binding.turn_id.as_ref().unwrap().as_str(), "turn-a");
        assert_eq!(binding.tool, "run_command");
        assert_eq!(binding.tool_call_id, "call-a");
        assert_eq!(binding.action_fingerprint, "fingerprint-a");
    }

    #[tokio::test]
    async fn only_the_first_approval_answer_is_accepted() {
        let pending: PendingApprovals = Arc::new(Mutex::new(HashMap::new()));
        let request = approval_request();
        let (reply, answer) = oneshot::channel();
        pending.lock().unwrap().insert(
            request.id.clone(),
            PendingApproval {
                binding: PendingBinding::for_approval(SessionId::new("session-a"), &request),
                request: UiApprovalRequest {
                    id: request.id.clone(),
                    tool: request.tool.clone(),
                    summary: request.description.clone(),
                    command: request.command.clone(),
                    risks: vec![],
                    call_id: Some(request.call_id.clone()),
                    always_persists: true,
                },
                reply,
            },
        );

        resolve_approval(&pending, &request.id, ApprovalDecision::ApproveOnce).unwrap();
        assert_eq!(answer.await.unwrap(), ApprovalDecision::ApproveOnce);
        let second = resolve_approval(&pending, &request.id, ApprovalDecision::Deny).unwrap_err();
        assert!(second.to_string().contains("already resolved"));
    }

    #[test]
    fn pending_request_is_bound_to_its_session() {
        let a = SessionId::new("session-a");
        let b = SessionId::new("session-b");
        assert!(validate_pending_session(&a, Some(a.clone())).is_ok());
        let mismatch = validate_pending_session(&a, Some(b)).unwrap_err();
        assert!(mismatch.to_string().contains("session mismatch"));
        let missing = validate_pending_session(&a, None).unwrap_err();
        assert!(missing.to_string().contains("not found"));
    }

    #[tokio::test]
    async fn disconnected_client_denies_approval_without_hanging() {
        let (events, receiver) = broadcast::channel(1);
        drop(receiver);
        let pending: PendingApprovals = Arc::new(Mutex::new(HashMap::new()));
        let approver = ChannelApprover {
            events,
            pending: pending.clone(),
            cancel: CancellationToken::new(),
            session_id: SessionId::new("session-a"),
        };

        let decision = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            approver.decide(&approval_request()),
        )
        .await
        .expect("disconnected approval must resolve");
        assert_eq!(decision, ApprovalDecision::Deny);
        assert!(pending.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn disconnected_client_skips_clarification_without_hanging() {
        let (events, receiver) = broadcast::channel(1);
        drop(receiver);
        let pending: PendingClarifications = Arc::new(Mutex::new(HashMap::new()));
        let clarifier = ChannelClarifier {
            events,
            pending: pending.clone(),
            cancel: CancellationToken::new(),
            session_id: SessionId::new("session-a"),
        };
        let request = ClarificationRequest {
            id: ClarificationId::new("clarification-disconnect"),
            turn_id: Some(leveler_core::TurnId::new("turn-a")),
            tool: "ask_user".to_string(),
            call_id: "call-a".to_string(),
            action_fingerprint: "fingerprint-a".to_string(),
            question: "which?".to_string(),
            options: vec![],
        };

        let answer = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            clarifier.clarify(&request),
        )
        .await
        .expect("disconnected clarification must resolve");
        // Never delivered ⇒ unattended, NOT an (empty) user answer (R004 F2).
        assert_eq!(answer, ClarifyOutcome::Unattended);
        assert!(pending.lock().unwrap().is_empty());
    }

    /// Someone is reading the prompt. Denying it on a clock turned "I stepped
    /// away for five minutes" into a refused command the user never refused.
    #[tokio::test]
    async fn a_watched_approval_waits_instead_of_denying_itself() {
        let (events, mut receiver) = broadcast::channel(4);
        let pending: PendingApprovals = Arc::new(Mutex::new(HashMap::new()));
        let approver = ChannelApprover {
            events,
            pending: pending.clone(),
            cancel: CancellationToken::new(),
            session_id: SessionId::new("session-a"),
        };
        let request = approval_request();
        let id = request.id.clone();
        let decide = tokio::spawn(async move { approver.decide(&request).await });
        let _ = receiver.recv().await;

        // Many deadlines later, still waiting for the person.
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        assert!(!decide.is_finished(), "a watched approval waits");

        resolve_approval(&pending, &id, ApprovalDecision::ApproveOnce).unwrap();
        let decision = tokio::time::timeout(std::time::Duration::from_millis(200), decide)
            .await
            .expect("an answered approval resolves")
            .unwrap();
        assert_eq!(decision, ApprovalDecision::ApproveOnce);
        assert!(pending.lock().unwrap().is_empty());
    }

    /// The client goes away while the prompt is open: nobody can answer, so it
    /// stops waiting — quickly, and as a denial.
    #[tokio::test]
    async fn an_approval_nobody_watches_any_more_denies() {
        let (events, mut receiver) = broadcast::channel(4);
        let pending: PendingApprovals = Arc::new(Mutex::new(HashMap::new()));
        let approver = ChannelApprover {
            events,
            pending: pending.clone(),
            cancel: CancellationToken::new(),
            session_id: SessionId::new("session-a"),
        };
        let decide = tokio::spawn(async move { approver.decide(&approval_request()).await });
        let _ = receiver.recv().await;
        drop(receiver);

        let decision = tokio::time::timeout(std::time::Duration::from_millis(200), decide)
            .await
            .expect("an unwatched approval resolves")
            .unwrap();
        assert_eq!(decision, ApprovalDecision::Deny);
        assert!(pending.lock().unwrap().is_empty());
    }

    /// A question is asked because the answer matters; while a client is
    /// attached it waits for one. It used to expire after five minutes, so an
    /// answer typed at 5m01s arrived after the model had moved on alone.
    #[tokio::test]
    async fn a_watched_question_waits_for_its_answer() {
        let (events, mut receiver) = broadcast::channel(4);
        let pending: PendingClarifications = Arc::new(Mutex::new(HashMap::new()));
        let clarifier = ChannelClarifier {
            events,
            pending: pending.clone(),
            cancel: CancellationToken::new(),
            session_id: SessionId::new("session-a"),
        };
        let request = ClarificationRequest {
            id: ClarificationId::new("clarification-watched"),
            turn_id: Some(leveler_core::TurnId::new("turn-a")),
            tool: "ask_user".to_string(),
            call_id: "call-a".to_string(),
            action_fingerprint: "fingerprint-a".to_string(),
            question: "which?".to_string(),
            options: vec![],
        };
        let id = request.id.clone();
        let ask = tokio::spawn(async move { clarifier.clarify(&request).await });
        let _ = receiver.recv().await;

        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        assert!(!ask.is_finished(), "a watched question waits");

        resolve_clarification(&pending, &id, "第二个".to_string()).unwrap();
        let outcome = tokio::time::timeout(std::time::Duration::from_millis(200), ask)
            .await
            .expect("an answered question resolves")
            .unwrap();
        assert_eq!(outcome, ClarifyOutcome::Answered("第二个".into()));
        assert!(pending.lock().unwrap().is_empty());
    }

    /// Nobody is left to answer: the question stops waiting, and says that is
    /// what happened — not that the user skipped it.
    #[tokio::test]
    async fn a_question_nobody_watches_any_more_is_unattended() {
        let (events, mut receiver) = broadcast::channel(4);
        let pending: PendingClarifications = Arc::new(Mutex::new(HashMap::new()));
        let clarifier = ChannelClarifier {
            events,
            pending: pending.clone(),
            cancel: CancellationToken::new(),
            session_id: SessionId::new("session-a"),
        };
        let request = ClarificationRequest {
            id: ClarificationId::new("clarification-abandoned"),
            turn_id: Some(leveler_core::TurnId::new("turn-a")),
            tool: "ask_user".to_string(),
            call_id: "call-a".to_string(),
            action_fingerprint: "fingerprint-a".to_string(),
            question: "which?".to_string(),
            options: vec![],
        };
        let ask = tokio::spawn(async move { clarifier.clarify(&request).await });
        let _ = receiver.recv().await;
        drop(receiver);

        let outcome = tokio::time::timeout(std::time::Duration::from_millis(200), ask)
            .await
            .expect("an unwatched question resolves")
            .unwrap();
        assert_eq!(outcome, ClarifyOutcome::Unattended);
        assert!(pending.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn answered_and_skipped_clarifications_stay_distinct() {
        for (wire_answer, expected) in [
            (
                "用第二个方案".to_string(),
                ClarifyOutcome::Answered("用第二个方案".into()),
            ),
            (String::new(), ClarifyOutcome::Skipped),
        ] {
            let (events, mut receiver) = broadcast::channel(4);
            let pending: PendingClarifications = Arc::new(Mutex::new(HashMap::new()));
            let clarifier = ChannelClarifier {
                events,
                pending: pending.clone(),
                cancel: CancellationToken::new(),
                session_id: SessionId::new("session-a"),
            };
            let request = ClarificationRequest {
                id: ClarificationId::new("clarification-answered"),
                turn_id: Some(leveler_core::TurnId::new("turn-a")),
                tool: "ask_user".to_string(),
                call_id: "call-a".to_string(),
                action_fingerprint: "fingerprint-a".to_string(),
                question: "which?".to_string(),
                options: vec![],
            };
            let pending_for_reply = pending.clone();
            let id = request.id.clone();
            let replier = tokio::spawn(async move {
                // Wait for the request event, then answer over the wire shape.
                let _ = receiver.recv().await;
                resolve_clarification(&pending_for_reply, &id, wire_answer).unwrap();
            });
            let outcome = tokio::time::timeout(
                std::time::Duration::from_millis(200),
                clarifier.clarify(&request),
            )
            .await
            .expect("answered clarification must resolve");
            replier.await.unwrap();
            assert_eq!(outcome, expected);
            assert!(pending.lock().unwrap().is_empty());
        }
    }
}
