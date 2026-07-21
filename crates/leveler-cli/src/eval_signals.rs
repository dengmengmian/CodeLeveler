//! Trajectory-signal collection for eval failure attribution (L1 taskset doc
//! §8). The collector folds the agent/engine event stream into the pure
//! [`TrajectorySignals`] that `leveler_eval::classify_failure` consumes — it
//! observes facts (tool names, error markers, node outcomes), never model text
//! semantics.

use std::collections::HashSet;

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
}

impl SignalCollector {
    pub(crate) fn new(relevant_paths: impl IntoIterator<Item = String>) -> Self {
        Self {
            signals: TrajectorySignals::default(),
            relevant: relevant_paths.into_iter().collect(),
            verify_calls: HashSet::new(),
            error_streak: None,
        }
    }

    pub(crate) fn observe_agent(&mut self, event: &AgentEvent) {
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
            return;
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
            serde_json::json!({"patch": "x"}),
        ));
        c.observe_agent(&result(
            "c2",
            "apply_patch",
            true,
            "could not find expected lines",
        ));
        c.observe_agent(&call(
            "c3",
            "apply_patch",
            serde_json::json!({"patch": "y"}),
        ));
        c.observe_agent(&result(
            "c3",
            "apply_patch",
            true,
            "could not find expected lines",
        ));
        c.observe_agent(&call(
            "c4",
            "apply_patch",
            serde_json::json!({"patch": "z"}),
        ));
        c.observe_agent(&result("c4", "apply_patch", false, "ok"));

        let s = c.finish(false);
        assert!(s.touched_relevant_files);
        assert_eq!(s.edit_attempts, 3);
        assert_eq!(s.edit_failures, 2);
        assert_eq!(classify_failure(&s), FailureCategory::Editing);
    }

    /// The loop guard's marker in a tool result counts a tooling trip.
    #[test]
    fn loop_guard_marker_counts_as_tooling() {
        let mut c = SignalCollector::new(Vec::new());
        c.observe_agent(&call(
            "c1",
            "run_command",
            serde_json::json!({"program": "ls"}),
        ));
        c.observe_agent(&result(
            "c1",
            "run_command",
            true,
            "This exact `run_command` call already ran 2 times with the same result and made no progress.",
        ));

        let s = c.finish(false);
        assert_eq!(s.loop_guard_trips, 1);
        assert_eq!(classify_failure(&s), FailureCategory::Tooling);
    }

    /// A denial marker flags an environment failure, which outranks the rest.
    #[test]
    fn denial_marker_flags_environment() {
        let mut c = SignalCollector::new(Vec::new());
        c.observe_agent(&call(
            "c1",
            "run_command",
            serde_json::json!({"program": "curl"}),
        ));
        c.observe_agent(&result(
            "c1",
            "run_command",
            true,
            "action not permitted: network access is blocked",
        ));

        let s = c.finish(false);
        assert!(s.env_failure);
        assert_eq!(classify_failure(&s), FailureCategory::Environment);
    }

    /// A successful verification-class command marks `verification_ran`; a
    /// non-verification command (echo) does not.
    #[test]
    fn only_verification_class_commands_mark_verification_ran() {
        let mut c = SignalCollector::new(Vec::new());
        c.observe_agent(&call(
            "c1",
            "run_command",
            serde_json::json!({"program": "echo", "args": ["hi"]}),
        ));
        c.observe_agent(&result("c1", "run_command", false, "hi"));
        let s = c.finish(false);
        assert!(!s.verification_ran);

        let mut c = SignalCollector::new(Vec::new());
        c.observe_agent(&call(
            "c2",
            "run_command",
            serde_json::json!({"program": "cargo", "args": ["test"]}),
        ));
        c.observe_agent(&result("c2", "run_command", false, "ok"));
        let s = c.finish(false);
        assert!(s.verification_ran);
    }

    /// Engine node outcomes feed the planning signal; other engine events
    /// reuse the agent logic through the shim (compaction shown here).
    #[test]
    fn engine_events_feed_node_and_compaction_signals() {
        let mut c = SignalCollector::new(Vec::new());
        c.observe_engine(EngineEvent::NodeFinished {
            node_id: "n1".into(),
            status: leveler_orchestrator::NodeStatus::Failed,
        });
        c.observe_engine(EngineEvent::NodeFinished {
            node_id: "n2".into(),
            status: leveler_orchestrator::NodeStatus::Completed,
        });
        c.observe_engine(EngineEvent::Compacted { from: 30, to: 12 });

        let s = c.finish(false);
        assert_eq!(s.node_total, 2);
        assert_eq!(s.node_failures, 1);
        assert_eq!(s.compactions, 1);
    }

    /// Consecutive same-tool errors build the arg-error streak; a success
    /// resets it. Three in a row is a tooling failure.
    #[test]
    fn consecutive_same_tool_errors_build_the_streak() {
        let mut c = SignalCollector::new(vec!["a.rs".to_string()]);
        for i in 0..3 {
            let id = format!("c{i}");
            c.observe_agent(&call(&id, "find_symbol", serde_json::json!({"q": i})));
            c.observe_agent(&result(&id, "find_symbol", true, "tool error: bad args"));
        }
        let s = c.finish(false);
        assert_eq!(s.arg_error_streak, 3);
        assert_eq!(classify_failure(&s), FailureCategory::Tooling);
    }
}
