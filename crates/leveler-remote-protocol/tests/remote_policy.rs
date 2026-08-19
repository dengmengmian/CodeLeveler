//! The remote capability gate: which commands a paired phone may issue.
//!
//! Two properties matter more than any individual verdict.
//!
//! First, **completeness**. A command that nobody classified must not slip
//! through as allowed. The implementation matches exhaustively with no wildcard
//! arm, so a new `ClientCommand` variant is a compile error rather than a
//! silent permission; the table below is cross-checked against the exported
//! schema so it cannot quietly fall behind either.
//!
//! Second, **the nested decisions**. `ApprovalDecision` and
//! `SetPermissionProfile` are allowed commands carrying values that are not:
//! `ApproveAlways` writes a standing rule into the repository's
//! `permissions.yaml`, and `FullAccess` disarms the approval prompt. Gating the
//! command without inspecting its payload would let a phone grant itself
//! permanent local privileges.
#![cfg(feature = "policy")]

use leveler_client_protocol::{
    ApprovalDecision, ApprovalId, CheckpointId, ClarificationId, ClientCommand, ModelRef,
    PermissionProfile, SessionId,
};
use leveler_remote_protocol::pairing::PairingScope;
use leveler_remote_protocol::policy::RemotePolicy;

fn session() -> SessionId {
    SessionId::new("s1")
}

/// Every `ClientCommand` variant with the verdict the design's exhaustive table
/// assigns it for an `interactive` pairing.
fn every_variant() -> Vec<(&'static str, ClientCommand, bool)> {
    vec![
        (
            "submit_message",
            ClientCommand::SubmitMessage {
                session_id: session(),
                content: "hi".to_string(),
                attachments: Vec::new(),
            },
            true,
        ),
        (
            "run_goal",
            ClientCommand::RunGoal {
                session_id: session(),
                content: "goal".to_string(),
            },
            true,
        ),
        (
            "add_attachment",
            ClientCommand::AddAttachment {
                session_id: session(),
                path: "/etc/passwd".to_string(),
            },
            false,
        ),
        (
            "add_attachment_data",
            ClientCommand::AddAttachmentData {
                session_id: session(),
                name: "a.png".to_string(),
                data_base64: "AA==".to_string(),
            },
            false,
        ),
        (
            "add_clipboard_image",
            ClientCommand::AddClipboardImage {
                session_id: session(),
            },
            false,
        ),
        (
            "cancel_current_turn",
            ClientCommand::CancelCurrentTurn {
                session_id: session(),
            },
            true,
        ),
        (
            "force_cancel_current_turn",
            ClientCommand::ForceCancelCurrentTurn {
                session_id: session(),
            },
            true,
        ),
        (
            "approval_decision",
            ClientCommand::ApprovalDecision {
                request_id: ApprovalId::new("a1"),
                decision: ApprovalDecision::ApproveOnce,
            },
            true,
        ),
        (
            "answer_clarification",
            ClientCommand::AnswerClarification {
                request_id: ClarificationId::new("c1"),
                answer: "yes".to_string(),
            },
            true,
        ),
        (
            "select_model",
            ClientCommand::SelectModel {
                session_id: session(),
                model: ModelRef::new("anthropic", "claude"),
            },
            true,
        ),
        (
            "set_permission_profile",
            ClientCommand::SetPermissionProfile {
                session_id: session(),
                mode: PermissionProfile::RequestApproval,
            },
            true,
        ),
        (
            "set_product_axes",
            ClientCommand::SetProductAxes {
                session_id: session(),
                work_profile: "w".to_string(),
                collaboration: "c".to_string(),
            },
            true,
        ),
        (
            "confirm_plan_to_goal",
            ClientCommand::ConfirmPlanToGoal {
                session_id: session(),
                content: "plan".to_string(),
            },
            true,
        ),
        (
            "list_memory",
            ClientCommand::ListMemory {
                session_id: session(),
                include_archived: false,
            },
            false,
        ),
        (
            "forget_memory",
            ClientCommand::ForgetMemory {
                session_id: session(),
                id: "m1".to_string(),
            },
            false,
        ),
        (
            "accept_memory",
            ClientCommand::AcceptMemory {
                session_id: session(),
                id: "m1".to_string(),
            },
            false,
        ),
        (
            "steer_current_turn",
            ClientCommand::SteerCurrentTurn {
                session_id: session(),
                content: "prefer tests".to_string(),
            },
            true,
        ),
        (
            "request_diff",
            ClientCommand::RequestDiff {
                session_id: session(),
            },
            true,
        ),
        (
            "compact_context",
            ClientCommand::CompactContext {
                session_id: session(),
            },
            true,
        ),
        (
            "clear_conversation",
            ClientCommand::ClearConversation {
                session_id: session(),
            },
            true,
        ),
        (
            "request_session_list",
            ClientCommand::RequestSessionList,
            true,
        ),
        (
            "request_session_list_for",
            ClientCommand::RequestSessionListFor {
                requester_session_id: session(),
            },
            true,
        ),
        (
            // Starting a fresh session creates and deletes nothing, so a
            // paired phone may do it — unlike the memory and attachment
            // commands below, which stay shut.
            "new_session_for",
            ClientCommand::NewSessionFor {
                requester_session_id: session(),
            },
            true,
        ),
        (
            "open_session",
            ClientCommand::OpenSession {
                session_id: session(),
            },
            true,
        ),
        (
            "open_session_for",
            ClientCommand::OpenSessionFor {
                requester_session_id: session(),
                session_id: SessionId::new("s2"),
            },
            true,
        ),
        (
            "delete_session",
            ClientCommand::DeleteSession {
                session_id: session(),
            },
            false,
        ),
        (
            "delete_session_for",
            ClientCommand::DeleteSessionFor {
                requester_session_id: session(),
                session_id: SessionId::new("s2"),
            },
            false,
        ),
        (
            "rename_session",
            ClientCommand::RenameSession {
                session_id: session(),
                name: "n".to_string(),
            },
            false,
        ),
        (
            "archive_session",
            ClientCommand::ArchiveSession {
                session_id: session(),
            },
            false,
        ),
        (
            "fork_session",
            ClientCommand::ForkSession {
                session_id: session(),
            },
            false,
        ),
        (
            "restore_checkpoint",
            ClientCommand::RestoreCheckpoint {
                session_id: session(),
                checkpoint_id: CheckpointId::new("cp1"),
            },
            false,
        ),
        (
            "btw",
            ClientCommand::Btw {
                session_id: session(),
                question: "q".to_string(),
            },
            true,
        ),
        (
            "run_user_shell",
            ClientCommand::RunUserShell {
                session_id: session(),
                command: "cargo test".to_string(),
            },
            false,
        ),
        (
            "cancel_user_shell",
            ClientCommand::CancelUserShell {
                session_id: session(),
                execution_id: leveler_client_protocol::UserShellId::new("ush-1"),
            },
            false,
        ),
        (
            "query_observability",
            ClientCommand::QueryObservability {
                session_id: session(),
                center_seq: None,
                before: 0,
                after: 80,
            },
            false,
        ),
        ("quit", ClientCommand::Quit, false),
    ]
}

fn interactive() -> RemotePolicy {
    RemotePolicy {
        scope: PairingScope::Interactive,
        allow_full_access: false,
    }
}

/// The table above must name every variant the protocol actually has.
///
/// Checked against the schema exported by `leveler-client-protocol`, so adding
/// a command without classifying it fails here even before the exhaustive match
/// is considered.
#[test]
fn the_table_covers_every_command_the_schema_declares() {
    let schema: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../schemas/client_command.schema.json"),
        )
        .expect("the exported schema exists"),
    )
    .expect("schema parses");

    let mut declared: Vec<String> = schema["oneOf"]
        .as_array()
        .expect("tagged enum")
        .iter()
        .map(|variant| {
            variant["properties"]["type"]["enum"][0]
                .as_str()
                .expect("discriminator")
                .to_string()
        })
        .collect();
    declared.sort();

    let mut classified: Vec<String> = every_variant()
        .into_iter()
        .map(|(name, _, _)| name.to_string())
        .collect();
    classified.sort();

    assert_eq!(
        classified, declared,
        "every ClientCommand must have a remote verdict; unclassified commands must never default to allowed"
    );
}

#[test]
fn each_command_gets_the_verdict_the_design_assigns() {
    let policy = interactive();
    for (name, command, expected_allow) in every_variant() {
        let verdict = policy.evaluate(&command);
        assert_eq!(
            verdict.is_allowed(),
            expected_allow,
            "{name}: expected allowed={expected_allow}, got {verdict:?}"
        );
        if !expected_allow {
            assert_eq!(
                verdict.code(),
                Some("command_not_allowed_remote"),
                "{name} should be refused with the command-level code"
            );
        }
    }
}

/// Uploads have exactly one remote path — the attachment RPC — so the inline
/// data command stays refused and there is no second way in.
#[test]
fn inline_attachment_data_is_refused_remotely() {
    let verdict = interactive().evaluate(&ClientCommand::AddAttachmentData {
        session_id: session(),
        name: "a.png".to_string(),
        data_base64: "AA==".to_string(),
    });
    assert!(!verdict.is_allowed());
    assert_eq!(verdict.code(), Some("command_not_allowed_remote"));
}

/// `ApproveAlways` persists a rule into the repository's `permissions.yaml`, so
/// a phone must not be able to issue one — the other three decisions are fine.
#[test]
fn approve_always_is_refused_while_other_decisions_pass() {
    let policy = interactive();

    for decision in [
        ApprovalDecision::ApproveOnce,
        ApprovalDecision::ApproveSession,
        ApprovalDecision::Deny,
    ] {
        assert!(
            policy
                .evaluate(&ClientCommand::ApprovalDecision {
                    request_id: ApprovalId::new("a1"),
                    decision,
                })
                .is_allowed(),
            "{decision:?} should be allowed remotely"
        );
    }

    let verdict = policy.evaluate(&ClientCommand::ApprovalDecision {
        request_id: ApprovalId::new("a1"),
        decision: ApprovalDecision::ApproveAlways,
    });
    assert!(!verdict.is_allowed());
    assert_eq!(
        verdict.code(),
        Some("approval_decision_not_allowed_remote"),
        "the refusal must name the nested decision, not the command"
    );
}

/// `FullAccess` disarms the approval prompt, so it needs a local opt-in that a
/// remote client cannot perform for itself.
#[test]
fn full_access_needs_a_local_opt_in() {
    let policy = interactive();

    for mode in [
        PermissionProfile::RequestApproval,
        PermissionProfile::Assisted,
    ] {
        assert!(
            policy
                .evaluate(&ClientCommand::SetPermissionProfile {
                    session_id: session(),
                    mode,
                })
                .is_allowed(),
            "{mode:?} should be allowed remotely"
        );
    }

    let verdict = policy.evaluate(&ClientCommand::SetPermissionProfile {
        session_id: session(),
        mode: PermissionProfile::FullAccess,
    });
    assert!(!verdict.is_allowed());
    assert_eq!(
        verdict.code(),
        Some("permission_profile_not_allowed_remote")
    );

    let opted_in = RemotePolicy {
        scope: PairingScope::Interactive,
        allow_full_access: true,
    };
    assert!(
        opted_in
            .evaluate(&ClientCommand::SetPermissionProfile {
                session_id: session(),
                mode: PermissionProfile::FullAccess,
            })
            .is_allowed(),
        "the local opt-in should permit it"
    );
}

/// An `observe` pairing receives events and snapshots and issues nothing. Every
/// command is refused, including ones an interactive pairing may send.
#[test]
fn observe_scope_refuses_every_command() {
    let policy = RemotePolicy {
        scope: PairingScope::Observe,
        allow_full_access: true, // must not rescue anything
    };

    for (name, command, allowed_when_interactive) in every_variant() {
        let verdict = policy.evaluate(&command);
        assert!(
            !verdict.is_allowed(),
            "{name} must be refused under observe (interactive verdict was allowed={allowed_when_interactive})"
        );
    }

    assert_eq!(
        policy.evaluate(&ClientCommand::RequestSessionList).code(),
        Some("command_not_allowed_remote")
    );
}
