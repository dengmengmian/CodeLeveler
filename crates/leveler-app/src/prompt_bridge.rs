use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::sync::{broadcast, oneshot};
use tokio_util::sync::CancellationToken;

use leveler_agent::{ClarificationRequest, Clarifier};
use leveler_core::{ApprovalId, ClarificationId, SessionId, TurnId};
use leveler_execution::{ApprovalDecision, ApprovalRequest, Approver, RiskLevel};

use leveler_client_protocol::{
    ClientError, RuntimeEvent, UiApprovalRequest, UiClarificationRequest,
};

#[cfg(not(test))]
fn control_response_timeout() -> std::time::Duration {
    std::time::Duration::from_secs(5 * 60)
}

#[cfg(test)]
fn control_response_timeout() -> std::time::Duration {
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
    async fn clarify(&self, request: &ClarificationRequest) -> String {
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
            return String::new();
        }
        // If the UI goes away or the turn is cancelled without answering, the
        // model proceeds on its own — never block the turn indefinitely.
        let answer = tokio::select! {
            answer = rx => answer.unwrap_or_default(),
            _ = self.cancel.cancelled() => String::new(),
            _ = tokio::time::sleep(control_response_timeout()) => String::new(),
        };
        self.pending.lock().unwrap().remove(&request.id);
        answer
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
        let ui = UiApprovalRequest {
            id: request.id.clone(),
            tool: request.tool.clone(),
            summary: request.description.clone(),
            command: request.command.clone(),
            risks: risk_bullets(request),
        };
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
        // If the UI goes away or the turn is cancelled without answering, default
        // to the safe (Deny) decision instead of hanging the blocking thread.
        let decision = tokio::select! {
            decision = rx => decision.unwrap_or(ApprovalDecision::Deny),
            _ = self.cancel.cancelled() => ApprovalDecision::Deny,
            _ = tokio::time::sleep(control_response_timeout()) => ApprovalDecision::Deny,
        };
        self.pending.lock().unwrap().remove(&request.id);
        decision
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
            call_id: "call-a".to_string(),
            action_fingerprint: "fingerprint-a".to_string(),
            tool: "run_command".to_string(),
            risk: RiskLevel::Destructive,
            description: "dangerous".to_string(),
            command: Some("rm file".to_string()),
            paths: Vec::new(),
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
        assert_eq!(answer, "");
        assert!(pending.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn unanswered_approval_times_out_to_deny() {
        let (events, _receiver) = broadcast::channel(1);
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
        .expect("approval must have a bounded response window");
        assert_eq!(decision, ApprovalDecision::Deny);
        assert!(pending.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn unanswered_clarification_times_out_to_empty_answer() {
        let (events, _receiver) = broadcast::channel(1);
        let pending: PendingClarifications = Arc::new(Mutex::new(HashMap::new()));
        let clarifier = ChannelClarifier {
            events,
            pending: pending.clone(),
            cancel: CancellationToken::new(),
            session_id: SessionId::new("session-a"),
        };
        let request = ClarificationRequest {
            id: ClarificationId::new("clarification-timeout"),
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
        .expect("clarification must have a bounded response window");
        assert_eq!(answer, "");
        assert!(pending.lock().unwrap().is_empty());
    }
}
