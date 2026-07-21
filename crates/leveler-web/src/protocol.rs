//! WebSocket wire DTOs: the browser ↔ server message shapes.
//!
//! These mirror the stable client protocol (commands in, events/snapshots out)
//! with WebSocket-specific framing: each upstream command delivery carries a
//! client-chosen `command_id` the server echoes in its `ack`, so the SPA can
//! match acknowledgements to queued messages. Golden fixtures pin the wire
//! shape — the TypeScript client depends on it byte for byte.

use serde::{Deserialize, Serialize};

use leveler_client_protocol::{ClientCommand, RuntimeEvent, UiSessionSnapshot};

use crate::projects::ProjectStatus;

/// A message the browser sends upstream over the WebSocket.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UpstreamMessage {
    /// Deliver a command to the runtime; the server answers with an `ack`
    /// (or an `error`) echoing `command_id`.
    Deliver {
        command_id: String,
        session_id: String,
        command: ClientCommand,
    },
    /// Ask for a fresh session snapshot (initial render / resync).
    Snapshot { session_id: String },
}

/// A message the server sends downstream over the WebSocket.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DownstreamMessage {
    /// A runtime event from the global event stream.
    Event { event: RuntimeEvent },
    /// A full session snapshot (on connect when `?session=` is given, on
    /// request, on resync).
    Snapshot { session: UiSessionSnapshot },
    /// A delivered command was accepted by the runtime.
    Ack { command_id: String },
    /// A frame could not be parsed, or a command was rejected. The connection
    /// stays open; `command_id` correlates the failure when known.
    Error {
        code: String,
        message: String,
        command_id: Option<String>,
    },
    /// A registered project's daemon changed state (aggregation mode only):
    /// the sidebar updates its status dot without re-fetching `/api/projects`.
    ProjectStatus { path: String, status: ProjectStatus },
    /// The event subscription lagged: the client must resync from a fresh
    /// snapshot. The server closes the connection right after this frame.
    ResyncRequired { session_id: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_client_protocol::{NotificationLevel, SessionId};

    #[test]
    fn upstream_deliver_matches_golden_fixture() {
        // Golden fixture: the upstream deliver shape the browser sends.
        let json = r#"{"type":"deliver","command_id":"cmd-1","session_id":"s1","command":{"type":"submit_message","session_id":"s1","content":"你好","attachments":[]}}"#;
        let message: UpstreamMessage = serde_json::from_str(json).unwrap();
        let UpstreamMessage::Deliver {
            command_id,
            session_id,
            command,
        } = message.clone()
        else {
            panic!("expected deliver, got {message:?}");
        };
        assert_eq!(command_id, "cmd-1");
        assert_eq!(session_id, "s1");
        assert!(matches!(command, ClientCommand::SubmitMessage { .. }));
        assert_eq!(serde_json::to_string(&message).unwrap(), json);
    }

    #[test]
    fn upstream_snapshot_matches_golden_fixture() {
        let json = r#"{"type":"snapshot","session_id":"s1"}"#;
        let message: UpstreamMessage = serde_json::from_str(json).unwrap();
        assert!(matches!(message, UpstreamMessage::Snapshot { .. }));
        assert_eq!(serde_json::to_string(&message).unwrap(), json);
    }

    #[test]
    fn downstream_event_matches_golden_fixture() {
        let frame = DownstreamMessage::Event {
            event: RuntimeEvent::Notification {
                level: NotificationLevel::Info,
                message: "hello".to_string(),
            },
        };
        let json = serde_json::to_string(&frame).unwrap();
        assert_eq!(
            json,
            r#"{"type":"event","event":{"type":"notification","level":"info","message":"hello"}}"#
        );
        let back: DownstreamMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, DownstreamMessage::Event { .. }));
    }

    #[test]
    fn downstream_ack_and_resync_match_golden_fixtures() {
        let ack = DownstreamMessage::Ack {
            command_id: "cmd-1".to_string(),
        };
        assert_eq!(
            serde_json::to_string(&ack).unwrap(),
            r#"{"type":"ack","command_id":"cmd-1"}"#
        );
        let resync = DownstreamMessage::ResyncRequired {
            session_id: "s1".to_string(),
        };
        assert_eq!(
            serde_json::to_string(&resync).unwrap(),
            r#"{"type":"resync_required","session_id":"s1"}"#
        );
    }

    #[test]
    fn downstream_project_status_matches_golden_fixture() {
        let frame = DownstreamMessage::ProjectStatus {
            path: "/Users/me/repo".to_string(),
            status: ProjectStatus::Offline,
        };
        assert_eq!(
            serde_json::to_string(&frame).unwrap(),
            r#"{"type":"project_status","path":"/Users/me/repo","status":"offline"}"#
        );
    }

    #[test]
    fn downstream_error_serializes_null_command_id() {
        let frame = DownstreamMessage::Error {
            code: "invalid_frame".to_string(),
            message: "bad JSON".to_string(),
            command_id: None,
        };
        assert_eq!(
            serde_json::to_string(&frame).unwrap(),
            r#"{"type":"error","code":"invalid_frame","message":"bad JSON","command_id":null}"#
        );
        let correlated = DownstreamMessage::Error {
            code: "runtime_error".to_string(),
            message: "boom".to_string(),
            command_id: Some("cmd-1".to_string()),
        };
        assert_eq!(
            serde_json::to_string(&correlated).unwrap(),
            r#"{"type":"error","code":"runtime_error","message":"boom","command_id":"cmd-1"}"#
        );
    }

    #[test]
    fn downstream_snapshot_roundtrips_with_tag() {
        let snapshot = UiSessionSnapshot {
            id: SessionId::new("s1"),
            repository: "/repo".to_string(),
            goal: "interactive session".to_string(),
            model: None,
            mode: leveler_client_protocol::PermissionProfile::Assisted,
            branch: None,
            status: "idle".to_string(),
            messages: Vec::new(),
            pending_interactions: Vec::new(),
            available_models: Vec::new(),
            vision: false,
            last_sequence: None,
            active_tools: Vec::new(),
            plan: None,
            verification: None,
            diff: None,
            checkpoints: Vec::new(),
            completion_report: None,
        };
        let json =
            serde_json::to_string(&DownstreamMessage::Snapshot { session: snapshot }).unwrap();
        assert!(json.starts_with(r#"{"type":"snapshot","session":{"id":"s1""#));
        let back: DownstreamMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, DownstreamMessage::Snapshot { .. }));
    }
}
