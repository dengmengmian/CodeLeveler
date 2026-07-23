//! Command delivery, idempotency, and crash-window recovery (M5).
//!
//! A future remote client (mobile → cloud → local daemon) delivers commands
//! over an unreliable link, so delivery is **at-least-once**: the same command
//! may arrive more than once. That is safe for *records* (dedup by
//! `command_id`) but does **not** make arbitrary *side effects* exactly-once —
//! a shell command that ran but whose completion event was lost cannot be
//! proven un-run. These types make that boundary explicit for every transport.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use leveler_core::{CommandId, SessionId};

use crate::command::ClientCommand;

/// A command as delivered: an idempotency key, the aggregate it targets, the
/// version it expects (optimistic concurrency against the event log), when it
/// was issued, and the payload. Production in-process and socket clients use
/// the same envelope path; raw `send` remains only as a low-level/testing seam.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandEnvelope {
    pub command_id: CommandId,
    pub session_id: SessionId,
    /// The event-log sequence the issuer last saw. `None` = don't check.
    pub expected_version: Option<i64>,
    /// RFC3339 timestamp, supplied by the issuer (not read from the clock here).
    pub issued_at: String,
    pub command: ClientCommand,
}

/// The result of admitting a command receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Receipt {
    /// First time this `command_id` is seen — run it.
    Accepted,
    /// A duplicate delivery — already admitted; must not run again.
    Duplicate,
}

/// Deduplicates at-least-once command delivery by `command_id`. Admitting the
/// same id twice yields [`Receipt::Duplicate`], so a retried command never
/// starts its action a second time.
#[derive(Debug, Default)]
pub struct CommandReceipts {
    seen: HashSet<String>,
}

impl CommandReceipts {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a receipt. Returns whether this is the first admission.
    pub fn admit(&mut self, command_id: &CommandId) -> Receipt {
        if self.seen.insert(command_id.as_str().to_string()) {
            Receipt::Accepted
        } else {
            Receipt::Duplicate
        }
    }

    pub fn has_seen(&self, command_id: &CommandId) -> bool {
        self.seen.contains(command_id.as_str())
    }
}

/// How an action interrupted between "started" and "its completion event" may
/// be recovered on resume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recovery {
    /// The action is idempotent (a read/search): re-running it has no external
    /// effect, so a resume may replay it automatically.
    SafeReplay,
    /// The action has a side effect that cannot be proven un-done (a shell
    /// command, an external API, a file write): a resume must NOT auto-replay —
    /// it surfaces for human confirmation.
    RequiresConfirmation,
}

/// Classify a tool by whether re-running it after an unconfirmed crash is safe.
/// Read-only tools are replayable; anything that mutates the workspace or runs
/// a process requires confirmation. Unknown tools default to the safe (for the
/// user) side: confirmation.
pub fn recovery_for_tool(tool_name: &str) -> Recovery {
    if leveler_model::is_safe_replay_tool(tool_name) {
        Recovery::SafeReplay
    } else {
        Recovery::RequiresConfirmation
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_command_receipt_does_not_readmit() {
        let mut receipts = CommandReceipts::new();
        let id = CommandId::new("cmd-1");
        assert_eq!(receipts.admit(&id), Receipt::Accepted);
        assert_eq!(receipts.admit(&id), Receipt::Duplicate);
        assert_eq!(receipts.admit(&CommandId::new("cmd-2")), Receipt::Accepted);
    }

    #[test]
    fn non_idempotent_actions_require_confirmation_on_recovery() {
        assert_eq!(recovery_for_tool("read_file"), Recovery::SafeReplay);
        assert_eq!(recovery_for_tool("grep"), Recovery::SafeReplay);
        assert_eq!(
            recovery_for_tool("run_command"),
            Recovery::RequiresConfirmation
        );
        assert_eq!(
            recovery_for_tool("apply_patch"),
            Recovery::RequiresConfirmation
        );
        // An unknown tool is treated conservatively.
        assert_eq!(
            recovery_for_tool("mcp__x__y"),
            Recovery::RequiresConfirmation
        );
    }

    #[test]
    fn envelope_round_trips_through_serde() {
        let env = CommandEnvelope {
            command_id: CommandId::new("cmd-9"),
            session_id: SessionId::new("s-1"),
            expected_version: Some(42),
            issued_at: "2026-07-12T00:00:00Z".to_string(),
            command: ClientCommand::Quit,
        };
        let json = serde_json::to_string(&env).unwrap();
        let back: CommandEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(back.command_id, env.command_id);
        assert_eq!(back.expected_version, Some(42));
    }
}
