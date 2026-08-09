//! Trajectory-signal collection for eval failure attribution (L1 taskset doc
//! §8). The collector folds the agent/engine event stream into the pure
//! [`TrajectorySignals`] that `leveler_eval::classify_failure` consumes — it
//! observes facts (tool names, error markers, node outcomes), never model text
//! semantics.
//!
//! Also measures **TTFF** (time to first user-visible feedback) and **max silent
//! gap** between feedback events from the same stream, using wall-clock
//! timestamps at observation time.

use std::collections::{HashMap, HashSet};
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
    /// Paths a complete change must reach (callers, consumers, contracts,
    /// config hops, tests). Metrics only — never shown to the agent.
    impact: Vec<String>,
    /// The metrics-only paths this run actually reached, by name — the source
    /// of the coverage counts and of the missed-path list a failed case is
    /// diagnosed with.
    relevant_seen: HashSet<String>,
    impact_seen: HashSet<String>,
    /// Distractors looked at, forbidden paths modified, and the snapshot of
    /// coverage taken at the moment of the first edit.
    distractor: Vec<String>,
    forbidden: Vec<String>,
    distractor_seen: HashSet<String>,
    forbidden_edited: HashSet<String>,
    coverage_at_first_edit: Option<(u32, u32)>,
    /// Paths a call *named*, held until its result comes back. Evidence is
    /// what a tool returned, not what it was asked for: a read that errored
    /// leaves the agent exactly as blind as never calling it (§24).
    pending_coverage: HashMap<String, (Vec<String>, Vec<String>, Vec<String>)>,
    /// Set once a verification-class command has failed, so an impact path
    /// first reached afterwards can be attributed to the compiler rather than
    /// to navigation.
    verification_failed: bool,
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
    /// Model rounds proxied by stream-attempt starts (retries inflate this
    /// slightly); used to stamp `first_edit_round`.
    rounds_started: u32,
    /// Distinct file paths read so far, and distinct search queries issued,
    /// so a repeat can be counted the first time it repeats.
    files_read: HashSet<String>,
    search_queries: HashSet<String>,
}

impl SignalCollector {
    #[cfg(test)]
    pub(crate) fn new(relevant_paths: impl IntoIterator<Item = String>) -> Self {
        Self::with_navigation_paths(relevant_paths, Vec::new(), Vec::new(), Vec::new())
    }

    /// The full metrics-only path model a navigation case declares.
    pub(crate) fn with_navigation_paths(
        relevant_paths: impl IntoIterator<Item = String>,
        impact_paths: impl IntoIterator<Item = String>,
        distractor_paths: impl IntoIterator<Item = String>,
        forbidden_edit_paths: impl IntoIterator<Item = String>,
    ) -> Self {
        Self {
            signals: TrajectorySignals::default(),
            impact: impact_paths.into_iter().collect(),
            relevant: relevant_paths.into_iter().collect(),
            relevant_seen: HashSet::new(),
            impact_seen: HashSet::new(),
            distractor: distractor_paths.into_iter().collect(),
            forbidden: forbidden_edit_paths.into_iter().collect(),
            distractor_seen: HashSet::new(),
            forbidden_edited: HashSet::new(),
            coverage_at_first_edit: None,
            pending_coverage: HashMap::new(),
            verification_failed: false,
            verify_calls: HashSet::new(),
            error_streak: None,
            started: Instant::now(),
            first_feedback: None,
            last_feedback: None,
            max_silent_ms: 0,
            feedback_events: 0,
            rounds_started: 0,
            files_read: HashSet::new(),
            search_queries: HashSet::new(),
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
        if matches!(event, AgentEvent::StreamAttemptStarted) {
            self.rounds_started = self.rounds_started.saturating_add(1);
        }
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
                // Coverage: every metrics-only path this call names, whether it
                // reads it or patches it. `arguments` is the raw JSON, so a
                // path inside an apply_patch body counts the same as a
                // `path` argument.
                let named = |group: &Vec<String>| -> Vec<String> {
                    group
                        .iter()
                        .filter(|p| arguments.contains(p.as_str()))
                        .cloned()
                        .collect()
                };
                self.pending_coverage.insert(
                    id.clone(),
                    (
                        named(&self.relevant),
                        named(&self.impact),
                        named(&self.distractor),
                    ),
                );
                if matches!(name.as_str(), "apply_patch" | "replace") {
                    for path in &self.forbidden {
                        if arguments.contains(path.as_str()) {
                            self.forbidden_edited.insert(path.clone());
                        }
                    }
                    // Freeze what the run knew when it first committed to a
                    // change; anything after this is recovery, not planning.
                    self.coverage_at_first_edit.get_or_insert((
                        self.relevant_seen.len() as u32,
                        self.impact_seen.len() as u32,
                    ));
                }
                if !self.signals.touched_relevant_files
                    && self.relevant.iter().any(|p| arguments.contains(p.as_str()))
                {
                    self.signals.touched_relevant_files = true;
                    // The round the run stopped hunting and looked at the code
                    // that actually matters.
                    self.signals.first_relevant_file_round = Some(self.rounds_started.max(1));
                }
                if name == "update_plan" && self.signals.first_plan_round.is_none() {
                    self.signals.first_plan_round = Some(self.rounds_started.max(1));
                }
                match name.as_str() {
                    "read_file" | "read_symbol" => {
                        self.signals.read_calls += 1;
                        // A path already read is a repeat; a new one widens the
                        // working set. Both are legitimate, the ratio is the
                        // signal.
                        if let Some(path) = tool_argument(arguments, "path")
                            && !self.files_read.insert(path)
                        {
                            self.signals.repeated_file_reads += 1;
                        }
                        self.signals.unique_files_read = self.files_read.len() as u32;
                        // No requested range means whole-file intent. The
                        // *returned* range can still be clipped by the byte
                        // cap; what this measures is what the model asked for,
                        // which is the navigation decision.
                        if has_argument(arguments, "start_line")
                            || has_argument(arguments, "end_line")
                        {
                            self.signals.narrow_reads += 1;
                        } else {
                            self.signals.broad_reads += 1;
                        }
                    }
                    "grep" | "find_files" | "find_symbol" | "list_files" | "find_references"
                    | "locate_hint" => {
                        self.signals.search_calls += 1;
                        // Whatever this tool calls its needle: the query text
                        // is what makes two searches "the same question".
                        let query = ["pattern", "query", "name", "symbol", "path", "text"]
                            .iter()
                            .find_map(|key| tool_argument(arguments, key))
                            .unwrap_or_else(|| arguments.to_string());
                        if !self.search_queries.insert(format!("{name}\u{1}{query}")) {
                            self.signals.repeated_search_queries += 1;
                        }
                        self.signals.unique_search_queries = self.search_queries.len() as u32;
                    }
                    "apply_patch" | "replace" => {
                        if name == "apply_patch" {
                            self.signals.apply_patch_calls += 1;
                        } else {
                            self.signals.replace_calls += 1;
                        }
                        if self.signals.first_edit_round.is_none() {
                            self.signals.first_edit_round = Some(self.rounds_started.max(1));
                        }
                    }
                    _ => {}
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
                    if self.verify_calls.contains(id) {
                        self.verification_failed = true;
                    }
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
                // Coverage lands here, once, and only for a call that returned.
                if let Some((relevant, impact, distractor)) = self.pending_coverage.remove(id)
                    && !*is_error
                {
                    self.relevant_seen.extend(relevant);
                    for path in impact {
                        // An impact path first reached after a check already
                        // failed was found by the compiler, not by navigation.
                        if self.verification_failed && !self.impact_seen.contains(&path) {
                            self.signals.verification_driven_impact_discovery = true;
                        }
                        self.impact_seen.insert(path);
                    }
                    self.distractor_seen.extend(distractor);
                }
            }
            AgentEvent::Compacted { .. } => self.signals.compactions += 1,
            AgentEvent::VerificationStarted => self.signals.verification_ran = true,
            _ => {}
        }
    }

    /// Engine events (session lifecycle, heartbeats) feed trajectory signals
    /// via the agent-event shim where applicable.
    pub(crate) fn observe_engine(&mut self, event: EngineEvent) {
        if let EngineEvent::NodeFinished { status, .. } = &event {
            self.signals.node_total += 1;
            if *status == leveler_engine::NodeStatus::Failed {
                self.signals.node_failures += 1;
            }
            self.note_feedback();
            return;
        }
        // TaskStarted / phase / command heartbeats are the earliest host-side
        // signals — must count toward TTFF so the metric reflects user-visible
        // progress, not first LLM token alone.
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
        self.signals.relevant_paths_touched = self.relevant_seen.len() as u32;
        self.signals.impact_paths_touched = self.impact_seen.len() as u32;
        self.signals.distractor_paths_read = self.distractor_seen.len() as u32;
        self.signals.forbidden_paths_edited = self.forbidden_edited.len() as u32;
        let (relevant_before, impact_before) = self.coverage_at_first_edit.unwrap_or((
            self.relevant_seen.len() as u32,
            self.impact_seen.len() as u32,
        ));
        self.signals.relevant_paths_before_edit = relevant_before;
        self.signals.impact_paths_before_edit = impact_before;
        self.signals
    }

    /// Impact-surface paths the run never reached. Empty is the good case; a
    /// non-empty list on a case the agent called done is the half-fix, named.
    #[cfg(test)]
    pub(crate) fn clone_signals(&self) -> TrajectorySignals {
        self.signals
    }

    pub(crate) fn missed_impact_paths(&self) -> Vec<String> {
        let mut missed: Vec<String> = self
            .impact
            .iter()
            .filter(|p| !self.impact_seen.contains(*p))
            .cloned()
            .collect();
        missed.sort();
        missed
    }
}

/// Whether a tool call's JSON arguments carry `key` at all — line numbers are
/// JSON numbers, so [`tool_argument`] (which reads strings) never sees them.
fn has_argument(arguments: &str, key: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(arguments)
        .ok()
        .and_then(|v| v.get(key).cloned())
        .is_some_and(|v| !v.is_null())
}

/// One string field out of a tool call's JSON arguments, when present.
fn tool_argument(arguments: &str, key: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(arguments)
        .ok()?
        .get(key)?
        .as_str()
        .map(str::to_string)
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

    /// Exploration/edit-selection counters: reads, searches, per-tool edit
    /// counts, and the first-edit round (proxied by stream attempts).
    #[test]
    fn exploration_and_edit_counters_track_tool_classes() {
        let mut c = SignalCollector::new(Vec::new());
        c.observe_agent(&AgentEvent::StreamAttemptStarted); // round 1
        c.observe_agent(&call("c1", "grep", serde_json::json!({"pattern": "x"})));
        c.observe_agent(&result("c1", "grep", false, "1 match"));
        c.observe_agent(&call(
            "c2",
            "read_file",
            serde_json::json!({"path": "a.md"}),
        ));
        c.observe_agent(&result("c2", "read_file", false, "…"));
        c.observe_agent(&AgentEvent::StreamAttemptStarted); // round 2
        c.observe_agent(&call(
            "c3",
            "apply_patch",
            serde_json::json!({"patch": "p"}),
        ));
        c.observe_agent(&result("c3", "apply_patch", true, "failed to apply hunk"));
        c.observe_agent(&AgentEvent::StreamAttemptStarted); // round 3
        c.observe_agent(&call("c4", "replace", serde_json::json!({"path": "a.md"})));
        c.observe_agent(&result("c4", "replace", false, "ok"));

        let s = c.finish(false);
        assert_eq!(s.read_calls, 1);
        assert_eq!(s.search_calls, 1);
        assert_eq!(s.apply_patch_calls, 1);
        assert_eq!(s.replace_calls, 1);
        assert_eq!(s.edit_attempts, 2);
        assert_eq!(s.edit_failures, 1);
        assert_eq!(
            s.first_edit_round,
            Some(2),
            "first edit landed in the second stream attempt"
        );
    }

    /// `first_relevant_file_round` is a FIRST, not a latest: a later touch of
    /// the same (or another) relevant path must not move it, or the metric
    /// silently reports "when it last looked" instead of "when it found it".
    #[test]
    fn first_relevant_round_records_the_first_touch_only() {
        let mut c = SignalCollector::new(vec!["internal/window/window.go".to_string()]);
        c.observe_agent(&AgentEvent::StreamAttemptStarted); // round 1
        c.observe_agent(&call(
            "c1",
            "grep",
            serde_json::json!({"pattern": "boundary"}),
        ));
        c.observe_agent(&AgentEvent::StreamAttemptStarted); // round 2
        c.observe_agent(&call(
            "c2",
            "read_file",
            serde_json::json!({"path": "internal/window/window.go"}),
        ));
        for round in 3..=8 {
            c.observe_agent(&AgentEvent::StreamAttemptStarted);
            c.observe_agent(&call(
                &format!("c{round}"),
                "read_file",
                serde_json::json!({"path": "internal/window/window.go"}),
            ));
        }
        let s = c.finish(false);
        assert_eq!(
            s.first_relevant_file_round,
            Some(2),
            "six later touches must not overwrite the first"
        );
    }

    /// Irrelevant work first: the round counter still points at the round the
    /// run actually reached the relevant code.
    #[test]
    fn first_relevant_round_skips_irrelevant_exploration() {
        let mut c = SignalCollector::new(vec!["internal/window/window.go".to_string()]);
        for round in 1..=4 {
            c.observe_agent(&AgentEvent::StreamAttemptStarted);
            c.observe_agent(&call(
                &format!("c{round}"),
                "read_file",
                serde_json::json!({"path": format!("internal/billing/store{round}.go")}),
            ));
        }
        c.observe_agent(&AgentEvent::StreamAttemptStarted); // round 5
        c.observe_agent(&call(
            "hit",
            "read_file",
            serde_json::json!({"path": "internal/window/window.go"}),
        ));
        assert_eq!(c.finish(false).first_relevant_file_round, Some(5));
    }

    /// A tool call that returned successfully. Coverage is credited from the
    /// result, so a test that only emits the call is modelling a call whose
    /// outcome nobody ever saw.
    fn ok_call(c: &mut SignalCollector, id: &str, name: &str, args: serde_json::Value) {
        c.observe_agent(&call(id, name, args));
        c.observe_agent(&AgentEvent::ToolResult {
            id: id.to_string(),
            name: name.to_string(),
            is_error: false,
            preview: "ok".to_string(),
        });
    }

    /// Coverage, not just first contact: the run's recall over the paths a
    /// correct fix has to reach. A run that finds one of three relevant files
    /// and stops has `touched_relevant_files = true` and is still two thirds
    /// blind — that is exactly the half-fix this metric exists to expose.
    #[test]
    fn relevant_and_impact_coverage_are_tracked_per_path() {
        let mut c = SignalCollector::with_navigation_paths(
            vec!["cmd/root.go".to_string(), "cmd/utils.go".to_string()],
            vec!["cmd/root.go".to_string(), "pkg/yqlib/stream.go".to_string()],
            Vec::new(),
            Vec::new(),
        );
        c.observe_agent(&AgentEvent::StreamAttemptStarted);
        ok_call(
            &mut c,
            "c1",
            "read_file",
            serde_json::json!({"path": "cmd/root.go"}),
        );
        c.observe_agent(&AgentEvent::StreamAttemptStarted);
        ok_call(
            &mut c,
            "c2",
            "read_file",
            serde_json::json!({"path": "cmd/root.go"}),
        );
        assert_eq!(
            c.missed_impact_paths(),
            vec!["pkg/yqlib/stream.go".to_string()],
            "a never-read impact path is the half-fix, named"
        );
        let s = c.finish(false);
        assert_eq!(
            s.relevant_paths_touched, 1,
            "a repeat of the same path is not extra coverage"
        );
        assert_eq!(s.impact_paths_touched, 1);
    }

    /// An edit counts as reaching a path just as much as a read does — a run
    /// that patched a caller it found through a search has covered it.
    #[test]
    fn an_edit_covers_an_impact_path_too() {
        let mut c = SignalCollector::with_navigation_paths(
            Vec::new(),
            vec!["internal/report/scan.go".to_string()],
            Vec::new(),
            Vec::new(),
        );
        c.observe_agent(&AgentEvent::StreamAttemptStarted);
        ok_call(
            &mut c,
            "e1",
            "apply_patch",
            serde_json::json!({"patch": "*** Update File: internal/report/scan.go"}),
        );
        assert!(c.missed_impact_paths().is_empty());
        assert_eq!(c.finish(false).impact_paths_touched, 1);
    }

    /// Broad vs narrow reads: the shape C2.1 measured. A read with no range is
    /// a whole-file intent; one with a range is targeted. This distinguishes
    /// "read the right region" from "read the file and hope".
    #[test]
    fn reads_are_classified_broad_or_narrow_by_requested_range() {
        let mut c = SignalCollector::new(Vec::new());
        c.observe_agent(&AgentEvent::StreamAttemptStarted);
        c.observe_agent(&call("b", "read_file", serde_json::json!({"path": "a.go"})));
        c.observe_agent(&call(
            "n",
            "read_file",
            serde_json::json!({"path": "b.go", "start_line": 100, "end_line": 200}),
        ));
        c.observe_agent(&call(
            "n2",
            "read_file",
            serde_json::json!({"path": "c.go", "start_line": 40}),
        ));
        let s = c.finish(false);
        assert_eq!(s.broad_reads, 1);
        assert_eq!(s.narrow_reads, 2);
    }

    /// C2.3C §37 E — a read that came back as an error is not evidence. A run
    /// that tried to open the right file and failed has not seen it, and a
    /// coverage metric that counts the attempt would score blindness as
    /// insight.
    #[test]
    fn a_failed_read_does_not_count_as_coverage() {
        let mut c = SignalCollector::with_navigation_paths(
            vec!["internal/dispatch/router.go".to_string()],
            vec!["internal/dispatch/router.go".to_string()],
            Vec::new(),
            Vec::new(),
        );
        c.observe_agent(&AgentEvent::StreamAttemptStarted);
        c.observe_agent(&call(
            "miss",
            "read_file",
            serde_json::json!({"path": "internal/dispatch/router.go"}),
        ));
        c.observe_agent(&AgentEvent::ToolResult {
            id: "miss".to_string(),
            name: "read_file".to_string(),
            is_error: true,
            preview: "file not found: internal/dispatch/router.go".to_string(),
        });
        let s = c.finish(false);
        assert_eq!(
            s.relevant_paths_touched, 0,
            "a failed read must not register as having reached the file"
        );
        assert_eq!(s.impact_paths_touched, 0);
    }

    /// C2.3C §16 B — a partial read is still evidence. A run that read lines
    /// 100-200 of the right file has learned something real about it, and the
    /// path-touch metric says exactly that and no more: it never claims the
    /// rest of the file was seen.
    #[test]
    fn a_successful_partial_read_counts_as_a_path_touch() {
        let mut c = SignalCollector::with_navigation_paths(
            vec!["internal/ingest/decoder.go".to_string()],
            vec!["internal/ingest/decoder.go".to_string()],
            Vec::new(),
            Vec::new(),
        );
        c.observe_agent(&AgentEvent::StreamAttemptStarted);
        ok_call(
            &mut c,
            "part",
            "read_file",
            serde_json::json!({"path": "internal/ingest/decoder.go", "start_line": 100, "end_line": 200}),
        );
        let s = c.finish(false);
        assert_eq!(s.relevant_paths_touched, 1);
        assert_eq!(s.impact_paths_touched, 1);
        assert_eq!(s.narrow_reads, 1, "a ranged read is a narrow read");
        assert_eq!(s.broad_reads, 0);
    }

    /// C2.3C §37 I — looking at a distractor is normal navigation; editing a
    /// forbidden path is a localisation error. The two must never collapse
    /// into one number.
    #[test]
    fn reading_a_distractor_is_not_the_same_as_editing_a_forbidden_path() {
        let mut c = SignalCollector::with_navigation_paths(
            Vec::new(),
            Vec::new(),
            vec!["legacy/old_router.go".to_string()],
            vec!["legacy/old_router.go".to_string()],
        );
        c.observe_agent(&AgentEvent::StreamAttemptStarted);
        ok_call(
            &mut c,
            "look",
            "read_file",
            serde_json::json!({"path": "legacy/old_router.go"}),
        );
        let s = c.clone_signals();
        assert_eq!(s.distractor_paths_read, 0, "counted at finish, not inline");

        ok_call(
            &mut c,
            "peek",
            "read_file",
            serde_json::json!({"path": "legacy/old_router.go"}),
        );
        let s = c.finish(false);
        assert_eq!(s.distractor_paths_read, 1, "a distinct distractor, read");
        assert_eq!(
            s.forbidden_paths_edited, 0,
            "reading a forbidden path is exploration, not an error"
        );
    }

    #[test]
    fn editing_a_forbidden_path_is_recorded_as_an_error() {
        let mut c = SignalCollector::with_navigation_paths(
            Vec::new(),
            Vec::new(),
            vec!["legacy/old_router.go".to_string()],
            vec!["legacy/old_router.go".to_string()],
        );
        c.observe_agent(&AgentEvent::StreamAttemptStarted);
        ok_call(
            &mut c,
            "bad",
            "apply_patch",
            serde_json::json!({"patch": "*** Update File: legacy/old_router.go"}),
        );
        assert_eq!(c.finish(false).forbidden_paths_edited, 1);
    }

    /// C2.3C §30 — completing a task after the compiler pointed at the missing
    /// caller is a different capability from finding it first. The scorecard
    /// has to be able to tell them apart.
    #[test]
    fn impact_found_only_after_a_failed_check_is_attributed_to_verification() {
        let mut c = SignalCollector::with_navigation_paths(
            Vec::new(),
            vec!["internal/report/scan.go".to_string()],
            Vec::new(),
            Vec::new(),
        );
        c.observe_agent(&AgentEvent::StreamAttemptStarted);
        c.observe_agent(&call(
            "build",
            "run_command",
            serde_json::json!({"program": "go", "args": ["build", "./..."]}),
        ));
        c.observe_agent(&AgentEvent::ToolResult {
            id: "build".to_string(),
            name: "run_command".to_string(),
            is_error: true,
            preview: "scan.go:12: not enough arguments".to_string(),
        });
        ok_call(
            &mut c,
            "late",
            "read_file",
            serde_json::json!({"path": "internal/report/scan.go"}),
        );
        let s = c.finish(false);
        assert_eq!(s.impact_paths_touched, 1);
        assert!(
            s.verification_driven_impact_discovery,
            "the compiler found this surface, not the navigation"
        );
    }

    /// The same path reached before any edit is navigation, not recovery.
    #[test]
    fn impact_found_before_the_first_check_is_not_verification_driven() {
        let mut c = SignalCollector::with_navigation_paths(
            Vec::new(),
            vec!["internal/report/scan.go".to_string()],
            Vec::new(),
            Vec::new(),
        );
        c.observe_agent(&AgentEvent::StreamAttemptStarted);
        ok_call(
            &mut c,
            "early",
            "read_file",
            serde_json::json!({"path": "internal/report/scan.go"}),
        );
        let s = c.finish(false);
        assert_eq!(s.impact_paths_touched, 1);
        assert!(!s.verification_driven_impact_discovery);
    }

    /// C2.3C §20 — coverage at the moment of the first edit, separate from
    /// coverage at the end. A run that edits on one of three impact paths and
    /// discovers the rest afterwards must not report as if it knew all three.
    #[test]
    fn coverage_before_the_first_edit_is_frozen_at_that_edit() {
        let mut c = SignalCollector::with_navigation_paths(
            Vec::new(),
            vec!["a.go".to_string(), "b.go".to_string(), "c.go".to_string()],
            Vec::new(),
            Vec::new(),
        );
        c.observe_agent(&AgentEvent::StreamAttemptStarted);
        ok_call(
            &mut c,
            "r1",
            "read_file",
            serde_json::json!({"path": "a.go"}),
        );
        ok_call(
            &mut c,
            "e1",
            "apply_patch",
            serde_json::json!({"patch": "*** Update File: a.go"}),
        );
        ok_call(
            &mut c,
            "r2",
            "read_file",
            serde_json::json!({"path": "b.go"}),
        );
        ok_call(
            &mut c,
            "r3",
            "read_file",
            serde_json::json!({"path": "c.go"}),
        );
        let s = c.finish(false);
        assert_eq!(s.impact_paths_touched, 3, "all three reached eventually");
        assert_eq!(
            s.impact_paths_before_edit, 1,
            "only one was known when it committed to the change"
        );
    }

    /// Never reached: absent, never a fabricated round number.
    #[test]
    fn first_relevant_round_is_absent_when_never_touched() {
        let mut c = SignalCollector::new(vec!["internal/window/window.go".to_string()]);
        c.observe_agent(&AgentEvent::StreamAttemptStarted);
        c.observe_agent(&call(
            "c1",
            "read_file",
            serde_json::json!({"path": "README.md"}),
        ));
        let s = c.finish(false);
        assert_eq!(s.first_relevant_file_round, None);
        assert!(!s.touched_relevant_files);
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
            kind: leveler_engine::ExecutionKind::Direct,
            task_id: None,
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
