//! Mapping the loop's events onto the engine's durable vocabulary.
//!
//! The harness owns this direction: the engine defines what it persists, and
//! the agent says which of its own events mean what. Putting it here is what
//! keeps `EngineEvent` free of any agent type.

use leveler_engine::EngineEvent;

use crate::{AgentEvent, AgentVerificationStatus};

/// Convert the executor's event stream 1:1. `Finished` becomes the transient
/// [`EngineEvent::RunFinished`]; the turn runner emits the real
/// [`EngineEvent::TurnFinished`] with turn identity and stop reason.
impl From<AgentEvent> for EngineEvent {
    fn from(event: AgentEvent) -> Self {
        use AgentEvent as A;
        match event {
            A::StreamAttemptStarted => EngineEvent::StreamAttemptStarted,
            A::AssistantDelta(text) => EngineEvent::AssistantDelta { text },
            A::ReasoningDelta(text) => EngineEvent::ReasoningDelta { text },
            A::AssistantText(text) => EngineEvent::AssistantMessage { text },
            A::ToolCall {
                id,
                name,
                arguments,
                parallel,
            } => EngineEvent::ToolCallStarted {
                call_id: id,
                name,
                arguments,
                parallel,
                // Stamped by the caller that owns the registry; see
                // `coding::turn::forward_event`.
                risk: None,
                // The top-level loop's own call; a delegated one arrives as a
                // ChildToolEvent and carries its agent id.
                agent_id: None,
            },
            A::ToolResult {
                id,
                name,
                is_error,
                preview,
                applied_diff,
            } => EngineEvent::ToolCallFinished {
                call_id: id,
                name,
                is_error,
                preview,
                agent_id: None,
                applied_diff,
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
            A::AdvisoryStarted { kind } => EngineEvent::AdvisoryStarted {
                kind: kind.as_key().to_string(),
            },
            A::CommandProgress { label, elapsed_ms } => {
                EngineEvent::CommandProgress { label, elapsed_ms }
            }
            A::PlanUpdated { steps } => EngineEvent::PlanUpdated { steps },
            A::GoalIntercepted { kind, detail } => EngineEvent::GoalIntercepted { kind, detail },
            A::DelegationStage { action, detail } => {
                EngineEvent::DelegationStage { action, detail }
            }
            A::EvidenceLedgerUpdated { ledger } => EngineEvent::EvidenceLedgerUpdated { ledger },
            A::ProgressUpdated { ledger } => EngineEvent::ProgressUpdated { ledger },
            A::ContextSnapshot { messages } => EngineEvent::ContextSnapshot {
                messages,
                // Executor in-loop snapshots carry no durable ordinal; they
                // merge via the legacy overlap heuristic on restore.
                through_ordinal: None,
            },
            A::VerificationStarted => EngineEvent::VerificationStarted,
            A::VerificationCheck {
                name,
                status,
                evidence,
            } => EngineEvent::VerificationCheck {
                name,
                status: match status {
                    AgentVerificationStatus::Passed => "passed".to_string(),
                    AgentVerificationStatus::Failed => "failed".to_string(),
                    AgentVerificationStatus::Skipped => "skipped".to_string(),
                },
                evidence,
            },
            A::VerificationFinished {
                passed,
                verification,
            } => EngineEvent::VerificationFinished {
                passed,
                verification,
            },
            A::SubAgentStarted {
                id,
                nickname,
                role,
                task,
                profile_id,
                profile_role,
                read_only,
            } => EngineEvent::SubAgentStarted {
                id,
                nickname,
                role,
                task,
                profile_id,
                profile_role,
                read_only,
            },
            // Durable-only: the drive loop intercepts this and writes the
            // row. Nothing downstream renders it, and the child's running
            // totals already reach the screen as SubAgentProgress. Mapping it
            // to that keeps the conversion total without inventing an engine
            // event nobody consumes.
            A::SubAgentModelRequest { record } => EngineEvent::SubAgentProgress {
                id: record.agent_id.clone().unwrap_or_default(),
                active: true,
                input_tokens: record.usage.input_tokens.min(u32::MAX as u64) as u32,
                output_tokens: record.usage.output_tokens.min(u32::MAX as u64) as u32,
                cached_input_tokens: record.usage.cached_input_tokens.min(u32::MAX as u64) as u32,
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
                contribution,
            } => EngineEvent::SubAgentFinished {
                id,
                nickname,
                ok,
                summary,
                contribution,
            },
            A::SubAgentActivity {
                id,
                phase,
                tool,
                preview,
                is_error,
            } => EngineEvent::SubAgentActivity {
                id,
                phase,
                tool,
                preview,
                is_error,
            },
            A::Finished(text) => EngineEvent::RunFinished { text },
        }
    }
}
