//! [`ClientCommand`] — intents a client sends into the runtime.
//!
//! Commands submit messages and cancel (or force-cancel) running
//! turn, quit. Later phases add plan approval, clarifications, permission
//! decisions, model/mode switches, attachments, checkpoints — as new variants.

use serde::{Deserialize, Serialize};

use leveler_core::{ApprovalId, CheckpointId, ClarificationId, SessionId};
use leveler_model::ModelRef;

use super::media::AttachmentRef;
use super::{ApprovalDecision, PermissionProfile};

/// A command from a UI client to the runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientCommand {
    /// Submit a user message; the runtime drives a turn in the given session.
    SubmitMessage {
        session_id: SessionId,
        content: String,
        #[serde(default)]
        attachments: Vec<AttachmentRef>,
    },
    /// Run an explicit goal task. Unlike ordinary chat messages, this enables
    /// goal-mode completion (`update_goal`) in the agent loop.
    RunGoal {
        session_id: SessionId,
        content: String,
    },
    /// Import a file as an attachment; the runtime processes and stores it.
    AddAttachment { session_id: SessionId, path: String },
    /// Import an attachment from immutable base64-encoded bytes already read
    /// by a trusted client. This avoids reopening an ambient path after a
    /// security-sensitive upload or file-picker validation.
    AddAttachmentData {
        session_id: SessionId,
        name: String,
        data_base64: String,
    },
    /// Import an image from the system clipboard (spec §38.1).
    AddClipboardImage { session_id: SessionId },
    /// Cooperatively cancel the running turn (graceful; resumable).
    CancelCurrentTurn { session_id: SessionId },
    /// Escalate a cancel the user has already requested once.
    ForceCancelCurrentTurn { session_id: SessionId },
    /// Resolve a pending permission request .
    ApprovalDecision {
        request_id: ApprovalId,
        decision: ApprovalDecision,
    },
    /// Answer a pending clarification (spec §35). An empty answer means "skip".
    AnswerClarification {
        request_id: ClarificationId,
        answer: String,
    },
    /// Switch the model used for subsequent turns .
    SelectModel {
        session_id: SessionId,
        model: ModelRef,
    },
    /// Switch the execution mode used for subsequent turns .
    SetPermissionProfile {
        session_id: SessionId,
        mode: PermissionProfile,
    },
    /// Set product session axes (work profile × collaboration). Wire strings:
    /// work_profile = economy|balanced|delivery; collaboration = chat|plan|goal.
    SetProductAxes {
        session_id: SessionId,
        work_profile: String,
        collaboration: String,
    },
    /// Confirm a collaboration-plan proposal and auto-enter goal mode (K24).
    ConfirmPlanToGoal {
        session_id: SessionId,
        /// Optional plan body; empty uses the last assistant proposal text.
        content: String,
    },
    /// List project durable memory (active; optional archived) for TUI/CLI.
    ListMemory {
        session_id: SessionId,
        /// Include archived (forgotten) entries.
        #[serde(default)]
        include_archived: bool,
    },
    /// Archive (forget) one active memory id — user-authoritative (no model).
    ForgetMemory { session_id: SessionId, id: String },
    /// Choose whether turns run the direct tool loop or the full orchestrated
    /// state machine (Understand to Plan to Execute to Verify to Review).
    SetAgentMode {
        session_id: SessionId,
        orchestrate: bool,
    },
    /// Recompute and push the working-tree diff.
    RequestDiff { session_id: SessionId },
    /// Summarize and compact the conversation history (spec §28, §53).
    CompactContext { session_id: SessionId },
    /// Start a fresh conversation: drop the session's stored message history so
    /// the next turn carries no prior context (a real "new chat", not a screen
    /// clear).
    ClearConversation { session_id: SessionId },
    /// Ask for the list of stored sessions (spec §52).
    RequestSessionList,
    /// Ask for the session list and route the response only to the requesting
    /// session's event stream.
    RequestSessionListFor { requester_session_id: SessionId },
    /// Open a stored session, loading its transcript into the view.
    OpenSession { session_id: SessionId },
    /// Open a stored session on behalf of another currently displayed session.
    /// The switch event is delivered to the requester before the client moves
    /// its subscription to the target.
    OpenSessionFor {
        requester_session_id: SessionId,
        session_id: SessionId,
    },
    /// Delete a stored session.
    DeleteSession { session_id: SessionId },
    /// Delete a stored session and route the refreshed list to the requester.
    DeleteSessionFor {
        requester_session_id: SessionId,
        session_id: SessionId,
    },
    /// Rename a stored session (overwrite its goal/title text).
    RenameSession { session_id: SessionId, name: String },
    /// Archive a stored session: it keeps its transcript but leaves the
    /// default session list.
    ArchiveSession { session_id: SessionId },
    /// Fork a stored session: create a new session with a copy of the
    /// transcript, so an alternative direction can be explored without
    /// touching the original.
    ForkSession { session_id: SessionId },
    /// Restore the conversation to a checkpoint (spec §68).
    RestoreCheckpoint {
        session_id: SessionId,
        checkpoint_id: CheckpointId,
    },
    /// Side question (`/btw`): single-turn answer using current session
    /// context, without tools and without appending to the transcript store.
    Btw {
        session_id: SessionId,
        question: String,
    },
    /// The runtime owner is shutting down; all work should stop. Disconnecting
    /// an individual UI client must not issue this command.
    Quit,
}

impl ClientCommand {
    /// The session this command targets, if any. Session-less commands
    /// (`ApprovalDecision`/`AnswerClarification` key off a request id;
    /// `RequestSessionList`/`Quit` are global) return `None`.
    pub fn session_id(&self) -> Option<&SessionId> {
        match self {
            ClientCommand::SubmitMessage { session_id, .. }
            | ClientCommand::RunGoal { session_id, .. }
            | ClientCommand::AddAttachment { session_id, .. }
            | ClientCommand::AddAttachmentData { session_id, .. }
            | ClientCommand::AddClipboardImage { session_id }
            | ClientCommand::CancelCurrentTurn { session_id }
            | ClientCommand::ForceCancelCurrentTurn { session_id }
            | ClientCommand::SelectModel { session_id, .. }
            | ClientCommand::SetPermissionProfile { session_id, .. }
            | ClientCommand::SetAgentMode { session_id, .. }
            | ClientCommand::SetProductAxes { session_id, .. }
            | ClientCommand::ConfirmPlanToGoal { session_id, .. }
            | ClientCommand::ListMemory { session_id, .. }
            | ClientCommand::ForgetMemory { session_id, .. }
            | ClientCommand::RequestDiff { session_id }
            | ClientCommand::CompactContext { session_id }
            | ClientCommand::ClearConversation { session_id }
            | ClientCommand::OpenSession { session_id }
            | ClientCommand::DeleteSession { session_id }
            | ClientCommand::RenameSession { session_id, .. }
            | ClientCommand::ArchiveSession { session_id }
            | ClientCommand::ForkSession { session_id }
            | ClientCommand::RestoreCheckpoint { session_id, .. }
            | ClientCommand::Btw { session_id, .. } => Some(session_id),
            ClientCommand::RequestSessionListFor {
                requester_session_id,
            }
            | ClientCommand::OpenSessionFor {
                requester_session_id,
                ..
            }
            | ClientCommand::DeleteSessionFor {
                requester_session_id,
                ..
            } => Some(requester_session_id),
            ClientCommand::ApprovalDecision { .. }
            | ClientCommand::AnswerClarification { .. }
            | ClientCommand::RequestSessionList
            | ClientCommand::Quit => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(cmd: ClientCommand, expected_type: &str) {
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(
            json.contains(&format!("\"type\":\"{expected_type}\"")),
            "json did not contain expected type: {json}"
        );
        let back: ClientCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cmd);
    }

    #[test]
    fn submit_message_roundtrips_through_serde() {
        roundtrip(
            ClientCommand::SubmitMessage {
                session_id: SessionId::new("s1"),
                content: "你好".to_string(),
                attachments: Vec::new(),
            },
            "submit_message",
        );
    }

    #[test]
    fn run_goal_roundtrips_through_serde() {
        roundtrip(
            ClientCommand::RunGoal {
                session_id: SessionId::new("s1"),
                content: "修复测试".to_string(),
            },
            "run_goal",
        );
    }

    #[test]
    fn add_attachment_roundtrips() {
        roundtrip(
            ClientCommand::AddAttachment {
                session_id: SessionId::new("s1"),
                path: "/tmp/file.rs".to_string(),
            },
            "add_attachment",
        );
    }

    #[test]
    fn add_attachment_data_roundtrips() {
        roundtrip(
            ClientCommand::AddAttachmentData {
                session_id: SessionId::new("s1"),
                name: "image.png".to_string(),
                data_base64: "aW1tdXRhYmxl".to_string(),
            },
            "add_attachment_data",
        );
    }

    #[test]
    fn add_clipboard_image_roundtrips() {
        roundtrip(
            ClientCommand::AddClipboardImage {
                session_id: SessionId::new("s1"),
            },
            "add_clipboard_image",
        );
    }

    #[test]
    fn session_menu_commands_roundtrip() {
        roundtrip(
            ClientCommand::RenameSession {
                session_id: SessionId::new("s1"),
                name: "登录修复".to_string(),
            },
            "rename_session",
        );
        roundtrip(
            ClientCommand::ArchiveSession {
                session_id: SessionId::new("s1"),
            },
            "archive_session",
        );
        roundtrip(
            ClientCommand::ForkSession {
                session_id: SessionId::new("s1"),
            },
            "fork_session",
        );
    }

    #[test]
    fn cancel_current_turn_roundtrips() {
        roundtrip(
            ClientCommand::CancelCurrentTurn {
                session_id: SessionId::new("s1"),
            },
            "cancel_current_turn",
        );
    }

    #[test]
    fn force_cancel_current_turn_roundtrips() {
        roundtrip(
            ClientCommand::ForceCancelCurrentTurn {
                session_id: SessionId::new("s1"),
            },
            "force_cancel_current_turn",
        );
    }

    #[test]
    fn approval_decision_roundtrips() {
        for decision in [
            ApprovalDecision::ApproveOnce,
            ApprovalDecision::ApproveSession,
            ApprovalDecision::Deny,
        ] {
            roundtrip(
                ClientCommand::ApprovalDecision {
                    request_id: ApprovalId::new("a1"),
                    decision,
                },
                "approval_decision",
            );
        }
    }

    #[test]
    fn answer_clarification_roundtrips() {
        roundtrip(
            ClientCommand::AnswerClarification {
                request_id: ClarificationId::new("c1"),
                answer: "yes".to_string(),
            },
            "answer_clarification",
        );
    }

    #[test]
    fn select_model_roundtrips() {
        roundtrip(
            ClientCommand::SelectModel {
                session_id: SessionId::new("s1"),
                model: ModelRef::new("openai", "gpt-4o"),
            },
            "select_model",
        );
    }

    #[test]
    fn set_permission_profile_roundtrips() {
        roundtrip(
            ClientCommand::SetPermissionProfile {
                session_id: SessionId::new("s1"),
                mode: PermissionProfile::FullAccess,
            },
            "set_permission_profile",
        );
    }

    #[test]
    fn set_agent_mode_roundtrips() {
        roundtrip(
            ClientCommand::SetAgentMode {
                session_id: SessionId::new("s1"),
                orchestrate: true,
            },
            "set_agent_mode",
        );
    }

    #[test]
    fn request_diff_roundtrips() {
        roundtrip(
            ClientCommand::RequestDiff {
                session_id: SessionId::new("s1"),
            },
            "request_diff",
        );
    }

    #[test]
    fn compact_context_roundtrips() {
        roundtrip(
            ClientCommand::CompactContext {
                session_id: SessionId::new("s1"),
            },
            "compact_context",
        );
    }

    #[test]
    fn clear_conversation_roundtrips() {
        roundtrip(
            ClientCommand::ClearConversation {
                session_id: SessionId::new("s1"),
            },
            "clear_conversation",
        );
    }

    #[test]
    fn request_session_list_roundtrips() {
        roundtrip(ClientCommand::RequestSessionList, "request_session_list");
        roundtrip(
            ClientCommand::RequestSessionListFor {
                requester_session_id: SessionId::new("s1"),
            },
            "request_session_list_for",
        );
    }

    #[test]
    fn open_session_roundtrips() {
        roundtrip(
            ClientCommand::OpenSession {
                session_id: SessionId::new("s1"),
            },
            "open_session",
        );
        roundtrip(
            ClientCommand::OpenSessionFor {
                requester_session_id: SessionId::new("s1"),
                session_id: SessionId::new("s2"),
            },
            "open_session_for",
        );
    }

    #[test]
    fn delete_session_roundtrips() {
        roundtrip(
            ClientCommand::DeleteSession {
                session_id: SessionId::new("s1"),
            },
            "delete_session",
        );
        roundtrip(
            ClientCommand::DeleteSessionFor {
                requester_session_id: SessionId::new("s1"),
                session_id: SessionId::new("s2"),
            },
            "delete_session_for",
        );
    }

    #[test]
    fn restore_checkpoint_roundtrips() {
        roundtrip(
            ClientCommand::RestoreCheckpoint {
                session_id: SessionId::new("s1"),
                checkpoint_id: CheckpointId::new("chk-1"),
            },
            "restore_checkpoint",
        );
    }

    #[test]
    fn quit_roundtrips() {
        roundtrip(ClientCommand::Quit, "quit");
    }

    #[test]
    fn btw_roundtrips() {
        roundtrip(
            ClientCommand::Btw {
                session_id: SessionId::new("s1"),
                question: "这个函数做什么？".to_string(),
            },
            "btw",
        );
    }
}
