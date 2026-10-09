//! Live (non-transcript) session view: what a reconnecting client must see
//! beyond the message history — running tools, the active plan, diff, and the
//! completion report.
//!
//! One owner for two jobs that must agree: folding the outbound RuntimeEvent
//! stream into per-session state (`apply`), and answering the reconnect
//! snapshot (`view`). The fold is a pure `(state, event) → state` reducer, so
//! it is testable without a runtime.

use std::collections::HashMap;
use std::sync::Mutex;

use leveler_client_protocol::{
    FinalizationStage, RuntimeEvent, UiActiveToolCall, UiCompletionReport, UiDiff, UiPlan,
};
use leveler_core::SessionId;

#[derive(Debug, Clone, Default)]
pub(crate) struct LiveSessionView {
    pub active_tools: Vec<UiActiveToolCall>,
    pub plan: Option<UiPlan>,
    pub diff: Option<UiDiff>,
    pub completion_report: Option<UiCompletionReport>,
    pub finalization_stage: Option<FinalizationStage>,
    /// The latest context accounting the kernel computed before sending its
    /// next request. `None` before the first model request of a session.
    pub context_usage: Option<leveler_model::ContextAccounting>,
    /// When each active tool started, by this runtime's clock.
    tool_started: HashMap<leveler_core::ToolCallId, std::time::Instant>,
}

/// Bounded end of a running command's output kept for the reconnect snapshot.
pub(crate) const TOOL_OUTPUT_TAIL_CAP: usize = 64 * 1024;

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

    /// The session's current live view (for the reconnect snapshot). A
    /// running tool's `elapsed_ms` is measured now, at snapshot time.
    pub fn view(&self, session_id: &SessionId) -> LiveSessionView {
        let mut view = self
            .views
            .lock()
            .unwrap()
            .get(session_id)
            .cloned()
            .unwrap_or_default();
        for tool in &mut view.active_tools {
            if let Some(started) = view.tool_started.get(&tool.id) {
                tool.elapsed_ms = started.elapsed().as_millis() as u64;
            }
        }
        view
    }

    /// Forget a session's live view (conversation cleared).
    pub fn clear(&self, session_id: &SessionId) {
        self.views.lock().unwrap().remove(session_id);
    }

    /// Forget only the last context accounting for a session, keeping the rest
    /// of its live view.
    ///
    /// The accounting describes ONE assembled request against the policy in
    /// force when it was published. When that stops being the request the
    /// runtime would send next — the model changed, so the resolved policy
    /// (window, reservation, soft threshold) changed, or the active context was
    /// replaced by a compaction — the old numbers must not be served as
    /// current. The next assembled request republishes them; meanwhile a client
    /// shows nothing rather than a denominator measured for a different policy.
    pub fn clear_context(&self, session_id: &SessionId) {
        if let Some(view) = self.views.lock().unwrap().get_mut(session_id) {
            view.context_usage = None;
        }
    }
}

/// The pure reducer: what each client-visible event means for the live view.
fn fold(view: &mut LiveSessionView, event: &RuntimeEvent) {
    match event {
        RuntimeEvent::ToolCallStarted {
            id,
            name,
            arguments,
            model_step,
            answer_effect,
            ..
        } => {
            view.active_tools.retain(|tool| tool.id != *id);
            view.tool_started
                .insert(id.clone(), std::time::Instant::now());
            view.active_tools.push(UiActiveToolCall {
                id: id.clone(),
                name: name.clone(),
                arguments: arguments.clone(),
                elapsed_ms: 0,
                output_tail: String::new(),
                output_truncated: false,
                // The live view carries the round so a reconnect restores the
                // SAME rounds, not a re-derived approximation.
                model_step: *model_step,
                // And it carries the answer classification for the same
                // reason: a reconnected client must not re-decide the
                // FinalAnswer boundary from the tool name.
                answer_effect: *answer_effect,
            });
        }
        RuntimeEvent::ToolCallCompleted { id, .. } => {
            view.active_tools.retain(|tool| tool.id != *id);
            view.tool_started.remove(id);
        }
        RuntimeEvent::ToolCallOutput { id, chunk, .. } => {
            if let Some(tool) = view.active_tools.iter_mut().find(|tool| tool.id == *id) {
                tool.output_tail.push_str(chunk);
                if tool.output_tail.len() > TOOL_OUTPUT_TAIL_CAP {
                    let cut = tool.output_tail.len() - TOOL_OUTPUT_TAIL_CAP;
                    let cut = leveler_core::ceil_char_boundary(&tool.output_tail, cut);
                    tool.output_tail.drain(..cut);
                    tool.output_truncated = true;
                }
            }
        }
        RuntimeEvent::PlanUpdated { plan } => view.plan = Some(plan.clone()),
        RuntimeEvent::ContextUsage { accounting } => {
            view.context_usage = Some(accounting.clone());
        }
        RuntimeEvent::DiffUpdated { diff, .. } => view.diff = Some(diff.clone()),
        RuntimeEvent::SessionCompleted { report } => {
            view.completion_report = Some(report.clone());
        }
        RuntimeEvent::UserMessageAdded { .. } => {
            view.completion_report = None;
            view.finalization_stage = None;
        }
        RuntimeEvent::TurnFinalizing { stage } => view.finalization_stage = Some(*stage),
        RuntimeEvent::TurnCompleted
        | RuntimeEvent::TurnCompletedWithWarnings { .. }
        | RuntimeEvent::TurnAnswered
        | RuntimeEvent::TurnTruncated { .. }
        | RuntimeEvent::TurnIncomplete { .. }
        | RuntimeEvent::TurnFailed { .. }
        | RuntimeEvent::TurnCancelled
        | RuntimeEvent::TaskCancelled => {
            view.active_tools.clear();
            view.tool_started.clear();
            view.plan = None;
            view.finalization_stage = None;
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_core::ToolCallId;

    fn plan() -> UiPlan {
        UiPlan {
            steps: vec![leveler_client_protocol::UiPlanStep {
                index: 0,
                description: "inspect lifecycle".to_string(),
                status: leveler_client_protocol::PlanStepStatus::Running,
            }],
        }
    }

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
                model_step: None,
                answer_effect: None,
            },
        );
        assert_eq!(views.view(&session_id).active_tools.len(), 1);

        views.apply(
            &session_id,
            &RuntimeEvent::ToolCallCompleted {
                exit_code: None,
                stop: None,
                id,
                ok: true,
                preview: "ok".to_string(),
                duration_ms: 1,
                applied_diff: None,
            },
        );
        assert!(views.view(&session_id).active_tools.is_empty());
    }

    /// The snapshot states the execution round the runtime already knows, so a
    /// reconnect does not have to re-derive one from the tools' kinds.
    #[test]
    fn a_running_calls_round_reaches_the_reconnect_snapshot() {
        let session_id = SessionId::new("s1");
        let views = LiveViews::default();
        for (id, name) in [("a", "read_file"), ("b", "grep")] {
            views.apply(
                &session_id,
                &RuntimeEvent::ToolCallStarted {
                    id: ToolCallId::new(id),
                    name: name.to_string(),
                    arguments: "{}".to_string(),
                    parallel: true,
                    model_step: Some(4),
                    answer_effect: None,
                },
            );
        }
        // A call from an older round is not retro-labelled by the newer one.
        views.apply(
            &session_id,
            &RuntimeEvent::ToolCallStarted {
                id: ToolCallId::new("c"),
                name: "shell_command".to_string(),
                arguments: "{}".to_string(),
                parallel: false,
                model_step: None,
                answer_effect: None,
            },
        );

        let tools = views.view(&session_id).active_tools;
        assert_eq!(
            tools.iter().filter(|t| t.model_step == Some(4)).count(),
            2,
            "{tools:?}"
        );
        assert_eq!(
            tools
                .iter()
                .find(|t| t.id.as_str() == "c")
                .unwrap()
                .model_step,
            None,
            "an unknown round stays unknown: {tools:?}"
        );
    }

    /// A reconnecting client must not restart a long command's clock at zero
    /// or lose what it already printed.
    #[test]
    fn a_running_command_keeps_its_elapsed_and_output_tail_for_reconnect() {
        let session_id = SessionId::new("s1");
        let views = LiveViews::default();
        let id = ToolCallId::new("tool-1");
        views.apply(
            &session_id,
            &RuntimeEvent::ToolCallStarted {
                id: id.clone(),
                name: "shell_command".to_string(),
                arguments: r#"{"cmd":"cargo test"}"#.to_string(),
                parallel: false,
                model_step: None,
                answer_effect: None,
            },
        );
        views.apply(
            &session_id,
            &RuntimeEvent::ToolCallOutput {
                id: id.clone(),
                stream: "stdout".to_string(),
                chunk: "Compiling leveler-core\n".to_string(),
            },
        );
        std::thread::sleep(std::time::Duration::from_millis(30));
        let tool = views.view(&session_id).active_tools.remove(0);
        assert!(tool.elapsed_ms >= 30, "{}", tool.elapsed_ms);
        assert_eq!(tool.output_tail, "Compiling leveler-core\n");
        assert!(!tool.output_truncated);

        let huge = "x".repeat(TOOL_OUTPUT_TAIL_CAP + 10);
        views.apply(
            &session_id,
            &RuntimeEvent::ToolCallOutput {
                id,
                stream: "stdout".to_string(),
                chunk: huge,
            },
        );
        let tool = views.view(&session_id).active_tools.remove(0);
        assert_eq!(tool.output_tail.len(), TOOL_OUTPUT_TAIL_CAP);
        assert!(tool.output_truncated);
    }

    #[test]
    fn live_view_preserves_finalization_for_reconnect_and_clears_at_terminal() {
        let session_id = SessionId::new("s1");
        let views = LiveViews::default();

        views.apply(
            &session_id,
            &RuntimeEvent::TurnFinalizing {
                stage: FinalizationStage::Review,
            },
        );
        assert_eq!(
            views.view(&session_id).finalization_stage,
            Some(FinalizationStage::Review)
        );

        views.apply(&session_id, &RuntimeEvent::TurnCompleted);
        assert_eq!(views.view(&session_id).finalization_stage, None);
    }

    #[test]
    fn every_turn_terminal_removes_the_plan_from_the_live_view() {
        let terminal_events = [
            RuntimeEvent::TurnCompleted,
            RuntimeEvent::TurnCompletedWithWarnings {
                reason: "warning".to_string(),
            },
            RuntimeEvent::TurnAnswered,
            RuntimeEvent::TurnTruncated {
                error: "truncated".to_string(),
            },
            RuntimeEvent::TurnIncomplete {
                reason: "incomplete".to_string(),
            },
            RuntimeEvent::TurnFailed {
                error: "failed".to_string(),
                failure: None,
            },
            RuntimeEvent::TurnCancelled,
        ];

        for terminal in terminal_events {
            let session_id = SessionId::new("s1");
            let views = LiveViews::default();
            views.apply(&session_id, &RuntimeEvent::PlanUpdated { plan: plan() });
            assert!(views.view(&session_id).plan.is_some());

            views.apply(&session_id, &terminal);

            assert!(
                views.view(&session_id).plan.is_none(),
                "terminal event left an active plan behind: {terminal:?}"
            );
        }
    }
}

#[cfg(test)]
mod task_cancel_cleanup_tests {
    use super::*;
    #[test]
    fn committed_task_cancellation_clears_running_chrome_for_reconnect() {
        let views = LiveViews::default();
        let session = SessionId::new("cancelled");
        views.apply(
            &session,
            &RuntimeEvent::ToolCallStarted {
                id: leveler_core::ToolCallId::new("call"),
                name: "run_command".into(),
                arguments: "{}".into(),
                parallel: false,
                model_step: None,
                answer_effect: None,
            },
        );
        views.apply(
            &session,
            &RuntimeEvent::PlanUpdated {
                plan: UiPlan { steps: Vec::new() },
            },
        );
        views.apply(
            &session,
            &RuntimeEvent::TurnFinalizing {
                stage: FinalizationStage::ResolvingOutcome,
            },
        );
        views.apply(&session, &RuntimeEvent::TaskCancelled);
        let restored = views.view(&session);
        assert!(restored.active_tools.is_empty());
        assert!(restored.plan.is_none());
        assert!(restored.finalization_stage.is_none());
    }

    /// Dropping the context accounting is surgical: the rest of the live view
    /// (a running tool, the plan) survives, because only the numbers measured
    /// for a policy that no longer applies are invalid.
    #[test]
    fn clear_context_keeps_the_rest_of_the_live_view() {
        let session_id = SessionId::new("s1");
        let views = LiveViews::default();
        let projection = leveler_model::RequestProjection::project(
            &[],
            &[],
            leveler_model::ReasoningReplayContract::NONE,
            leveler_model::ReasoningRetention::default(),
        );
        let accounting = leveler_model::ContextAccounting::compute(
            leveler_model::ModelRef::new("mock", "m"),
            &projection,
            None,
            None,
            None,
        );
        views.apply(
            &session_id,
            &RuntimeEvent::PlanUpdated {
                plan: UiPlan { steps: Vec::new() },
            },
        );
        views.apply(&session_id, &RuntimeEvent::ContextUsage { accounting });
        assert!(views.view(&session_id).context_usage.is_some());

        views.clear_context(&session_id);
        let view = views.view(&session_id);
        assert!(view.context_usage.is_none());
        assert!(
            view.plan.is_some(),
            "the plan is not the context's to clear"
        );

        // Clearing a session with no live view is a no-op, not a panic.
        views.clear_context(&SessionId::new("never-seen"));
    }
}
