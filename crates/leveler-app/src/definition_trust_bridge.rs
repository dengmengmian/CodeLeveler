//! The runtime side of mid-turn definition authorization.
//!
//! A tool (`load_skill`) reaches this bridge only through the structured
//! [`leveler_tools::DefinitionTrustAuthorizer`] trait. The bridge turns the
//! request into a client event, parks the tool call on a oneshot waiter, and
//! applies the human's structured decision. It never parses model text and
//! never lets a tool write a trust record on its own.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use leveler_client_protocol::{
    DefinitionTrustDecision, RuntimeEvent, UiDefinitionTrustRequest, UiDefinitionTrustState,
};
use leveler_core::{DefinitionTrustRequestId, SessionId};
use leveler_definition_trust::{
    ApprovalRequest, ApprovalSurface, DefinitionTrust, approve, read_skill_package,
};
use leveler_execution::PermissionProfile;
use leveler_tools::definition_trust::{
    DefinitionTrustAuthorizationOutcome, DefinitionTrustAuthorizationRequest,
    DefinitionTrustAuthorizer,
};

use crate::event_bridge::RuntimeEventSender;

/// How often an unwatched request is checked for a vanished client.
#[cfg(not(test))]
fn control_liveness_tick() -> std::time::Duration {
    std::time::Duration::from_secs(10)
}

#[cfg(test)]
fn control_liveness_tick() -> std::time::Duration {
    std::time::Duration::from_millis(10)
}

/// One parked tool call waiting for a human trust decision.
pub(crate) struct PendingDefinitionTrust {
    pub(crate) session_id: SessionId,
    pub(crate) request: UiDefinitionTrustRequest,
    pub(crate) reply: oneshot::Sender<DefinitionTrustDecision>,
}

pub(crate) type PendingDefinitionTrusts =
    Arc<Mutex<HashMap<DefinitionTrustRequestId, PendingDefinitionTrust>>>;

/// Resolve one parked request, refusing an answer from another session.
pub(crate) fn resolve_definition_trust(
    pending: &PendingDefinitionTrusts,
    request_id: &DefinitionTrustRequestId,
    session_id: Option<&SessionId>,
    decision: DefinitionTrustDecision,
) -> Result<(), leveler_client_protocol::ClientError> {
    let entry = {
        let mut map = pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match map.get(request_id) {
            Some(entry) => {
                if let Some(session_id) = session_id
                    && &entry.session_id != session_id
                {
                    return Err(leveler_client_protocol::ClientError::Runtime(format!(
                        "definition-trust request {} belongs to another session",
                        request_id.as_str()
                    )));
                }
                map.remove(request_id)
            }
            None => None,
        }
    };
    let entry = entry.ok_or_else(|| {
        leveler_client_protocol::ClientError::Runtime(
            "pending definition-trust request not found or already resolved".to_string(),
        )
    })?;
    entry.reply.send(decision).map_err(|_| {
        leveler_client_protocol::ClientError::Runtime(
            "pending definition-trust request is no longer waiting for a response".to_string(),
        )
    })
}

/// Removes a waiter and tells every client the prompt is gone, however the
/// waiting future ends (decision, cancellation, disconnect, panic).
struct WaiterGuard {
    pending: PendingDefinitionTrusts,
    events: RuntimeEventSender,
    id: DefinitionTrustRequestId,
}

impl Drop for WaiterGuard {
    fn drop(&mut self) {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.id);
        let _ = self.events.send(RuntimeEvent::DefinitionTrustResolved {
            id: self.id.clone(),
        });
    }
}

/// The result of re-reading and re-recording the approved bytes.
enum ApproveOutcome {
    Approved,
    Changed,
    Failed(String),
}

#[derive(Clone)]
pub(crate) struct ChannelDefinitionTrustAuthorizer {
    pub(crate) app: Arc<crate::Application>,
    pub(crate) events: RuntimeEventSender,
    pub(crate) pending: PendingDefinitionTrusts,
    /// The turn's cancellation token. A cancelled turn releases the waiter
    /// safely; it does not record a denial the person never made.
    pub(crate) cancel: CancellationToken,
    pub(crate) session_id: SessionId,
    /// The mode the turn started under, used only if the live cell has no
    /// answer (a session that has never executed).
    pub(crate) fallback_mode: PermissionProfile,
    /// One definition question at a time. Two parallel tool calls for the same
    /// bytes wait behind one prompt instead of stacking duplicate dialogs.
    pub(crate) serial: Arc<tokio::sync::Mutex<()>>,
}

impl ChannelDefinitionTrustAuthorizer {
    fn live_mode(&self) -> PermissionProfile {
        self.app
            .live_permission_profile(self.session_id.as_str())
            .unwrap_or(self.fallback_mode)
    }

    async fn approve_current(
        &self,
        request: &DefinitionTrustAuthorizationRequest,
    ) -> DefinitionTrustAuthorizationOutcome {
        let Some(workspace) = self
            .app
            .layout
            .primary_workspace()
            .map(|root| root.to_path_buf())
        else {
            return DefinitionTrustAuthorizationOutcome::Failed(
                "no workspace is available to record a project approval".into(),
            );
        };
        let home = self.app.layout.home().root().to_path_buf();
        let identity = request.identity.clone();
        let location = request.location.clone();
        let shown_digest = request.current_digest.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            let package = match read_skill_package(&location.join("SKILL.md")) {
                Ok(package) => package,
                Err(error) => return ApproveOutcome::Failed(error),
            };
            let digest =
                leveler_definition_trust::DefinitionIdentity::skill_package_digest(&package);
            if digest != shown_digest {
                return ApproveOutcome::Changed;
            }
            match approve(
                &home,
                &workspace,
                &[ApprovalRequest::skill_package(identity, &package)],
                ApprovalSurface::Tui,
            ) {
                Ok(_) => ApproveOutcome::Approved,
                Err(error) => ApproveOutcome::Failed(error.to_string()),
            }
        })
        .await;
        let outcome = match outcome {
            Ok(ApproveOutcome::Approved) => {
                let session = self
                    .app
                    .definition_trust_for_session(self.session_id.as_str(), self.live_mode());
                session.allow(
                    &request.identity,
                    &request.current_digest,
                    leveler_definition_trust::SessionTrustSource::TrustCurrent,
                );
                DefinitionTrustAuthorizationOutcome::Trusted
            }
            Ok(ApproveOutcome::Changed) => DefinitionTrustAuthorizationOutcome::ContentChanged,
            Ok(ApproveOutcome::Failed(error)) => {
                // The detail can name host paths; keep it in the local log and
                // give the model a safe, actionable reason.
                tracing::warn!(
                    event = "definition_trust_approval_failed",
                    session = %self.session_id,
                    identity = %request.identity.id,
                    %error,
                );
                DefinitionTrustAuthorizationOutcome::Failed(
                    "the approval transaction failed; see the runtime log".into(),
                )
            }
            Err(error) => {
                tracing::warn!(
                    event = "definition_trust_approval_failed",
                    session = %self.session_id,
                    identity = %request.identity.id,
                    %error,
                );
                DefinitionTrustAuthorizationOutcome::Failed(
                    "the approval worker failed; see the runtime log".into(),
                )
            }
        };
        tracing::info!(
            event = "definition_trust_decision",
            session = %self.session_id,
            identity = %request.identity.id,
            digest = %request.current_digest,
            decision = "trust_current",
            outcome = ?outcome,
        );
        outcome
    }
}

#[async_trait]
impl DefinitionTrustAuthorizer for ChannelDefinitionTrustAuthorizer {
    async fn authorize(
        &self,
        request: DefinitionTrustAuthorizationRequest,
    ) -> DefinitionTrustAuthorizationOutcome {
        let _serial = self.serial.lock().await;
        let mode = self.live_mode();
        // A session this process has not seen yet still gets its own state;
        // an allowance granted now applies to the rest of the session.
        let session = self
            .app
            .definition_trust_for_session(self.session_id.as_str(), mode);
        // A store nobody can verify is not a question for the user: the
        // reload would fail and a persistent approval cannot be written.
        if matches!(request.state, DefinitionTrust::StoreUnavailable { .. }) {
            return DefinitionTrustAuthorizationOutcome::Unavailable;
        }
        // A decision this session already made for these exact bytes answers
        // the request without a second prompt. A different digest is a new
        // question.
        if session
            .overlay(&request.identity, &request.current_digest)
            .is_some_and(|decision| decision.admits())
        {
            return DefinitionTrustAuthorizationOutcome::AllowedOnce;
        }
        if session.is_denied(&request.identity, &request.current_digest) {
            return DefinitionTrustAuthorizationOutcome::Denied;
        }
        // FullAccess adopts only content that predates the session baseline and
        // never prompts for the rest. A changed agent-written definition has no
        // adoption path; `/trust` is the human surface for it.
        if mode == PermissionProfile::FullAccess
            || self.app.live_permission_profile(self.session_id.as_str())
                == Some(PermissionProfile::FullAccess)
        {
            return DefinitionTrustAuthorizationOutcome::Unavailable;
        }

        let id = DefinitionTrustRequestId::generate();
        tracing::info!(
            event = "definition_trust_requested",
            session = %self.session_id,
            request = %id,
            identity = %request.identity.id,
            digest = %request.current_digest,
            call = %request.call_id,
        );
        let ui = UiDefinitionTrustRequest {
            id: id.clone(),
            session_id: self.session_id.clone(),
            call_id: request.call_id.clone(),
            identity: request.identity.id.clone(),
            display: request.identity.display.clone(),
            kind: request.identity.kind.as_str().to_string(),
            scope: request.identity.scope.as_str().to_string(),
            source: request.source.clone(),
            location: request.location.display().to_string(),
            state: ui_state(&request.state),
            approved_digest: request.state.approved_digest().map(str::to_string),
            current_digest: request.current_digest.clone(),
            current_content: request.current_content.clone(),
            approved_content: request.approved_content.clone(),
            content_truncated: request.content_truncated,
        };
        let (tx, mut rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                id.clone(),
                PendingDefinitionTrust {
                    session_id: self.session_id.clone(),
                    request: ui.clone(),
                    reply: tx,
                },
            );
        let _guard = WaiterGuard {
            pending: self.pending.clone(),
            events: self.events.clone(),
            id,
        };
        if self
            .events
            .send(RuntimeEvent::DefinitionTrustRequested { request: ui })
            .is_err()
        {
            return DefinitionTrustAuthorizationOutcome::Unavailable;
        }
        let decision = loop {
            tokio::select! {
                decision = &mut rx => match decision {
                    Ok(decision) => break decision,
                    Err(_) => return DefinitionTrustAuthorizationOutcome::Unavailable,
                },
                _ = self.cancel.cancelled() => {
                    return DefinitionTrustAuthorizationOutcome::Unavailable;
                }
                _ = tokio::time::sleep(control_liveness_tick()) => {
                    // Nobody is attached to answer. A safe refusal is better
                    // than a tool call hanging for a client that may never
                    // come back; a later client re-asks on its next load.
                    if self.events.receiver_count() == 0 {
                        return DefinitionTrustAuthorizationOutcome::Unavailable;
                    }
                }
            }
        };
        match decision {
            DefinitionTrustDecision::Deny => {
                session.deny(&request.identity, &request.current_digest);
                tracing::info!(
                    event = "definition_trust_decision",
                    session = %self.session_id,
                    identity = %request.identity.id,
                    digest = %request.current_digest,
                    decision = "deny",
                );
                DefinitionTrustAuthorizationOutcome::Denied
            }
            DefinitionTrustDecision::AllowOnce => {
                session.allow(
                    &request.identity,
                    &request.current_digest,
                    leveler_definition_trust::SessionTrustSource::AllowOnce,
                );
                tracing::info!(
                    event = "definition_trust_decision",
                    session = %self.session_id,
                    identity = %request.identity.id,
                    digest = %request.current_digest,
                    decision = "allow_once",
                );
                DefinitionTrustAuthorizationOutcome::AllowedOnce
            }
            DefinitionTrustDecision::TrustCurrentVersion => self.approve_current(&request).await,
        }
    }
}

fn ui_state(state: &DefinitionTrust) -> UiDefinitionTrustState {
    match state {
        DefinitionTrust::Unapproved | DefinitionTrust::SessionTrusted { .. } => {
            UiDefinitionTrustState::Unapproved
        }
        DefinitionTrust::Changed { .. } => UiDefinitionTrustState::Changed,
        DefinitionTrust::StoreUnavailable { .. } => UiDefinitionTrustState::StoreUnavailable,
        DefinitionTrust::Trusted { .. }
        | DefinitionTrust::Builtin
        | DefinitionTrust::FullAdopted => UiDefinitionTrustState::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_definition_trust::{DefinitionIdentity, DefinitionKind, DefinitionScope};
    use leveler_project::Layout;

    fn request(root: &std::path::Path) -> DefinitionTrustAuthorizationRequest {
        DefinitionTrustAuthorizationRequest {
            call_id: "call-1".into(),
            identity: DefinitionIdentity::project(DefinitionKind::Skill, ".leveler/skills/demo")
                .unwrap(),
            kind: DefinitionKind::Skill,
            scope: DefinitionScope::Project,
            source: "CodeLeveler".into(),
            location: root.join(".leveler/skills/demo"),
            state: DefinitionTrust::Unapproved,
            current_digest: "sha256:current".into(),
            current_content: "--- SKILL.md ---\nBODY\n".into(),
            approved_content: None,
            content_truncated: false,
        }
    }

    fn authorizer(
        app: Arc<crate::Application>,
        events: tokio::sync::broadcast::Sender<RuntimeEvent>,
        cancel: CancellationToken,
        session: &str,
    ) -> (
        ChannelDefinitionTrustAuthorizer,
        PendingDefinitionTrusts,
        tokio::sync::broadcast::Receiver<RuntimeEvent>,
    ) {
        let pending: PendingDefinitionTrusts = Arc::new(Mutex::new(HashMap::new()));
        let authorizer = ChannelDefinitionTrustAuthorizer {
            app,
            events: events.clone().into(),
            pending: pending.clone(),
            cancel,
            session_id: SessionId::new(session),
            fallback_mode: PermissionProfile::Assisted,
            serial: Arc::new(tokio::sync::Mutex::new(())),
        };
        let receiver = events.subscribe();
        (authorizer, pending, receiver)
    }

    async fn assembled(root: tempfile::TempDir) -> Arc<crate::Application> {
        // A real layout keeps the Leveler home OUTSIDE the workspace; an
        // equal-path layout is refused by the trust store boundary.
        let repo = root.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let layout =
            Layout::from_parts(repo, root.path().join("configs"), root.path().join("state"));
        Arc::new(crate::Application::assemble(layout).unwrap())
    }

    #[tokio::test]
    async fn allow_once_resolves_the_same_tool_call_and_stays_session_scoped() {
        let tmp = tempfile::tempdir().unwrap();
        let app = assembled(tmp).await;
        std::fs::create_dir_all(
            app.layout
                .primary_workspace()
                .unwrap()
                .join(".leveler/skills/demo"),
        )
        .unwrap();
        let (events, _) = tokio::sync::broadcast::channel(16);
        let (authorizer, pending, mut receiver) =
            authorizer(app.clone(), events, CancellationToken::new(), "session-a");
        let request = request(app.layout.primary_workspace().unwrap());
        let decide = tokio::spawn(async move { authorizer.authorize(request).await });

        let event = receiver.recv().await.unwrap();
        let RuntimeEvent::DefinitionTrustRequested { request } = event else {
            panic!("expected a trust request");
        };
        assert_eq!(request.session_id, SessionId::new("session-a"));
        assert_eq!(request.call_id, "call-1");
        assert_eq!(request.state, UiDefinitionTrustState::Unapproved);
        assert!(request.current_content.contains("BODY"));
        resolve_definition_trust(
            &pending,
            &request.id,
            Some(&SessionId::new("session-a")),
            DefinitionTrustDecision::AllowOnce,
        )
        .unwrap();
        let outcome = decide.await.unwrap();
        assert_eq!(outcome, DefinitionTrustAuthorizationOutcome::AllowedOnce);

        // The exact bytes are admitted for this session, and nothing reached
        // the persistent store.
        let session = app.definition_trust_session("session-a").unwrap();
        assert!(
            session
                .overlay(&request_identity(), "sha256:current")
                .is_some()
        );
        assert_eq!(
            leveler_definition_trust::DefinitionTrustStore::load(app.layout.home().root()).status(),
            &leveler_definition_trust::StoreStatus::Missing
        );
    }

    #[tokio::test]
    async fn a_refusal_is_remembered_and_not_asked_twice() {
        let tmp = tempfile::tempdir().unwrap();
        let app = assembled(tmp).await;
        let (events, _) = tokio::sync::broadcast::channel(16);
        let (authorizer, pending, mut receiver) =
            authorizer(app.clone(), events, CancellationToken::new(), "session-a");
        let request = request(app.layout.primary_workspace().unwrap());
        let decide = {
            let request = request.clone();
            let authorizer = authorizer.clone();
            tokio::spawn(async move { authorizer.authorize(request).await })
        };
        let event = receiver.recv().await.unwrap();
        let RuntimeEvent::DefinitionTrustRequested { request: shown } = event else {
            panic!("expected a trust request");
        };
        resolve_definition_trust(&pending, &shown.id, None, DefinitionTrustDecision::Deny).unwrap();
        assert_eq!(
            decide.await.unwrap(),
            DefinitionTrustAuthorizationOutcome::Denied
        );

        // The same bytes are refused without a second prompt. The resolved
        // event from the first decision is already in the channel; only a new
        // Requested would mean the refusal was forgotten.
        let again = authorizer.authorize(request).await;
        assert_eq!(again, DefinitionTrustAuthorizationOutcome::Denied);
        while let Ok(event) = receiver.try_recv() {
            assert!(
                !matches!(event, RuntimeEvent::DefinitionTrustRequested { .. }),
                "a refused digest must not raise another prompt"
            );
        }
    }

    fn request_for_skill(root: &std::path::Path) -> DefinitionTrustAuthorizationRequest {
        let skill_md = root.join(".leveler/skills/demo/SKILL.md");
        let package = leveler_definition_trust::read_skill_package(&skill_md).unwrap();
        let digest = leveler_definition_trust::DefinitionIdentity::skill_package_digest(&package);
        let mut request = request(root);
        request.current_digest = digest;
        request.current_content = "--- SKILL.md ---\nBODY\n".into();
        request
    }

    #[tokio::test]
    async fn trust_current_writes_the_exact_bytes_after_rechecking_them() {
        let tmp = tempfile::tempdir().unwrap();
        let app = assembled(tmp).await;
        let skill = app
            .layout
            .primary_workspace()
            .unwrap()
            .join(".leveler/skills/demo");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: demo\ndescription: Demo.\n---\n\nBODY\n",
        )
        .unwrap();
        let (events, _) = tokio::sync::broadcast::channel(16);
        let (authorizer, pending, mut receiver) =
            authorizer(app.clone(), events, CancellationToken::new(), "session-a");
        let request = request_for_skill(app.layout.primary_workspace().unwrap());
        let shown = request.current_digest.clone();
        let decide = tokio::spawn(async move { authorizer.authorize(request).await });
        let RuntimeEvent::DefinitionTrustRequested { request } = receiver.recv().await.unwrap()
        else {
            panic!("expected a trust request");
        };
        resolve_definition_trust(
            &pending,
            &request.id,
            None,
            DefinitionTrustDecision::TrustCurrentVersion,
        )
        .unwrap();
        assert_eq!(
            decide.await.unwrap(),
            DefinitionTrustAuthorizationOutcome::Trusted
        );
        let view = leveler_definition_trust::DefinitionTrustView::load(
            app.layout.home().root(),
            app.layout.primary_workspace(),
        );
        assert!(view.check(&request_identity(), &shown).is_trusted());
        let session = app.definition_trust_session("session-a").unwrap();
        assert!(session.overlay(&request_identity(), &shown).is_some());
    }

    #[tokio::test]
    async fn trust_current_refuses_when_the_file_moved_before_the_write() {
        let tmp = tempfile::tempdir().unwrap();
        let app = assembled(tmp).await;
        let skill = app
            .layout
            .primary_workspace()
            .unwrap()
            .join(".leveler/skills/demo");
        std::fs::create_dir_all(&skill).unwrap();
        let skill_md = skill.join("SKILL.md");
        std::fs::write(
            &skill_md,
            "---\nname: demo\ndescription: Demo.\n---\n\nBODY\n",
        )
        .unwrap();
        let (events, _) = tokio::sync::broadcast::channel(16);
        let (authorizer, pending, mut receiver) =
            authorizer(app.clone(), events, CancellationToken::new(), "session-a");
        let request = request_for_skill(app.layout.primary_workspace().unwrap());
        let shown = request.current_digest.clone();
        let decide = tokio::spawn(async move { authorizer.authorize(request).await });
        let RuntimeEvent::DefinitionTrustRequested { request } = receiver.recv().await.unwrap()
        else {
            panic!("expected a trust request");
        };
        std::fs::write(
            &skill_md,
            "---\nname: demo\ndescription: Demo.\n---\n\nCHANGED\n",
        )
        .unwrap();
        resolve_definition_trust(
            &pending,
            &request.id,
            None,
            DefinitionTrustDecision::TrustCurrentVersion,
        )
        .unwrap();
        assert_eq!(
            decide.await.unwrap(),
            DefinitionTrustAuthorizationOutcome::ContentChanged
        );
        let view = leveler_definition_trust::DefinitionTrustView::load(
            app.layout.home().root(),
            app.layout.primary_workspace(),
        );
        assert!(!view.check(&request_identity(), &shown).is_trusted());
        assert_eq!(
            leveler_definition_trust::DefinitionTrustStore::load(app.layout.home().root()).status(),
            &leveler_definition_trust::StoreStatus::Missing
        );
    }

    fn request_identity() -> DefinitionIdentity {
        DefinitionIdentity::project(DefinitionKind::Skill, ".leveler/skills/demo").unwrap()
    }

    #[tokio::test]
    async fn an_unattended_or_cancelled_request_refuses_instead_of_hanging() {
        let tmp = tempfile::tempdir().unwrap();
        let app = assembled(tmp).await;
        let (events, receiver) = tokio::sync::broadcast::channel(16);
        drop(receiver);
        let (unwatched, _pending, receiver) =
            authorizer(app.clone(), events, CancellationToken::new(), "session-a");
        drop(receiver);
        let outcome = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            unwatched.authorize(request(app.layout.primary_workspace().unwrap())),
        )
        .await
        .expect("an unwatched request resolves");
        assert_eq!(outcome, DefinitionTrustAuthorizationOutcome::Unavailable);

        let (events, _keep) = tokio::sync::broadcast::channel(16);
        let cancel = CancellationToken::new();
        let (authorizer, _pending, receiver) = authorizer(app, events, cancel.clone(), "session-a");
        drop(receiver);
        let wait = tokio::spawn(async move {
            authorizer
                .authorize(request(std::path::Path::new("/tmp")))
                .await
        });
        cancel.cancel();
        let outcome = tokio::time::timeout(std::time::Duration::from_millis(500), wait)
            .await
            .expect("a cancelled request resolves")
            .unwrap();
        assert_eq!(outcome, DefinitionTrustAuthorizationOutcome::Unavailable);
    }

    #[test]
    fn a_state_mapping_never_calls_a_session_allowance_persistent() {
        assert_eq!(
            ui_state(&DefinitionTrust::SessionTrusted {
                source: leveler_definition_trust::SessionTrustSource::AllowOnce
            }),
            UiDefinitionTrustState::Unapproved
        );
        assert_eq!(
            ui_state(&DefinitionTrust::Changed {
                approved_digest: "sha256:old".into()
            }),
            UiDefinitionTrustState::Changed
        );
    }
}
