use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::sync::{broadcast, oneshot};
use tokio_util::sync::CancellationToken;

use leveler_agent::{ClarificationQuestionKind, ClarificationRequest, Clarifier, ClarifyOutcome};
use leveler_core::{ApprovalId, ClarificationId, SessionId, TurnId};
use leveler_core::{Capability, GrantBinding, ResourceIdentity};
use leveler_execution::{ApprovalDecision, ApprovalOutcome, ApprovalRequest, Approver, RiskLevel};

use leveler_client_protocol::{
    ClientError, RuntimeEvent, UiApprovalRequest, UiClarificationQuestion, UiClarificationRequest,
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
    pub(crate) reply: oneshot::Sender<PendingReply>,
}

/// What a parked approval waiter is told.
///
/// `Superseded` is not a decision: the permission profile changed while the
/// question was open, so the caller must re-resolve under the profile now in
/// force instead of granting or refusing. It never persists a grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PendingReply {
    Decided(ApprovalDecision),
    Superseded,
}

pub(crate) type PendingApprovals = Arc<Mutex<HashMap<ApprovalId, PendingApproval>>>;

/// Pending clarifications keyed by id (spec §35).
pub(crate) struct PendingClarification {
    pub(crate) binding: PendingBinding,
    pub(crate) request: UiClarificationRequest,
    pub(crate) reply: oneshot::Sender<String>,
}
pub(crate) type PendingClarifications = Arc<Mutex<HashMap<ClarificationId, PendingClarification>>>;

/// The awaiting future owns the waiter lifetime. Outer cancellation may drop
/// that future before its select branch runs, so cleanup must happen on Drop.
enum PendingWaiterGuard {
    Approval {
        pending: PendingApprovals,
        events: broadcast::Sender<RuntimeEvent>,
        id: ApprovalId,
    },
    Clarification {
        pending: PendingClarifications,
        events: broadcast::Sender<RuntimeEvent>,
        id: ClarificationId,
    },
}
impl Drop for PendingWaiterGuard {
    fn drop(&mut self) {
        match self {
            Self::Approval {
                pending,
                events,
                id,
            } => {
                pending.lock().unwrap().remove(id);
                let _ = events.send(RuntimeEvent::ApprovalResolved { id: id.clone() });
            }
            Self::Clarification {
                pending,
                events,
                id,
            } => {
                pending.lock().unwrap().remove(id);
                let _ = events.send(RuntimeEvent::ClarificationResolved { id: id.clone() });
            }
        }
    }
}

pub(crate) fn resolve_approval(
    pending: &PendingApprovals,
    request_id: &ApprovalId,
    decision: ApprovalDecision,
) -> Result<(), ClientError> {
    let request = pending.lock().unwrap().remove(request_id).ok_or_else(|| {
        ClientError::Runtime("pending approval not found or already resolved".to_string())
    })?;
    request
        .reply
        .send(PendingReply::Decided(decision))
        .map_err(|_| {
            ClientError::Runtime("pending approval is no longer waiting for a response".to_string())
        })
}

/// Void every parked permission approval belonging to `session_id`.
///
/// This is what a permission-profile change means for a question already
/// waiting: the question's premise is gone. The waiter is woken with
/// [`PendingReply::Superseded`] so the tool call is re-resolved under the new
/// profile — on Full that is `Allow` — instead of being answered under a mode
/// the user has left. Returns how many waiters were superseded.
///
/// Clarifications are deliberately untouched: `request_user_input` is not a
/// permission approval, and Full does not take away the model's ability to ask
/// the user a question.
pub(crate) fn supersede_pending_approvals(
    pending: &PendingApprovals,
    session_id: &SessionId,
) -> usize {
    // Collect first, send after releasing the lock: `send` wakes the waiter,
    // whose `PendingWaiterGuard::drop` takes the same lock.
    let mut superseded = Vec::new();
    {
        let mut map = pending.lock().unwrap();
        let ids: Vec<ApprovalId> = map
            .iter()
            .filter(|(_, entry)| entry.binding.session_id == *session_id)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            if let Some(entry) = map.remove(&id) {
                superseded.push(entry);
            }
        }
    }
    let count = superseded.len();
    for entry in superseded {
        // A closed receiver means the waiter already ended; nothing to wake.
        let _ = entry.reply.send(PendingReply::Superseded);
    }
    count
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
            questions: request
                .questions
                .iter()
                .map(|q| UiClarificationQuestion {
                    header: q.header.clone(),
                    question: q.question.clone(),
                    kind: match q.kind {
                        ClarificationQuestionKind::Single => {
                            leveler_client_protocol::ClarificationQuestionKind::Single
                        }
                        ClarificationQuestionKind::Multi => {
                            leveler_client_protocol::ClarificationQuestionKind::Multi
                        }
                        ClarificationQuestionKind::Text => {
                            leveler_client_protocol::ClarificationQuestionKind::Text
                        }
                    },
                    options: q.options.clone(),
                    allow_other: q.allow_other,
                    min_choices: q.min_choices,
                    max_choices: q.max_choices,
                })
                .collect(),
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
        let _waiter = PendingWaiterGuard::Clarification {
            pending: self.pending.clone(),
            events: self.events.clone(),
            id: request.id.clone(),
        };
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
        loop {
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
        }
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
        // A superseded question is not a decision. The only callers left on this
        // plain entry point are ones without a re-resolution loop, so treat it
        // as the safe refusal rather than inventing a grant.
        match self.decide_or_supersede(request).await {
            ApprovalOutcome::Decided(decision) => decision,
            ApprovalOutcome::Superseded => ApprovalDecision::Deny,
        }
    }

    async fn decide_or_supersede(&self, request: &ApprovalRequest) -> ApprovalOutcome {
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
        let _waiter = PendingWaiterGuard::Approval {
            pending: self.pending.clone(),
            events: self.events.clone(),
            id: request.id.clone(),
        };
        if self
            .events
            .send(RuntimeEvent::ApprovalRequested { request: ui })
            .is_err()
        {
            self.pending.lock().unwrap().remove(&request.id);
            return ApprovalOutcome::Decided(ApprovalDecision::Deny);
        }
        // If the turn is cancelled, or the UI that could answer is gone,
        // default to the safe (Deny) decision instead of waiting forever.
        let mut rx = rx;
        let reply = loop {
            tokio::select! {
                reply = &mut rx => break reply.unwrap_or(PendingReply::Decided(ApprovalDecision::Deny)),
                _ = self.cancel.cancelled() => break PendingReply::Decided(ApprovalDecision::Deny),
                _ = tokio::time::sleep(control_liveness_tick()) => {
                    if self.events.receiver_count() == 0 {
                        break PendingReply::Decided(ApprovalDecision::Deny);
                    }
                }
            }
        };
        match reply {
            PendingReply::Decided(decision) => ApprovalOutcome::Decided(decision),
            PendingReply::Superseded => ApprovalOutcome::Superseded,
        }
    }
}

/// The approval as a client shows it.
fn ui_approval_request(request: &ApprovalRequest) -> UiApprovalRequest {
    UiApprovalRequest {
        grant: request.grant.clone(),
        requires_human_consent: request.requires_human_consent(),
        id: request.id.clone(),
        tool: request.tool.clone(),
        summary: request.description.clone(),
        command: request.command.clone(),
        risks: consent_bullets(request),
        // The call this decision is holding, so the UI can stop painting it
        // as work in progress while it waits on the human.
        call_id: Some(request.call_id.clone()),
        always_persists: request.always_persists(),
    }
}

/// The bullets the approval prompt shows: the action's risk level, the paths
/// it names, and — when the action is bound to resolved resources — what it
/// will do to them.
///
/// This is where an approval stops being engine data and becomes a prompt. A
/// [`Capability`]/[`ResourceIdentity`] pair is an authorization match key: it
/// carries a project hash, a credential incarnation and an object identity,
/// and its names are a policy vocabulary. The bindings stay on the wire for the
/// clients that must decide which options to offer, but no surface renders them
/// — the consequence is projected here, once, instead of in every renderer.
fn consent_bullets(request: &ApprovalRequest) -> Vec<String> {
    let mut bullets = Vec::new();
    match request.risk {
        RiskLevel::Network => bullets.push("将访问网络".to_string()),
        RiskLevel::Destructive => bullets.push("可能造成破坏性变更".to_string()),
        RiskLevel::Privileged => bullets.push("需要提升权限".to_string()),
        RiskLevel::Safe | RiskLevel::WorkspaceWrite => {}
    }
    for path in &request.paths {
        bullets.push(format!("涉及路径 {}", path.display()));
    }
    if let Some(grant) = &request.grant {
        for binding in &grant.bindings {
            bullets.extend(effect_lines(binding));
        }
    }
    bullets
}

/// One binding as the lines a person reads: what the action does, then the
/// destination when the resource names one a person can recognise.
fn effect_lines(binding: &GrantBinding) -> Vec<String> {
    let described = |verb: &str, detail: &str| format!("{verb} {detail}");
    match (&binding.capability, &binding.resource) {
        // A remote's name is what the command itself uses; its destination is
        // what makes the consequence concrete.
        (Capability::RemoteRead, ResourceIdentity::ConfiguredRemote { remote_name, .. }) => {
            vec![described("读取远端仓库", remote_name)]
        }
        (
            Capability::RemoteMutate,
            ResourceIdentity::ConfiguredRemote {
                remote_name,
                canonical_url,
                ..
            },
        ) => remote_lines("修改远端仓库", remote_name, canonical_url),
        (
            Capability::RemoteForce,
            ResourceIdentity::ConfiguredRemote {
                remote_name,
                canonical_url,
                ..
            },
        ) => remote_lines(
            "强制修改远端仓库（可能覆盖他人提交）",
            remote_name,
            canonical_url,
        ),
        (Capability::FilesystemRead, ResourceIdentity::FilesystemPath { canonical_path, .. }) => {
            vec![described("读取文件", canonical_path)]
        }
        (Capability::FilesystemWrite, ResourceIdentity::FilesystemPath { canonical_path, .. }) => {
            vec![described("写入文件", canonical_path)]
        }
        (Capability::FilesystemDelete, ResourceIdentity::FilesystemPath { canonical_path, .. }) => {
            vec![described("删除文件", canonical_path)]
        }
        (Capability::BackgroundTaskObserve, ResourceIdentity::BackgroundTask { task_id, .. }) => {
            vec![described("观察后台任务", task_id)]
        }
        (Capability::BackgroundTaskControl, ResourceIdentity::BackgroundTask { task_id, .. }) => {
            vec![described("控制后台任务", task_id)]
        }
        (Capability::ExternalProcessControl, ResourceIdentity::ExternalProcess { pid, .. }) => {
            vec![described("控制外部进程", &pid.to_string())]
        }
        // A credential is named by the destination it authenticates to; its
        // incarnation identity is a hash and never part of the prompt.
        (Capability::CredentialUse, ResourceIdentity::Credential { host, .. }) => {
            vec![format!("使用 {host} 的 Git 凭据")]
        }
        (Capability::CredentialRawRead, ResourceIdentity::Credential { host, .. }) => {
            vec![format!("读取 {host} 的凭据原文")]
        }
        (Capability::CredentialRawWrite, ResourceIdentity::Credential { host, .. }) => {
            vec![format!("更换 {host} 的凭据来源")]
        }
        // A repository identity is a hash, so the capability's own wording is
        // the whole line — and the same fallback keeps a pair the engine does
        // not bind today from dropping its consequence.
        (capability, _) => vec![capability_line(*capability).to_string()],
    }
}

fn remote_lines(verb: &str, remote_name: &str, canonical_url: &str) -> Vec<String> {
    let mut lines = vec![format!("{verb} {remote_name}")];
    if let Some(target) = remote_target(canonical_url) {
        lines.push(format!("目标 {target}"));
    }
    lines
}

/// A remote URL as a person reads a destination: host and path, without the
/// scheme, the userinfo or the `.git` suffix. Anything unreadable is shown as
/// it is rather than silently dropped.
fn remote_target(canonical_url: &str) -> Option<String> {
    let trimmed = canonical_url.trim();
    if trimmed.is_empty() {
        return None;
    }
    // `git@host:owner/repo` is the scp form; its colon separates host and path,
    // which only holds when there is no scheme in front of it.
    let (rest, scp_form) = match trimmed.split_once("://") {
        Some((_, rest)) => (rest, false),
        None => (trimmed, true),
    };
    let rest = rest.rsplit_once('@').map_or(rest, |(_, rest)| rest);
    let rest = match rest.split_once(':') {
        Some((host, path)) if scp_form && !host.is_empty() && !host.contains('/') => {
            format!("{host}/{path}")
        }
        _ => rest.to_string(),
    };
    let rest = rest.strip_suffix(".git").unwrap_or(&rest);
    Some(rest.trim_end_matches('/').to_string())
}

/// One capability in the words a person consents to, for a resource that
/// contributes no readable detail.
fn capability_line(capability: Capability) -> &'static str {
    match capability {
        Capability::RepositoryRead => "读取仓库",
        Capability::RepositoryMutate => "修改仓库内容",
        Capability::RepositoryMetadataWrite => "修改仓库元数据（不改变工作区内容）",
        Capability::RepositoryConfigWrite => "修改仓库配置",
        Capability::RepositoryDestroy => "删除仓库或工作区数据",
        Capability::RemoteRead => "读取远端仓库",
        Capability::RemoteMutate => "修改远端仓库",
        Capability::RemoteForce => "强制修改远端仓库",
        Capability::FilesystemRead => "读取文件",
        Capability::FilesystemWrite => "写入文件",
        Capability::FilesystemDelete => "删除文件",
        Capability::BackgroundTaskObserve => "观察后台任务",
        Capability::BackgroundTaskControl => "控制后台任务",
        Capability::ExternalProcessControl => "控制外部进程",
        Capability::CredentialUse => "使用 Git 凭据",
        Capability::CredentialRawRead => "读取凭据原文",
        Capability::CredentialRawWrite => "更换凭据来源",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_projection_preserves_typed_resource_and_human_only_semantics() {
        let mut request = approval_request();
        request.grant = Some(leveler_core::GrantRequest {
            project_identity: "project-a".into(),
            bindings: vec![leveler_core::GrantBinding {
                capability: leveler_core::Capability::FilesystemWrite,
                resource: leveler_core::ResourceIdentity::FilesystemPath {
                    canonical_path: "/fixture/file".into(),
                    object_identity: "object-a".into(),
                },
            }],
        });
        let projected = ui_approval_request(&request);
        assert_eq!(projected.grant, request.grant);
        assert!(projected.project_available());
        assert!(!projected.always_persists);
        request.tool = "save_agent".into();
        let projected = ui_approval_request(&request);
        assert!(projected.requires_human_consent);
        assert!(!projected.project_available());
    }

    /// A prompt with no resolved resource shows the risk level and the paths,
    /// and nothing only the engine can read.
    #[test]
    fn a_plain_command_prompt_names_no_authorization_data() {
        let mut destructive = approval_request();
        destructive.risk = RiskLevel::Destructive;
        destructive.command = None;
        assert_eq!(
            consent_bullets(&destructive),
            vec!["可能造成破坏性变更".to_string()]
        );

        let mut paths = approval_request();
        paths.risk = RiskLevel::Safe;
        paths.paths = vec![std::path::PathBuf::from("src/lib.rs")];
        let bullets = consent_bullets(&paths);
        assert_eq!(bullets, vec!["涉及路径 src/lib.rs".to_string()]);
        assert_no_authorization_data(&bullets);
    }

    /// The git-push prompt: the remote it writes to, the destination a person
    /// can read, and the credential it authenticates with — never the hashes the
    /// bindings match on, and never a serialized resource.
    #[test]
    fn a_remote_write_prompt_states_the_destination_and_the_credential() {
        let mut request = approval_request();
        request.risk = RiskLevel::Network;
        request.command = Some("git push github main".into());
        request.grant = Some(leveler_core::GrantRequest {
            project_identity: "sha256:project-identity".into(),
            bindings: vec![
                leveler_core::GrantBinding {
                    capability: leveler_core::Capability::RemoteMutate,
                    resource: leveler_core::ResourceIdentity::ConfiguredRemote {
                        repository: "sha256:repository-identity".into(),
                        remote_name: "github".into(),
                        canonical_url: "https://github.com/dengmengmian/devorder.git".into(),
                        transport: "https".into(),
                    },
                },
                leveler_core::GrantBinding {
                    capability: leveler_core::Capability::CredentialUse,
                    resource: leveler_core::ResourceIdentity::Credential {
                        project: "sha256:project-identity".into(),
                        host: "github.com".into(),
                        identity: "sha256:credential-incarnation".into(),
                        transport: "https".into(),
                    },
                },
            ],
        });
        let bullets = consent_bullets(&request);
        assert_eq!(
            bullets,
            vec![
                "将访问网络".to_string(),
                "修改远端仓库 github".to_string(),
                "目标 github.com/dengmengmian/devorder".to_string(),
                "使用 github.com 的 Git 凭据".to_string(),
            ]
        );
        assert_no_authorization_data(&bullets);
    }

    /// A repository identity is a hash, so the capability's own wording is the
    /// whole line rather than an identity nobody can decide with.
    #[test]
    fn a_repository_prompt_states_the_capability_without_its_identity() {
        for (capability, expected) in [
            (leveler_core::Capability::RepositoryRead, "读取仓库"),
            (
                leveler_core::Capability::RepositoryMetadataWrite,
                "修改仓库元数据（不改变工作区内容）",
            ),
            (
                leveler_core::Capability::RepositoryDestroy,
                "删除仓库或工作区数据",
            ),
        ] {
            let mut request = approval_request();
            request.risk = RiskLevel::Safe;
            request.grant = Some(leveler_core::GrantRequest {
                project_identity: "sha256:project-identity".into(),
                bindings: vec![leveler_core::GrantBinding {
                    capability,
                    resource: leveler_core::ResourceIdentity::Repository {
                        identity: "sha256:repository-identity".into(),
                    },
                }],
            });
            let bullets = consent_bullets(&request);
            assert_eq!(bullets, vec![expected.to_string()], "{capability:?}");
            assert_no_authorization_data(&bullets);
        }
    }

    #[test]
    fn a_remote_destination_drops_the_scheme_userinfo_and_git_suffix() {
        for (url, expected) in [
            (
                "https://github.com/owner/repo.git",
                Some("github.com/owner/repo"),
            ),
            (
                "git@github.com:owner/repo.git",
                Some("github.com/owner/repo"),
            ),
            (
                "ssh://git@github.com/owner/repo",
                Some("github.com/owner/repo"),
            ),
            // A port belongs to the destination: only the scheme form carries it.
            (
                "https://github.com:8443/owner/repo.git",
                Some("github.com:8443/owner/repo"),
            ),
            ("  ", None),
        ] {
            assert_eq!(remote_target(url).as_deref(), expected, "{url}");
        }
    }

    /// No bullet of a consent prompt may carry an authorization match key: a
    /// project or credential hash, a raw capability name, or a serialized
    /// resource.
    fn assert_no_authorization_data(bullets: &[String]) {
        for bullet in bullets {
            for leaked in ["Project:", "Capability:", "Resource:", "sha256", "{", "_"] {
                assert!(!bullet.contains(leaked), "{leaked} leaked in {bullet}");
            }
        }
    }

    fn approval_request() -> ApprovalRequest {
        ApprovalRequest {
            grant: None,
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
                    grant: None,
                    requires_human_consent: false,
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
        assert_eq!(
            answer.await.unwrap(),
            PendingReply::Decided(ApprovalDecision::ApproveOnce)
        );
        let second = resolve_approval(&pending, &request.id, ApprovalDecision::Deny).unwrap_err();
        assert!(second.to_string().contains("already resolved"));
    }

    #[tokio::test]
    async fn supersede_wakes_only_the_target_session_and_removes_the_waiter() {
        let pending: PendingApprovals = Arc::new(Mutex::new(HashMap::new()));
        let target = SessionId::new("session-a");
        let other = SessionId::new("session-b");
        let request = approval_request();
        let (reply, answer) = oneshot::channel();
        pending.lock().unwrap().insert(
            request.id.clone(),
            PendingApproval {
                binding: PendingBinding::for_approval(target.clone(), &request),
                request: UiApprovalRequest {
                    grant: None,
                    requires_human_consent: false,
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
        // A second waiter on another session must be untouched.
        let mut second_request = approval_request();
        second_request.id = ApprovalId::new("other-approval");
        let (other_reply, _other_answer) = oneshot::channel();
        pending.lock().unwrap().insert(
            second_request.id.clone(),
            PendingApproval {
                binding: PendingBinding::for_approval(other.clone(), &second_request),
                request: UiApprovalRequest {
                    grant: None,
                    requires_human_consent: false,
                    id: second_request.id.clone(),
                    tool: second_request.tool.clone(),
                    summary: second_request.description.clone(),
                    command: second_request.command.clone(),
                    risks: vec![],
                    call_id: Some(second_request.call_id.clone()),
                    always_persists: true,
                },
                reply: other_reply,
            },
        );

        assert_eq!(supersede_pending_approvals(&pending, &target), 1);
        assert_eq!(answer.await.unwrap(), PendingReply::Superseded);
        let remaining = pending.lock().unwrap();
        assert!(
            remaining.contains_key(&second_request.id),
            "another session's question must survive"
        );
        assert!(!remaining.contains_key(&request.id));
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
            questions: Vec::new(),
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
            questions: Vec::new(),
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
            questions: Vec::new(),
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
    async fn only_the_first_clarification_answer_is_accepted() {
        let pending: PendingClarifications = Arc::new(Mutex::new(HashMap::new()));
        let insert = |id: &str| {
            let request = ClarificationRequest {
                id: ClarificationId::new(id),
                turn_id: Some(TurnId::new("turn-a")),
                tool: "ask_user".into(),
                call_id: id.into(),
                action_fingerprint: id.into(),
                question: "which?".into(),
                options: vec![],
                questions: vec![],
            };
            let (reply, answer) = oneshot::channel();
            pending.lock().unwrap().insert(
                request.id.clone(),
                PendingClarification {
                    binding: PendingBinding::for_clarification(
                        SessionId::new("session-a"),
                        &request,
                    ),
                    request: UiClarificationRequest::single(request.id.clone(), "which?", vec![]),
                    reply,
                },
            );
            (request.id, answer)
        };
        let (first_id, first_answer) = insert("clarification-first");
        resolve_clarification(&pending, &first_id, "first choice".into()).unwrap();
        assert_eq!(first_answer.await.unwrap(), "first choice");

        let (next_id, mut next_answer) = insert("clarification-next");
        let duplicate =
            resolve_clarification(&pending, &first_id, "late choice".into()).unwrap_err();
        assert!(duplicate.to_string().contains("already resolved"));
        assert!(matches!(
            next_answer.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        resolve_clarification(&pending, &next_id, "next choice".into()).unwrap();
        assert_eq!(next_answer.await.unwrap(), "next choice");
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
                questions: Vec::new(),
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
