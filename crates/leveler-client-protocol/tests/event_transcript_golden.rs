//! A real conversation, serialised from the real types, for other languages to
//! replay.
//!
//! The mobile app turns `RuntimeEvent`s into a transcript, and it was written
//! against field names I guessed at rather than read: `content` where the type
//! says `text`, a flattened pending interaction where the type nests it under
//! `request`. Both compiled, both passed their own tests, and both rendered
//! empty bubbles on a device.
//!
//! Schemas alone did not catch that — a schema says what *may* appear, not what
//! a session actually looks like. This writes one concrete session: a snapshot
//! and the exact event sequence a turn produces, from the Rust types, so a
//! client that mis-reads a field fails a test instead of a user.
//!
//! Regenerate after a protocol change with:
//!
//! ```text
//! UPDATE_GOLDEN=1 cargo test -p leveler-client-protocol --test event_transcript_golden
//! ```

use std::path::{Path, PathBuf};

use leveler_client_protocol::{
    ApprovalId, MessageId, NotificationLevel, PermissionProfile, RuntimeEvent, SessionId,
    UiApprovalRequest, UiMessage, UiPendingInteraction, UiRole, UiSessionSnapshot,
};

fn golden_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("testdata")
        .join("session_transcript.golden.json")
}

fn snapshot() -> UiSessionSnapshot {
    UiSessionSnapshot {
        id: SessionId::new("s_golden"),
        repository: "/repo".to_string(),
        goal: "修一个 bug".to_string(),
        model: None,
        mode: PermissionProfile::RequestApproval,
        branch: Some("main".to_string()),
        status: "idle".to_string(),
        messages: vec![
            UiMessage {
                id: MessageId::new("m_user_1"),
                role: UiRole::User,
                text: "第一句".to_string(),
            },
            UiMessage {
                id: MessageId::new("m_assistant_1"),
                role: UiRole::Assistant,
                text: "第一句的回答".to_string(),
            },
        ],
        pending_interactions: vec![UiPendingInteraction::Approval(UiApprovalRequest {
            id: ApprovalId::new("a_pending"),
            tool: "run_command".to_string(),
            summary: "删除 build 目录".to_string(),
            command: Some("rm -rf build".to_string()),
            risks: vec!["会删除文件".to_string()],
        })],
        available_models: Vec::new(),
        vision: false,
        last_sequence: None,
        active_tools: Vec::new(),
        plan: None,
        verification: None,
        diff: None,
        checkpoints: Vec::new(),
        user_shells: Vec::new(),
        completion_report: None,
        reasoning: None,
    }
}

/// One turn, in the order a runtime emits it — including the kinds a chat UI
/// has no use for. Those are the ones that broke the app: it treated every
/// unrecognised kind as a reason to resynchronise, so an ordinary turn put the
/// phone in a permanent "reconnecting" state.
fn turn() -> Vec<RuntimeEvent> {
    vec![
        RuntimeEvent::UserMessageAdded {
            message: UiMessage {
                id: MessageId::new("m_user_2"),
                role: UiRole::User,
                text: "1+1 等于几".to_string(),
            },
        },
        RuntimeEvent::AgentActivity {
            label: "思考中".to_string(),
        },
        RuntimeEvent::AssistantMessageStarted {
            message_id: MessageId::new("m_assistant_2"),
        },
        RuntimeEvent::AssistantTextDelta {
            message_id: MessageId::new("m_assistant_2"),
            delta: "等于 ".to_string(),
        },
        RuntimeEvent::ReasoningDelta {
            delta: "（模型的推理，不该出现在对话里）".to_string(),
        },
        RuntimeEvent::TokenUsage {
            input_tokens: 12,
            output_tokens: 3,
            cached_input_tokens: 0,
        },
        RuntimeEvent::AssistantTextDelta {
            message_id: MessageId::new("m_assistant_2"),
            delta: "2".to_string(),
        },
        RuntimeEvent::AssistantMessageCompleted {
            message_id: MessageId::new("m_assistant_2"),
        },
        RuntimeEvent::Notification {
            level: NotificationLevel::Info,
            message: "顺带一提".to_string(),
        },
        RuntimeEvent::TurnCompleted,
    ]
}

#[test]
fn the_transcript_golden_matches_the_types() {
    let document = serde_json::json!({
        "note": "One session as the runtime really emits it. Replayed by \
                 apps/leveler-mobile/test/transcript_golden_test.dart; a client that \
                 mis-reads a field fails there instead of showing a user an empty bubble.",
        "snapshot": snapshot(),
        "turn": turn(),
        "expected": {
            "transcript_after_snapshot": [
                {"role": "user", "text": "第一句"},
                {"role": "assistant", "text": "第一句的回答"},
            ],
            "pending_approvals_after_snapshot": ["a_pending"],
            "transcript_after_turn": [
                {"role": "user", "text": "第一句"},
                {"role": "assistant", "text": "第一句的回答"},
                {"role": "user", "text": "1+1 等于几"},
                {"role": "assistant", "text": "等于 2"},
                // A notification is the host speaking, not the assistant. It
                // belongs in the transcript as an aside — dropping it, which is
                // what the mobile client used to do, loses the one channel the
                // host has for telling a user something outside an answer.
                {"role": "notice", "text": "顺带一提"},
            ],
            // The point of the whole file: an ordinary turn must not leave a
            // client believing its view is stale.
            "needs_resync_after_turn": false,
        },
    });
    let mut rendered = serde_json::to_string_pretty(&document).expect("golden serialises");
    rendered.push('\n');

    let path = golden_path();
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().expect("testdata has a parent"))
            .expect("create testdata");
        std::fs::write(&path, rendered).expect("write golden");
        return;
    }

    let current = std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "{} could not be read ({error}). Regenerate with:\n  \
             UPDATE_GOLDEN=1 cargo test -p leveler-client-protocol --test event_transcript_golden",
            path.display()
        )
    });
    assert_eq!(
        current, rendered,
        "the event shapes changed; regenerate and update the mobile client:\n  \
         UPDATE_GOLDEN=1 cargo test -p leveler-client-protocol --test event_transcript_golden"
    );
}
