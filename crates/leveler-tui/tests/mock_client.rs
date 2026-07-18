//! Integration test of the client seam via the mock runtime (§69.6): commands
//! are recorded, events are broadcast to subscribers.

use std::sync::Arc;

use leveler_client_protocol::mock::MockRuntimeClient;
use leveler_client_protocol::{
    ClientCommand, InteractiveRuntimeClient, MessageId, RuntimeEvent, SessionId, UiMessage, UiRole,
};

#[tokio::test]
async fn records_commands_and_broadcasts_events() {
    let client = Arc::new(MockRuntimeClient::new(SessionId::new("s1")));
    let mut rx = client.subscribe();

    client
        .send(ClientCommand::SubmitMessage {
            session_id: SessionId::new("s1"),
            content: "hi".into(),
            attachments: Vec::new(),
        })
        .await
        .unwrap();
    assert_eq!(client.commands().len(), 1);

    client.emit(RuntimeEvent::UserMessageAdded {
        message: UiMessage {
            id: MessageId::new("u1"),
            role: UiRole::User,
            text: "hi".into(),
        },
    });
    let event = rx.recv().await.unwrap();
    assert!(matches!(event, RuntimeEvent::UserMessageAdded { .. }));
}

#[tokio::test]
async fn snapshot_returns_seeded_session() {
    let client = MockRuntimeClient::new(SessionId::new("s1"));
    let snap = client.snapshot(&SessionId::new("s1")).await.unwrap();
    assert_eq!(snap.id, SessionId::new("s1"));
}
