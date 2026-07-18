//! The engine's unified event taxonomy (plan B2).
//!
//! One enum covers kernel events (lifecycle, model/tool, approvals,
//! verification) and strategy events (plan/orchestrate). Events are
//! adjacently tagged (`{"type": …, "payload": …}`) so the persisted `type`
//! column is queryable without parsing payloads. Replaying an unknown type is
//! a hard error — never silently skipped.
//!
//! ## Projection contract (M2)
//!
//! [`EngineEvent`] is the *canonical* domain fact: persisted (unless
//! [`EngineEvent::is_transient`]) and replayable. Client-facing `RuntimeEvent`
//! (in `leveler-client-protocol`) is a *projection* of it, built by the app
//! layer; UI types never flow back down here (the engine has no dependency on
//! any client crate). Three properties hold:
//!
//! - **persist-before-forward**: a non-transient event is written to the log
//!   before any observer sees it, so a crash never exposes an un-persisted
//!   fact (see `EventLog::append`).
//! - **transient loss is recoverable**: deltas/usage/run-finished markers carry
//!   no replay value; a client that misses them rebuilds authoritative state
//!   from a snapshot.
//! - **data classification** ([`EngineEvent::data_class`]): every event is
//!   either `Projectable` (safe for a future sanitized cloud projection) or
//!   `LocalOnly` (embeds source, model output, tool output, or full context).

use serde::{Deserialize, Serialize};

use leveler_core::{ApprovalId, ClarificationId, TurnId};
use leveler_lifecycle::{AgentState, TaskOutcome, TurnOutcome};
use leveler_orchestrator::{NodeStatus, Requirement, ReviewFinding, TaskGraph};

use crate::EngineError;

/// How a session executes: single agent, plan/orchestrate state machine, or
/// parallel worktree candidates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionKind {
    Direct,
    Orchestrate,
    Parallel,
}

impl ExecutionKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ExecutionKind::Direct => "direct",
            ExecutionKind::Orchestrate => "orchestrate",
            ExecutionKind::Parallel => "parallel",
        }
    }

    pub fn parse(s: &str) -> Result<Self, EngineError> {
        match s {
            "direct" => Ok(ExecutionKind::Direct),
            // Canonical wire form is `orchestrate`; accept legacy `orchestrated`.
            "orchestrate" | "orchestrated" => Ok(ExecutionKind::Orchestrate),
            "parallel" => Ok(ExecutionKind::Parallel),
            other => Err(EngineError::Corrupt(format!(
                "unknown execution kind `{other}`"
            ))),
        }
    }
}

#[cfg(test)]
mod execution_kind_tests {
    use super::*;

    #[test]
    fn orchestrated_legacy_kind_normalizes_to_orchestrate() {
        assert_eq!(
            ExecutionKind::parse("orchestrated").unwrap(),
            ExecutionKind::Orchestrate
        );
        assert_eq!(ExecutionKind::Orchestrate.as_str(), "orchestrate");
        assert_eq!(
            ExecutionKind::parse("orchestrate").unwrap(),
            ExecutionKind::Orchestrate
        );
    }
}

/// What a turn is, matching `turns.kind`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TurnKind {
    /// The top-level user goal (goal-mode executor).
    User,
    /// A conversational turn (multimodal content, goal mode off).
    Chat,
    /// One orchestrated graph node.
    Node { node_id: String },
    /// A verification-repair attempt.
    Repair { attempt: u32 },
}

impl TurnKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            TurnKind::User => "user",
            TurnKind::Chat => "chat",
            TurnKind::Node { .. } => "node",
            TurnKind::Repair { .. } => "repair",
        }
    }
}

/// Every event the engine can emit. Kernel events first, then strategy
/// events. Transient events (deltas, usage) are forwarded to observers but
/// never persisted — see [`EngineEvent::is_transient`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum EngineEvent {
    // ── kernel: lifecycle ────────────────────────────────────────────────
    TaskStarted {
        goal: String,
        model: String,
        /// request_approval | assisted | full_access
        mode: String,
        sandbox: bool,
        kind: ExecutionKind,
    },
    TurnStarted {
        turn_id: TurnId,
        kind: TurnKind,
    },
    TurnFinished {
        turn_id: TurnId,
        /// Explicit terminal status. Legacy events omitted it and deserialize
        /// as `Completed`; new failed/interrupted paths must persist it.
        #[serde(default)]
        outcome: TurnOutcome,
        /// Debug repr of the executor's StopReason (completed | blocked | …).
        stop_reason: String,
        rounds: u32,
        modified_files: Vec<String>,
    },
    TaskFinished {
        outcome: TaskOutcome,
        reason: Option<String>,
    },

    // ── kernel: model / tools (1:1 from AgentEvent) ──────────────────────
    /// TRANSIENT: discard in-flight deltas before a fresh stream attempt.
    StreamAttemptStarted,
    /// TRANSIENT: streamed assistant text.
    AssistantDelta {
        text: String,
    },
    /// TRANSIENT: streamed reasoning text.
    ReasoningDelta {
        text: String,
    },
    AssistantMessage {
        text: String,
    },
    ToolCallStarted {
        call_id: String,
        name: String,
        arguments: String,
        /// Risk recorded at execution time. Legacy events omit it; recovery
        /// treats `None` conservatively rather than consulting today's registry.
        #[serde(default)]
        risk: Option<leveler_execution::RiskLevel>,
    },
    ToolCallFinished {
        call_id: String,
        name: String,
        is_error: bool,
        preview: String,
    },
    WorkspaceSnapshotCreated {
        call_id: String,
        snapshot: String,
    },
    /// TRANSIENT: token usage for the context gauge.
    TokenUsage {
        input_tokens: u32,
        output_tokens: u32,
        cached_input_tokens: u32,
    },
    Compacted {
        from: usize,
        to: usize,
    },
    /// The model replaced its structured plan (update_plan tool). Full list,
    /// not a delta; step text derives from the task/model output.
    PlanUpdated {
        steps: Vec<leveler_agent::PlanStep>,
    },
    /// Host refused update_goal(complete) (process gate). Persisted for UI/resume.
    GoalIntercepted {
        kind: String,
        detail: String,
    },
    /// Delivery process-evidence ledger snapshot (SoT for resume seed).
    EvidenceLedgerUpdated {
        ledger: leveler_lifecycle::EvidenceLedger,
    },
    /// Cross-round progress / closeout ledger (engine continue reads last).
    ProgressUpdated {
        ledger: leveler_lifecycle::ProgressLedger,
    },
    /// Exact messages the next request will use. Unlike the raw transcript,
    /// this includes compaction and transient continuation nudges.
    ContextSnapshot {
        messages: Vec<leveler_model::Message>,
    },
    SubAgentStarted {
        id: String,
        nickname: String,
        role: String,
        task: String,
    },
    /// TRANSIENT: live execution state and cumulative usage for one sub-agent.
    SubAgentProgress {
        id: String,
        active: bool,
        input_tokens: u32,
        output_tokens: u32,
        cached_input_tokens: u32,
    },
    SubAgentFinished {
        id: String,
        nickname: String,
        ok: bool,
        summary: String,
    },
    /// TRANSIENT: the executor's final-text marker; the turn runner replaces
    /// it with [`EngineEvent::TurnFinished`].
    RunFinished {
        text: String,
    },

    // ── kernel: approvals / clarifications ──────────────────────────────
    ApprovalRequested {
        id: ApprovalId,
        tool: String,
        summary: String,
        command: Option<String>,
        risk: String,
    },
    ApprovalResolved {
        id: ApprovalId,
        /// approve | approve_session | deny
        decision: String,
    },
    ClarificationRequested {
        id: ClarificationId,
        question: String,
        options: Vec<String>,
    },
    ClarificationAnswered {
        id: ClarificationId,
        answer: String,
    },

    // ── kernel: verification ─────────────────────────────────────────────
    VerificationStarted,
    VerificationCheck {
        name: String,
        /// passed | failed | skipped | tool_missing
        status: String,
        evidence: Option<String>,
    },
    VerificationFinished {
        passed: bool,
    },
    /// One acceptance criterion's command-backed evidence.
    /// `status`: met | unmet | unverifiable.
    /// `reject_reason`: optional machine-readable refuse code
    /// (`no_command` / `trivial` / `dangerous` / `cancelled`).
    AcceptanceEvidence {
        id: String,
        description: String,
        required: bool,
        status: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reject_reason: Option<String>,
    },

    // ── strategy: plan / orchestrate ─────────────────────────────────────
    PhaseChanged {
        from: AgentState,
        to: AgentState,
    },
    RequirementReady {
        requirement: Requirement,
    },
    ContextReady {
        candidate_files: Vec<String>,
        estimated_tokens: u32,
    },
    PlanReady {
        graph: TaskGraph,
    },
    NodeStarted {
        node_id: String,
        description: String,
    },
    NodeFinished {
        node_id: String,
        status: NodeStatus,
    },
    RepairStarted {
        attempt: u32,
    },

    // ── strategy: parallel worktree candidates ───────────────────────────
    CandidateStarted {
        branch: String,
    },
    CandidateFinished {
        branch: String,
        /// The child engine session that produced this candidate.
        session_id: String,
        verified: bool,
    },
    ReviewStarted {
        lenses: usize,
    },
    ReviewFinding {
        finding: ReviewFinding,
    },
    ReviewFailed {
        lens: String,
        error: String,
    },
    ReviewFinished {
        findings: usize,
        failures: usize,
        blocking: bool,
    },
}

impl EngineEvent {
    /// Transient events are forwarded to observers but never persisted (they
    /// are the overwhelming write volume and carry no replay value).
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            EngineEvent::StreamAttemptStarted
                | EngineEvent::AssistantDelta { .. }
                | EngineEvent::ReasoningDelta { .. }
                | EngineEvent::TokenUsage { .. }
                | EngineEvent::SubAgentProgress { .. }
                | EngineEvent::RunFinished { .. }
        )
    }

    /// The persisted row: (`type` tag, full tagged JSON payload). The tag is
    /// extracted from the serialization itself so it can never drift from the
    /// serde attribute.
    pub fn to_row(&self) -> Result<(String, String), EngineError> {
        let value = serde_json::to_value(self)?;
        let tag = value
            .get("type")
            .and_then(|t| t.as_str())
            .ok_or_else(|| EngineError::Corrupt("event serialized without a type tag".into()))?
            .to_string();
        Ok((tag, value.to_string()))
    }

    /// Replay a persisted payload. An unknown or malformed event is a hard
    /// error — a resume must never silently drop history.
    pub fn from_payload(payload: &str) -> Result<Self, EngineError> {
        serde_json::from_str(payload)
            .map_err(|e| EngineError::Corrupt(format!("unreplayable event payload: {e}")))
    }

    /// How far this event may travel beyond the local machine. Local execution
    /// keeps everything; a future cloud control-plane projection (M6/M7) may
    /// carry only [`DataClass::Projectable`] events — lifecycle, status, phase,
    /// verifier verdicts, and the approval/clarification control surface a phone
    /// needs. [`DataClass::LocalOnly`] events embed source code, model output,
    /// tool output, or full conversation context and must never enter a syncable
    /// projection.
    ///
    /// The match is exhaustive on purpose: adding an event forces a deliberate
    /// classification rather than defaulting to "shareable".
    pub fn data_class(&self) -> DataClass {
        use DataClass::{LocalOnly, Projectable};
        match self {
            // Lifecycle, status, counts, ids, verdicts, and the remote-control
            // surface — safe for a sanitized projection.
            EngineEvent::TaskStarted { .. }
            | EngineEvent::TurnStarted { .. }
            | EngineEvent::TurnFinished { .. }
            | EngineEvent::TaskFinished { .. }
            | EngineEvent::TokenUsage { .. }
            | EngineEvent::Compacted { .. }
            | EngineEvent::ApprovalRequested { .. }
            | EngineEvent::ApprovalResolved { .. }
            | EngineEvent::ClarificationRequested { .. }
            | EngineEvent::ClarificationAnswered { .. }
            | EngineEvent::VerificationStarted
            | EngineEvent::VerificationFinished { .. }
            | EngineEvent::AcceptanceEvidence { .. }
            | EngineEvent::PhaseChanged { .. }
            | EngineEvent::ProgressUpdated { .. }
            | EngineEvent::ContextReady { .. }
            | EngineEvent::NodeStarted { .. }
            | EngineEvent::NodeFinished { .. }
            | EngineEvent::RepairStarted { .. }
            | EngineEvent::CandidateStarted { .. }
            | EngineEvent::CandidateFinished { .. }
            | EngineEvent::ReviewStarted { .. }
            | EngineEvent::ReviewFinished { .. } => Projectable,

            // Embeds model output, tool arguments/output, source-bearing
            // evidence, or the full conversation — local machine only.
            EngineEvent::StreamAttemptStarted
            | EngineEvent::AssistantDelta { .. }
            | EngineEvent::ReasoningDelta { .. }
            | EngineEvent::AssistantMessage { .. }
            | EngineEvent::ToolCallStarted { .. }
            | EngineEvent::ToolCallFinished { .. }
            | EngineEvent::WorkspaceSnapshotCreated { .. }
            | EngineEvent::ContextSnapshot { .. }
            | EngineEvent::SubAgentStarted { .. }
            | EngineEvent::SubAgentProgress { .. }
            | EngineEvent::SubAgentFinished { .. }
            | EngineEvent::RunFinished { .. }
            | EngineEvent::PlanUpdated { .. }
            | EngineEvent::GoalIntercepted { .. }
            | EngineEvent::EvidenceLedgerUpdated { .. }
            | EngineEvent::VerificationCheck { .. }
            | EngineEvent::RequirementReady { .. }
            | EngineEvent::PlanReady { .. }
            | EngineEvent::ReviewFinding { .. }
            | EngineEvent::ReviewFailed { .. } => LocalOnly,
        }
    }

    /// Build the only event representation allowed to cross a remote/public
    /// boundary. This is an explicit reconstruction, never serialization of
    /// the domain event, so sensitive fields cannot be accidentally retained.
    /// Returning `None` is the deny-by-default path.
    pub fn public_projection(&self) -> Option<PublicEvent> {
        Some(match self {
            EngineEvent::TaskStarted { sandbox, kind, .. } => PublicEvent::TaskStarted {
                sandbox: *sandbox,
                kind: *kind,
            },
            EngineEvent::TurnStarted { turn_id, kind } => PublicEvent::TurnStarted {
                turn_id: turn_id.clone(),
                kind: PublicTurnKind::from(kind),
            },
            EngineEvent::TurnFinished {
                turn_id,
                outcome,
                rounds,
                modified_files,
                ..
            } => PublicEvent::TurnFinished {
                turn_id: turn_id.clone(),
                outcome: *outcome,
                rounds: *rounds,
                modified_file_count: modified_files.len(),
            },
            EngineEvent::TaskFinished { outcome, .. } => {
                PublicEvent::TaskFinished { outcome: *outcome }
            }
            EngineEvent::TokenUsage {
                input_tokens,
                output_tokens,
                cached_input_tokens,
            } => PublicEvent::TokenUsage {
                input_tokens: *input_tokens,
                output_tokens: *output_tokens,
                cached_input_tokens: *cached_input_tokens,
            },
            EngineEvent::Compacted { from, to } => PublicEvent::Compacted {
                from: *from,
                to: *to,
            },
            EngineEvent::ApprovalRequested { id, .. } => {
                PublicEvent::ApprovalRequested { id: id.clone() }
            }
            EngineEvent::ApprovalResolved { id, .. } => {
                PublicEvent::ApprovalResolved { id: id.clone() }
            }
            EngineEvent::ClarificationRequested { id, options, .. } => {
                PublicEvent::ClarificationRequested {
                    id: id.clone(),
                    option_count: options.len(),
                }
            }
            EngineEvent::ClarificationAnswered { id, .. } => {
                PublicEvent::ClarificationAnswered { id: id.clone() }
            }
            EngineEvent::VerificationStarted => PublicEvent::VerificationStarted,
            EngineEvent::VerificationFinished { passed } => {
                PublicEvent::VerificationFinished { passed: *passed }
            }
            EngineEvent::AcceptanceEvidence {
                required, status, ..
            } => PublicEvent::AcceptanceEvidence {
                required: *required,
                status: PublicAcceptanceStatus::parse(status)?,
            },
            EngineEvent::PhaseChanged { from, to } => PublicEvent::PhaseChanged {
                from: *from,
                to: *to,
            },
            EngineEvent::ProgressUpdated { ledger } => PublicEvent::TurnProgress {
                closing: ledger.closing,
                no_progress_streak: ledger.no_progress_streak,
                closeout_deny_rounds: ledger.closeout_deny_rounds,
            },
            EngineEvent::ContextReady {
                candidate_files,
                estimated_tokens,
            } => PublicEvent::ContextReady {
                candidate_file_count: candidate_files.len(),
                estimated_tokens: *estimated_tokens,
            },
            EngineEvent::NodeStarted { .. } => PublicEvent::NodeStarted,
            EngineEvent::NodeFinished { status, .. } => {
                PublicEvent::NodeFinished { status: *status }
            }
            EngineEvent::RepairStarted { attempt } => {
                PublicEvent::RepairStarted { attempt: *attempt }
            }
            EngineEvent::CandidateStarted { .. } => PublicEvent::CandidateStarted,
            EngineEvent::CandidateFinished { verified, .. } => PublicEvent::CandidateFinished {
                verified: *verified,
            },
            EngineEvent::ReviewStarted { lenses } => PublicEvent::ReviewStarted { lenses: *lenses },
            EngineEvent::ReviewFinished {
                findings,
                failures,
                blocking,
            } => PublicEvent::ReviewFinished {
                findings: *findings,
                failures: *failures,
                blocking: *blocking,
            },

            EngineEvent::StreamAttemptStarted
            | EngineEvent::AssistantDelta { .. }
            | EngineEvent::ReasoningDelta { .. }
            | EngineEvent::AssistantMessage { .. }
            | EngineEvent::ToolCallStarted { .. }
            | EngineEvent::ToolCallFinished { .. }
            | EngineEvent::WorkspaceSnapshotCreated { .. }
            | EngineEvent::ContextSnapshot { .. }
            | EngineEvent::SubAgentStarted { .. }
            | EngineEvent::SubAgentProgress { .. }
            | EngineEvent::SubAgentFinished { .. }
            | EngineEvent::RunFinished { .. }
            | EngineEvent::PlanUpdated { .. }
            | EngineEvent::GoalIntercepted { .. }
            | EngineEvent::EvidenceLedgerUpdated { .. }
            | EngineEvent::VerificationCheck { .. }
            | EngineEvent::RequirementReady { .. }
            | EngineEvent::PlanReady { .. }
            | EngineEvent::ReviewFinding { .. }
            | EngineEvent::ReviewFailed { .. } => return None,
        })
    }
}

/// Sanitized lifecycle facts allowed to leave the local runtime. It contains
/// no free-form text, paths, commands, prompts, model output, or tool data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum PublicEvent {
    TaskStarted {
        sandbox: bool,
        kind: ExecutionKind,
    },
    TurnStarted {
        turn_id: TurnId,
        kind: PublicTurnKind,
    },
    TurnFinished {
        turn_id: TurnId,
        outcome: TurnOutcome,
        rounds: u32,
        modified_file_count: usize,
    },
    TaskFinished {
        outcome: TaskOutcome,
    },
    TokenUsage {
        input_tokens: u32,
        output_tokens: u32,
        cached_input_tokens: u32,
    },
    Compacted {
        from: usize,
        to: usize,
    },
    ApprovalRequested {
        id: ApprovalId,
    },
    ApprovalResolved {
        id: ApprovalId,
    },
    ClarificationRequested {
        id: ClarificationId,
        option_count: usize,
    },
    ClarificationAnswered {
        id: ClarificationId,
    },
    VerificationStarted,
    VerificationFinished {
        passed: bool,
    },
    AcceptanceEvidence {
        required: bool,
        status: PublicAcceptanceStatus,
    },
    PhaseChanged {
        from: AgentState,
        to: AgentState,
    },
    /// Coarse thrash/closeout counters only (no paths/tool text).
    TurnProgress {
        closing: bool,
        no_progress_streak: u32,
        closeout_deny_rounds: u32,
    },
    ContextReady {
        candidate_file_count: usize,
        estimated_tokens: u32,
    },
    NodeStarted,
    NodeFinished {
        status: NodeStatus,
    },
    RepairStarted {
        attempt: u32,
    },
    CandidateStarted,
    CandidateFinished {
        verified: bool,
    },
    ReviewStarted {
        lenses: usize,
    },
    ReviewFinished {
        findings: usize,
        failures: usize,
        blocking: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicTurnKind {
    User,
    Chat,
    Node,
    Repair,
}

impl From<&TurnKind> for PublicTurnKind {
    fn from(value: &TurnKind) -> Self {
        match value {
            TurnKind::User => Self::User,
            TurnKind::Chat => Self::Chat,
            TurnKind::Node { .. } => Self::Node,
            TurnKind::Repair { .. } => Self::Repair,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicAcceptanceStatus {
    Met,
    Unmet,
    Unverifiable,
}

impl PublicAcceptanceStatus {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "met" => Some(Self::Met),
            "unmet" => Some(Self::Unmet),
            "unverifiable" => Some(Self::Unverifiable),
            _ => None,
        }
    }
}

/// How far an [`EngineEvent`] may travel beyond the local machine — see
/// [`EngineEvent::data_class`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataClass {
    /// Safe for a sanitized cloud/control-plane projection.
    Projectable,
    /// Contains source, model content, tool output, or full context — never
    /// leaves the local machine.
    LocalOnly,
}

/// Convert the executor's event stream 1:1. `Finished` becomes the transient
/// [`EngineEvent::RunFinished`]; the turn runner emits the real
/// [`EngineEvent::TurnFinished`] with turn identity and stop reason.
impl From<leveler_agent::AgentEvent> for EngineEvent {
    fn from(event: leveler_agent::AgentEvent) -> Self {
        use leveler_agent::AgentEvent as A;
        match event {
            A::StreamAttemptStarted => EngineEvent::StreamAttemptStarted,
            A::AssistantDelta(text) => EngineEvent::AssistantDelta { text },
            A::ReasoningDelta(text) => EngineEvent::ReasoningDelta { text },
            A::AssistantText(text) => EngineEvent::AssistantMessage { text },
            A::ToolCall {
                id,
                name,
                arguments,
            } => EngineEvent::ToolCallStarted {
                call_id: id,
                name,
                arguments,
                risk: None,
            },
            A::ToolResult {
                id,
                name,
                is_error,
                preview,
            } => EngineEvent::ToolCallFinished {
                call_id: id,
                name,
                is_error,
                preview,
            },
            A::WorkspaceSnapshot { call_id, snapshot } => {
                EngineEvent::WorkspaceSnapshotCreated { call_id, snapshot }
            }
            A::Usage {
                input_tokens,
                output_tokens,
                cached_input_tokens,
            } => EngineEvent::TokenUsage {
                input_tokens,
                output_tokens,
                cached_input_tokens,
            },
            A::Compacted { from, to } => EngineEvent::Compacted { from, to },
            A::PlanUpdated { steps } => EngineEvent::PlanUpdated { steps },
            A::GoalIntercepted { kind, detail } => EngineEvent::GoalIntercepted { kind, detail },
            A::EvidenceLedgerUpdated { ledger } => EngineEvent::EvidenceLedgerUpdated { ledger },
            A::ProgressUpdated { ledger } => EngineEvent::ProgressUpdated { ledger },
            A::ContextSnapshot { messages } => EngineEvent::ContextSnapshot { messages },
            A::VerificationStarted => EngineEvent::VerificationStarted,
            A::VerificationCheck {
                name,
                status,
                evidence,
            } => EngineEvent::VerificationCheck {
                name,
                status: match status {
                    leveler_agent::AgentVerificationStatus::Passed => "passed".to_string(),
                    leveler_agent::AgentVerificationStatus::Failed => "failed".to_string(),
                    leveler_agent::AgentVerificationStatus::Skipped => "skipped".to_string(),
                },
                evidence,
            },
            A::VerificationFinished { passed } => EngineEvent::VerificationFinished { passed },
            A::SubAgentStarted {
                id,
                nickname,
                role,
                task,
            } => EngineEvent::SubAgentStarted {
                id,
                nickname,
                role,
                task,
            },
            A::SubAgentProgress {
                id,
                active,
                input_tokens,
                output_tokens,
                cached_input_tokens,
            } => EngineEvent::SubAgentProgress {
                id,
                active,
                input_tokens,
                output_tokens,
                cached_input_tokens,
            },
            A::SubAgentFinished {
                id,
                nickname,
                ok,
                summary,
            } => EngineEvent::SubAgentFinished {
                id,
                nickname,
                ok,
                summary,
            },
            A::Finished(text) => EngineEvent::RunFinished { text },
        }
    }
}

#[cfg(test)]
mod contract_tests {
    use super::*;

    #[test]
    fn legacy_turn_finished_without_outcome_defaults_to_completed() {
        let payload = serde_json::json!({
            "type": "turn_finished",
            "payload": {
                "turn_id": "turn-1",
                "stop_reason": "Completed",
                "rounds": 2,
                "modified_files": []
            }
        });
        let event = EngineEvent::from_payload(&payload.to_string()).unwrap();
        assert!(matches!(
            event,
            EngineEvent::TurnFinished {
                outcome: TurnOutcome::Completed,
                ..
            }
        ));
    }

    #[test]
    fn full_context_and_model_content_are_local_only() {
        // The full conversation, model output, and tool output must never enter
        // a syncable projection.
        assert_eq!(
            EngineEvent::ContextSnapshot { messages: vec![] }.data_class(),
            DataClass::LocalOnly
        );
        assert_eq!(
            EngineEvent::AssistantMessage { text: "x".into() }.data_class(),
            DataClass::LocalOnly
        );
        assert_eq!(
            EngineEvent::ToolCallFinished {
                call_id: "c".into(),
                name: "read_file".into(),
                is_error: false,
                preview: "source".into(),
            }
            .data_class(),
            DataClass::LocalOnly
        );
        assert_eq!(
            EngineEvent::VerificationCheck {
                name: "test".into(),
                status: "failed".into(),
                evidence: Some("stack trace".into()),
            }
            .data_class(),
            DataClass::LocalOnly
        );
    }

    #[test]
    fn lifecycle_and_control_surface_are_projectable() {
        assert_eq!(
            EngineEvent::TaskFinished {
                outcome: TaskOutcome::Verified,
                reason: None,
            }
            .data_class(),
            DataClass::Projectable
        );
        assert_eq!(
            EngineEvent::PhaseChanged {
                from: AgentState::Plan,
                to: AgentState::Execute,
            }
            .data_class(),
            DataClass::Projectable
        );
        assert_eq!(
            EngineEvent::ApprovalRequested {
                id: leveler_core::ApprovalId::generate(),
                tool: "run_command".into(),
                summary: "run tests".into(),
                command: Some("cargo test".into()),
                risk: "assisted".into(),
            }
            .data_class(),
            DataClass::Projectable
        );
        assert_eq!(
            EngineEvent::VerificationFinished { passed: true }.data_class(),
            DataClass::Projectable
        );
    }

    #[test]
    fn public_projection_does_not_serialize_sensitive_event_fields() {
        let secret = "LVTEST_PUBLIC_SECRET_DO_NOT_LEAK";
        let events = [
            EngineEvent::TaskStarted {
                goal: secret.into(),
                model: secret.into(),
                mode: secret.into(),
                sandbox: true,
                kind: ExecutionKind::Direct,
            },
            EngineEvent::TurnFinished {
                turn_id: TurnId::new("turn-safe"),
                outcome: TurnOutcome::Failed,
                stop_reason: secret.into(),
                rounds: 2,
                modified_files: vec![secret.into()],
            },
            EngineEvent::TaskFinished {
                outcome: TaskOutcome::Failed,
                reason: Some(secret.into()),
            },
            EngineEvent::ApprovalRequested {
                id: ApprovalId::new("approval-safe"),
                tool: secret.into(),
                summary: secret.into(),
                command: Some(secret.into()),
                risk: "destructive".into(),
            },
            EngineEvent::ClarificationRequested {
                id: ClarificationId::new("clarification-safe"),
                question: secret.into(),
                options: vec![secret.into()],
            },
            EngineEvent::CandidateStarted {
                branch: secret.into(),
            },
        ];

        for event in events {
            let projected = event
                .public_projection()
                .expect("safe event shape should remain projectable");
            let json = serde_json::to_string(&projected).unwrap();
            assert!(!json.contains(secret), "public projection leaked: {json}");
        }
    }

    #[test]
    fn source_and_model_content_have_no_public_projection() {
        for event in [
            EngineEvent::AssistantMessage {
                text: "source".into(),
            },
            EngineEvent::ToolCallStarted {
                call_id: "call".into(),
                name: "run_command".into(),
                arguments: "{\"token\":\"secret\"}".into(),
                risk: None,
            },
            EngineEvent::ContextSnapshot { messages: vec![] },
        ] {
            assert!(event.public_projection().is_none());
        }
    }

    #[test]
    fn transient_events_are_never_persisted_and_carry_no_replay_value() {
        // Losing a transient event must not lose authoritative state — the
        // client rebuilds from a snapshot, and the log never stored it.
        for e in [
            EngineEvent::AssistantDelta { text: "d".into() },
            EngineEvent::ReasoningDelta { text: "r".into() },
            EngineEvent::TokenUsage {
                input_tokens: 1,
                output_tokens: 2,
                cached_input_tokens: 0,
            },
            EngineEvent::RunFinished { text: "f".into() },
        ] {
            assert!(e.is_transient(), "{e:?} must be transient");
        }
        // Canonical lifecycle events are persisted.
        assert!(
            !EngineEvent::TaskFinished {
                outcome: TaskOutcome::Failed,
                reason: None,
            }
            .is_transient()
        );
    }

    #[test]
    fn sub_agent_progress_has_a_transient_engine_event_shape() {
        let payload = serde_json::json!({
            "type": "sub_agent_progress",
            "payload": {
                "id": "agent-2",
                "active": true,
                "input_tokens": 2400,
                "output_tokens": 180,
                "cached_input_tokens": 1200
            }
        })
        .to_string();
        let event = EngineEvent::from_payload(&payload);
        assert!(
            event.is_ok(),
            "progress event must have a stable wire shape"
        );
        assert!(event.unwrap().is_transient());
    }

    #[test]
    fn persisted_events_round_trip_through_to_row_and_from_payload() {
        let event = EngineEvent::PhaseChanged {
            from: AgentState::Understand,
            to: AgentState::Localize,
        };
        let (tag, payload) = event.to_row().unwrap();
        assert_eq!(tag, "phase_changed");
        assert_eq!(EngineEvent::from_payload(&payload).unwrap(), event);
    }

    #[test]
    fn legacy_tool_started_without_risk_remains_readable_and_unknown() {
        let payload = r#"{"type":"tool_call_started","payload":{"call_id":"c1","name":"read_file","arguments":"{}"}}"#;
        assert!(matches!(
            EngineEvent::from_payload(payload).unwrap(),
            EngineEvent::ToolCallStarted { risk: None, .. }
        ));
    }
}
