//! Live (non-transcript) session view: what a reconnecting client must see
//! beyond the message history — running tools, the active plan, verification,
//! diff, and the completion report.
//!
//! One owner for two jobs that must agree: folding the outbound RuntimeEvent
//! stream into per-session state (`apply`), and answering the reconnect
//! snapshot (`view`). The fold is a pure `(state, event) → state` reducer, so
//! it is testable without a runtime.

use std::collections::HashMap;
use std::sync::Mutex;

use leveler_client_protocol::{
    RuntimeEvent, UiActiveToolCall, UiCompletionReport, UiDiff, UiPlan, UiVerification,
};
use leveler_core::SessionId;

#[derive(Debug, Clone, Default)]
pub(crate) struct LiveSessionView {
    pub active_tools: Vec<UiActiveToolCall>,
    pub plan: Option<UiPlan>,
    pub verification: Option<UiVerification>,
    pub diff: Option<UiDiff>,
    pub completion_report: Option<UiCompletionReport>,
}

/// Per-session live views, shared via `Arc` with the event forwarder tasks.
#[derive(Default)]
pub(crate) struct LiveViews {
    views: Mutex<HashMap<SessionId, LiveSessionView>>,
}

impl LiveViews {
    /// Fold one outbound event into the session's live view.
    pub fn apply(&self, session_id: &SessionId, event: &RuntimeEvent) {
        let mut views = self.views.lock().unwrap();
        let view = views.entry(session_id.clone()).or_default();
        fold(view, event);
    }

    /// The session's current live view (for the reconnect snapshot).
    pub fn view(&self, session_id: &SessionId) -> LiveSessionView {
        self.views
            .lock()
            .unwrap()
            .get(session_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Forget a session's live view (conversation cleared).
    pub fn clear(&self, session_id: &SessionId) {
        self.views.lock().unwrap().remove(session_id);
    }
}

/// The pure reducer: what each client-visible event means for the live view.
fn fold(view: &mut LiveSessionView, event: &RuntimeEvent) {
    match event {
        RuntimeEvent::ToolCallStarted {
            id,
            name,
            arguments,
            ..
        } => {
            view.active_tools.retain(|tool| tool.id != *id);
            view.active_tools.push(UiActiveToolCall {
                id: id.clone(),
                name: name.clone(),
                arguments: arguments.clone(),
            });
        }
        RuntimeEvent::ToolCallCompleted { id, .. } => {
            view.active_tools.retain(|tool| tool.id != *id);
        }
        RuntimeEvent::PlanUpdated { plan } => view.plan = Some(plan.clone()),
        RuntimeEvent::VerificationUpdated { verification } => {
            view.verification = Some(verification.clone());
        }
        RuntimeEvent::DiffUpdated { diff } => view.diff = Some(diff.clone()),
        RuntimeEvent::SessionCompleted { report } => {
            view.completion_report = Some(report.clone());
        }
        RuntimeEvent::UserMessageAdded { .. } => {
            view.completion_report = None;
        }
        RuntimeEvent::TurnCompleted
        | RuntimeEvent::TurnAnswered
        | RuntimeEvent::TurnTruncated { .. }
        | RuntimeEvent::TurnIncomplete { .. }
        | RuntimeEvent::TurnCompletedUnverified { .. }
        | RuntimeEvent::TurnFailed { .. }
        | RuntimeEvent::TurnCancelled => view.active_tools.clear(),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_core::ToolCallId;

    #[test]
    fn live_view_tracks_only_tools_that_are_still_running() {
        let session_id = SessionId::new("s1");
        let views = LiveViews::default();
        let id = ToolCallId::new("tool-1");

        views.apply(
            &session_id,
            &RuntimeEvent::ToolCallStarted {
                id: id.clone(),
                name: "run_command".to_string(),
                arguments: r#"{"cmd":"cargo test"}"#.to_string(),
                parallel: false,
            },
        );
        assert_eq!(views.view(&session_id).active_tools.len(), 1);

        views.apply(
            &session_id,
            &RuntimeEvent::ToolCallCompleted {
                id,
                ok: true,
                preview: "ok".to_string(),
                duration_ms: 1,
            },
        );
        assert!(views.view(&session_id).active_tools.is_empty());
    }
}
