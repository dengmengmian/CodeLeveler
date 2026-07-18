//! Typed identifiers.
//!
//! Every subsystem gets its own newtype so an `ArtifactId` can never be passed
//! where a `SessionId` is expected. All wrap a string (UUID today, but the
//! representation is intentionally opaque so it can change later).

use std::fmt;

use serde::{Deserialize, Serialize};

/// Generate a fresh random v4 UUID rendered as a lowercase hyphenated string.
pub fn new_uuid_string() -> String {
    uuid::Uuid::new_v4().to_string()
}

macro_rules! string_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub struct $name(String);

        impl $name {
 /// Wrap an existing identifier string.
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

 /// Generate a fresh random identifier.
            pub fn generate() -> Self {
                Self(new_uuid_string())
            }

 /// Borrow the underlying string.
            pub fn as_str(&self) -> &str {
                &self.0
            }

 /// Consume into the underlying string.
            pub fn into_inner(self) -> String {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }
    };
}

string_id!(
 /// Identifies a single agent session (one user goal end to end).
    SessionId
);
string_id!(
 /// Identifies one turn within a session.
    TurnId
);
string_id!(
 /// Identifies a single model request.
    RequestId
);
string_id!(
 /// Identifies a tool call. Must be stable across streaming reassembly.
    ToolCallId
);
string_id!(
 /// Identifies a stored artifact (large tool output, diff, report, ...).
    ArtifactId
);
string_id!(
 /// Identifies a pending permission approval request.
    ApprovalId
);
string_id!(
 /// Identifies a pending clarification (ask-user) request.
    ClarificationId
);
string_id!(
 /// Identifies a conversation checkpoint (restore point).
    CheckpointId
);
string_id!(
 /// Identifies one persisted engine event (append-only event log).
    EventId
);
string_id!(
 /// Identifies a client command, used as an idempotency key: a command may be
 /// delivered more than once (at-least-once), so the same id must not run the
 /// action twice.
    CommandId
);
string_id!(
 /// Identifies a task graph.
    TaskId
);
string_id!(
 /// Identifies a single node within a task graph.
    TaskNodeId
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_distinct_types_but_roundtrip_strings() {
        let s = SessionId::new("abc");
        assert_eq!(s.as_str(), "abc");
        assert_eq!(s.to_string(), "abc");
        assert_eq!(s.clone().into_inner(), "abc");
    }

    #[test]
    fn generate_produces_unique_values() {
        let a = RequestId::generate();
        let b = RequestId::generate();
        assert_ne!(a, b);
    }

    #[test]
    fn serde_roundtrip_is_transparent_string() {
        let id = ToolCallId::new("call_42");
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"call_42\"");
        let back: ToolCallId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }
}
