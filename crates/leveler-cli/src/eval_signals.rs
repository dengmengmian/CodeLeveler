//! Trajectory-signal collection for eval failure attribution (L1 taskset doc
//! §8). The collector folds the agent/engine event stream into the pure
//! [`TrajectorySignals`] that `leveler_eval::classify_failure` consumes — it
//! observes facts (tool names, error markers, node outcomes), never model text
//! semantics.
//!
//! Also measures **TTFF** (time to first user-visible feedback) and **max silent
//! gap** between feedback events from the same stream, using wall-clock
//! timestamps at observation time.

use std::collections::HashSet;
use std::time::Instant;

use leveler_agent::AgentEvent;
use leveler_engine::EngineEvent;
use leveler_eval::TrajectorySignals;

/// Programs whose successful run counts as verification evidence. Mirrors the
/// executor's verification-class heuristic; kept local so the harness has no
/// dependency on executor internals.
const VERIFICATION_PROGRAMS: &[&str] = &[
    "cargo", "rustc", "go", "npm", "pnpm", "yarn", "npx", "bun", "deno", "node", "tsc", "jest",
    "vitest", "mocha", "pytest", "python", "python3", "tox", "mypy", "ruff", "make", "just",
    "gradle", "gradlew", "mvn", "mvnw", "dotnet", "ctest", "cmake", "swift", "zig",
];

/// The marker the executor's no-progress loop guard puts in a blocked call's
/// tool result.
const LOOP_GUARD_MARKER: &str = "made no progress";

/// The marker an authorization denial puts in a tool result.
const DENIAL_MARKER: &str = "action not permitted";

pub(crate) struct SignalCollector {
    signals: TrajectorySignals,
    /// Case-relevant paths (the overlay's files): the harness's proxy for
    /// "where the defect/acceptance lives".
    relevant: Vec<String>,
    /// Tool-call ids of verification-class `run_command`s, to mark
    /// `verification_ran` when one succeeds.
    verify_calls: HashSet<String>,
    /// (tool name, current consecutive-error count) for the arg-error streak.
    error_streak: Option<(String, u32)>,
    /// Case start for TTFF measurement.
    started: Instant,
    /// Instant of the first user-visible feedback event, if any.
    first_feedback: Option<Instant>,
    /// Instant of the most recent feedback event (for silent-gap tracking).
    last_feedback: Option<Instant>,
    /// Longest gap (ms) between consecutive feedback events.
    max_silent_ms: u64,
    /// Count of feedback events observed (need ≥2 for a silent gap).
    feedback_events: u32,
}

impl SignalCollector {
    pub(crate) fn new(relevant_paths: impl IntoIterator<Item = String>) -> Self {
        Self {
            signals: TrajectorySignals::default(),
            relevant: relevant_paths.into_iter().collect(),
            verify_calls: HashSet::new(),
            error_streak: None,
            started: Instant::now(),
            first_feedback: None,
            last_feedback: None,
            max_silent_ms: 0,
            feedback_events: 0,
        }
    }

    /// Record a user-visible progress/feedback signal for TTFF / silent gap.
    fn note_feedback(&mut self) {
        let now = Instant::now();
        if self.first_feedback.is_none() {
            self.first_feedback = Some(now);
            let ttff = now
                .duration_since(self.started)
                .as_millis()
                .min(u128::from(u64::MAX)) as u64;
            self.signals.ttff_ms = Some(ttff);
        }
        if let Some(prev) = self.last_feedback {
            let gap = now
                .duration_since(prev)
                .as_millis()
                .min(u128::from(u64::MAX)) as u64;
            self.max_silent_ms = self.max_silent_ms.max(gap);
        }
        self.last_feedback = Some(now);
        self.feedback_events = self.feedback_events.saturating_add(1);
        if self.feedback_events >= 2 {
            self.signals.max_silent_ms = Some(self.max_silent_ms);
        }
    }

    pub(crate) fn observe_agent(&mut self, event: &AgentEvent) {
        match event {
            // User-visible feedback: status/wait labels, streaming text,
            // reasoning, tools, plan, verification, command heartbeats.
            // StreamAttemptStarted / AdvisoryStarted name the wait so TTFF is
            // not stuck behind the first model token (often tens of seconds).
            AgentEvent::StreamAttemptStarted
            | AgentEvent::AdvisoryStarted { .. }
            | AgentEvent::AssistantDelta(_)
            | AgentEvent::ReasoningDelta(_)
            | AgentEvent::AssistantText(_)
            | AgentEvent::ToolCall { .. }
            | AgentEvent::ToolResult { .. }
            | AgentEvent::PlanUpdated { .. }
            | AgentEvent::VerificationStarted
            | AgentEvent::VerificationCheck { .. }
            | AgentEvent::VerificationFinished { .. }
            | AgentEvent::CommandProgress { .. }
            | AgentEvent::SubAgentStarted { .. }
            | AgentEvent::SubAgentActivity { .. }
            | AgentEvent::SubAgentFinished { .. }
            | AgentEvent::ProgressUpdated { .. } => {
                self.note_feedback();
            }
            _ => {}
        }

        match event {
            AgentEvent::ToolCall {
                id,
                name,
                arguments,
                ..
            } => {
                self.signals.tool_calls += 1;
                if !self.signals.touched_relevant_files
                    && self.relevant.iter().any(|p| arguments.contains(p.as_str()))
                {
                    self.signals.touched_relevant_files = true;
                }
                if name == "run_command" && is_verification_program(arguments) {
                    self.verify_calls.insert(id.clone());
                }
            }
            AgentEvent::ToolResult {
                id,
                name,
                is_error,
                preview,
            } => {
                if matches!(name.as_str(), "apply_patch" | "replace") {
                    self.signals.edit_attempts += 1;
                    if *is_error {
                        self.signals.edit_failures += 1;
                    }
                }
                if *is_error {
                    if preview.contains(LOOP_GUARD_MARKER) {
                        self.signals.loop_guard_trips += 1;
                    }
                    if preview.contains(DENIAL_MARKER) {
                        self.signals.env_failure = true;
                    }
                    let streak = match self.error_streak.take() {
                        Some((tool, n)) if tool == *name => n + 1,
                        _ => 1,
                    };
                    self.signals.arg_error_streak = self.signals.arg_error_streak.max(streak);
                    self.error_streak = Some((name.clone(), streak));
                } else {
                    self.error_streak = None;
                    if self.verify_calls.contains(id) {
                        self.signals.verification_ran = true;
                    }
                }
            }
            AgentEvent::Compacted { .. } => self.signals.compactions += 1,
            AgentEvent::VerificationStarted => self.signals.verification_ran = true,
            _ => {}
        }
    }

    /// Orchestrated runs emit engine events; node outcomes feed the planning
    /// signal and everything else reuses the agent-event logic via the shim.
    pub(crate) fn observe_engine(&mut self, event: EngineEvent) {
        if let EngineEvent::NodeFinished { status, .. } = &event {
            self.signals.node_total += 1;
            if *status == leveler_orchestrator::NodeStatus::Failed {
                self.signals.node_failures += 1;
            }
            // Node lifecycle is user-visible progress in the plan UI.
            self.note_feedback();
            return;
        }
        // TaskStarted / phase / plan lifecycle / command heartbeats are the
        // earliest host-side signals — must count toward TTFF so the metric
        // reflects user-visible progress, not first LLM token alone.
        if matches!(
            &event,
            EngineEvent::TaskStarted { .. }
                | EngineEvent::NodeStarted { .. }
                | EngineEvent::PhaseChanged { .. }
                | EngineEvent::RequirementReady { .. }
                | EngineEvent::ContextReady { .. }
                | EngineEvent::PlanReady { .. }
                | EngineEvent::CommandProgress { .. }
                | EngineEvent::StreamAttemptStarted
                | EngineEvent::AdvisoryStarted { .. }
        ) {
            self.note_feedback();
        }
        if let Some(agent_event) = leveler_app::engine_event_to_agent(event) {
            self.observe_agent(&agent_event);
        }
    }

    /// Final signals; `context_overflow` comes from the run outcome (budget /
    /// context ceiling), which the event stream itself does not carry.
    pub(crate) fn finish(mut self, context_overflow: bool) -> TrajectorySignals {
        self.signals.context_overflow = context_overflow;
        self.signals
    }
}

/// Whether a `run_command`'s JSON arguments name a verification-class program.
fn is_verification_program(arguments: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(arguments) else {
        return false;
    };
    let Some(program) = value.get("program").and_then(|v| v.as_str()) else {
        return false;
    };
    let base = std::path::Path::new(program)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(program);
    VERIFICATION_PROGRAMS.contains(&base)
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_eval::{FailureCategory, classify_failure};
    use std::thread;
    use std::time::Duration;

    fn call(id: &str, name: &str, args: serde_json::Value) -> AgentEvent {
        AgentEvent::ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: args.to_string(),
            parallel: false,
        }
    }

    fn result(id: &str, name: &str, is_error: bool, preview: &str) -> AgentEvent {
        AgentEvent::ToolResult {
            id: id.into(),
            name: name.into(),
            is_error,
            preview: preview.into(),
        }
    }

    /// A failed run that grepped around but never opened the overlay's files
    /// and never edited must classify as a localization failure.
    #[test]
    fn read_only_run_that_missed_the_relevant_files_is_localization() {
        let mut c = SignalCollector::new(vec!["src/parser.rs".to_string()]);
        c.observe_agent(&call("c1", "grep", serde_json::json!({"pattern": "foo"})));
        c.observe_agent(&result("c1", "grep", false, "3 matches"));
        c.observe_agent(&call(
            "c2",
            "read_file",
            serde_json::json!({"path": "src/other.rs"}),
        ));
        c.observe_agent(&result("c2", "read_file", false, "…"));

        let s = c.finish(false);
        assert_eq!(s.tool_calls, 2);
        assert!(!s.touched_relevant_files);
        assert_eq!(classify_failure(&s), FailureCategory::Localization);
    }

    /// Reading a relevant file and failing most edits is an editing failure.
    #[test]
    fn majority_failed_edits_classify_as_editing() {
        let mut c = SignalCollector::new(vec!["src/parser.rs".to_string()]);
        c.observe_agent(&call(
            "c1",
            "read_file",
            serde_json::json!({"path": "src/parser.rs"}),
        ));
        c.observe_agent(&result("c1", "read_file", false, "…"));
        c.observe_agent(&call(
            "c2",
            "apply_patch",
            serde_json::json!({"path": "src/parser.rs"}),
        ));
        c.observe_agent(&result("c2", "apply_patch", true, "patch failed"));
        c.observe_agent(&call(
            "c3",
            "apply_patch",
            serde_json::json!({"path": "src/parser.rs"}),
        ));
        c.observe_agent(&result("c3", "apply_patch", true, "patch failed"));
        c.observe_agent(&call(
            "c4",
            "apply_patch",
            serde_json::json!({"path": "src/parser.rs"}),
        ));
        c.observe_agent(&result("c4", "apply_patch", false, "ok"));

        let s = c.finish(false);
        assert!(s.touched_relevant_files);
        assert_eq!(s.edit_attempts, 3);
        assert_eq!(s.edit_failures, 2);
        assert_eq!(classify_failure(&s), FailureCategory::Editing);
    }

    #[test]
    fn loop_guard_trips_classify_as_tooling() {
        let mut c = SignalCollector::new(Vec::new());
        c.observe_agent(&call(
            "c1",
            "read_file",
            serde_json::json!({"path": "a.rs"}),
        ));
        c.observe_agent(&result(
            "c1",
            "read_file",
            true,
            "blocked: made no progress on this call",
        ));
        let s = c.finish(false);
        assert!(s.loop_guard_trips >= 1);
        assert_eq!(classify_failure(&s), FailureCategory::Tooling);
    }

    #[test]
    fn only_verification_class_commands_mark_verification_ran() {
        let mut c = SignalCollector::new(Vec::new());
        c.observe_agent(&call(
            "c1",
            "run_command",
            serde_json::json!({"program": "ls", "args": ["-la"]}),
        ));
        c.observe_agent(&result("c1", "run_command", false, "ok"));
        let s = c.finish(false);
        assert!(!s.verification_ran);

        let mut c = SignalCollector::new(Vec::new());
        c.observe_agent(&call(
            "c1",
            "run_command",
            serde_json::json!({"program": "cargo", "args": ["test"]}),
        ));
        c.observe_agent(&result("c1", "run_command", false, "ok"));
        let s = c.finish(false);
        assert!(s.verification_ran);
    }

    #[test]
    fn ttff_records_time_to_first_feedback_event() {
        let mut c = SignalCollector::new(Vec::new());
        thread::sleep(Duration::from_millis(15));
        c.observe_agent(&AgentEvent::AssistantDelta("hi".into()));
        let s = c.finish(false);
        let ttff = s.ttff_ms.expect("first feedback must set TTFF");
        assert!(
            ttff >= 10,
            "TTFF should reflect wall time before first event, got {ttff}ms"
        );
        // Single feedback event → no silent gap yet.
        assert!(s.max_silent_ms.is_none());
    }

    #[test]
    fn silent_duration_is_max_gap_between_feedback_events() {
        let mut c = SignalCollector::new(Vec::new());
        c.observe_agent(&AgentEvent::AssistantDelta("a".into()));
        thread::sleep(Duration::from_millis(25));
        c.observe_agent(&call("c1", "read_file", serde_json::json!({"path": "x"})));
        thread::sleep(Duration::from_millis(5));
        c.observe_agent(&result("c1", "read_file", false, "…"));
        let s = c.finish(false);
        let silent = s.max_silent_ms.expect("≥2 feedback events → silent gap");
        assert!(
            silent >= 20,
            "max silent gap should capture the 25ms pause, got {silent}ms"
        );
        assert!(s.ttff_ms.is_some());
    }

    #[test]
    fn command_progress_counts_as_feedback_for_ttff() {
        let mut c = SignalCollector::new(Vec::new());
        c.observe_agent(&AgentEvent::CommandProgress {
            label: "cargo test".into(),
            elapsed_ms: 1000,
        });
        let s = c.finish(false);
        assert!(s.ttff_ms.is_some());
    }

    #[test]
    fn stream_attempt_and_task_started_count_as_early_feedback() {
        // Host-side "work started" must set TTFF without waiting for tokens.
        let mut c = SignalCollector::new(Vec::new());
        c.observe_agent(&AgentEvent::StreamAttemptStarted);
        let s = c.finish(false);
        assert!(s.ttff_ms.is_some());
        assert!(
            s.ttff_ms.unwrap() < 1000,
            "StreamAttemptStarted is immediate host feedback"
        );

        let mut c = SignalCollector::new(Vec::new());
        c.observe_engine(EngineEvent::TaskStarted {
            goal: "x".into(),
            model: "m".into(),
            mode: "assisted".into(),
            sandbox: false,
            kind: leveler_engine::ExecutionKind::Orchestrate,
        });
        let s = c.finish(false);
        assert!(s.ttff_ms.is_some());
        assert!(
            s.ttff_ms.unwrap() < 1000,
            "TaskStarted is immediate host feedback, got {:?}",
            s.ttff_ms
        );
    }
}
