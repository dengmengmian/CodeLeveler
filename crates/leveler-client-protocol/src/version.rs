//! Protocol versioning and the wire envelope (M6).
//!
//! In-process and local-socket clients use the same validation boundary as a
//! future cloud transport: a top-level version so an old client and a new runtime
//! (or vice-versa) fail loudly on an incompatible **major** rather than
//! mis-parsing. Minor bumps are additive (new event variants, new optional
//! fields) and stay compatible — existing golden fixtures keep deserializing.

use serde::{Deserialize, Serialize};

/// The protocol version this build speaks. Bump `minor` for additive changes,
/// `major` for a breaking wire change.
pub const PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion { major: 1, minor: 3 };

/// A semantic-ish protocol version. Same `major` = compatible; `minor` is
/// forward/backward compatible within a major.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
}

impl ProtocolVersion {
    pub const fn current() -> Self {
        PROTOCOL_VERSION
    }

    /// Whether this build can understand a peer speaking `other`.
    pub fn is_compatible_with(self, other: ProtocolVersion) -> bool {
        self.major == other.major
    }
}

/// Errors decoding a protocol envelope.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProtocolError {
    #[error("incompatible protocol major version: peer speaks {got}, this build speaks {ours}")]
    IncompatibleMajor { got: u16, ours: u16 },
}

/// A versioned wire envelope around any protocol body (command, event, or
/// snapshot). Local command issuance and future remote transports both validate
/// this envelope before dispatch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolEnvelope<T> {
    pub protocol: ProtocolVersion,
    pub body: T,
}

impl<T> ProtocolEnvelope<T> {
    /// Wrap a body with this build's protocol version.
    pub fn wrap(body: T) -> Self {
        Self {
            protocol: PROTOCOL_VERSION,
            body,
        }
    }

    /// Take the body, rejecting a peer whose major version this build cannot
    /// understand — never mis-parse across a breaking boundary.
    pub fn into_body(self) -> Result<T, ProtocolError> {
        if !PROTOCOL_VERSION.is_compatible_with(self.protocol) {
            return Err(ProtocolError::IncompatibleMajor {
                got: self.protocol.major,
                ours: PROTOCOL_VERSION.major,
            });
        }
        Ok(self.body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ClientCommand;

    #[test]
    fn same_major_is_compatible_across_minor() {
        assert!(PROTOCOL_VERSION.is_compatible_with(ProtocolVersion {
            major: 1,
            minor: 99
        }));
        assert!(!PROTOCOL_VERSION.is_compatible_with(ProtocolVersion { major: 2, minor: 0 }));
    }

    #[test]
    fn unknown_major_is_rejected_not_misparsed() {
        let env = ProtocolEnvelope {
            protocol: ProtocolVersion { major: 2, minor: 0 },
            body: ClientCommand::Quit,
        };
        assert_eq!(
            env.into_body().unwrap_err(),
            ProtocolError::IncompatibleMajor { got: 2, ours: 1 }
        );
    }

    #[test]
    fn wrap_stamps_current_version_and_unwraps() {
        let env = ProtocolEnvelope::wrap(ClientCommand::Quit);
        assert_eq!(env.protocol, PROTOCOL_VERSION);
        assert!(matches!(env.into_body(), Ok(ClientCommand::Quit)));
    }

    #[test]
    fn envelope_wire_format_is_stable() {
        // Golden fixture: an envelope's JSON shape must not drift within a major.
        let env = ProtocolEnvelope::wrap(ClientCommand::Quit);
        let json = serde_json::to_string(&env).unwrap();
        assert_eq!(
            json,
            r#"{"protocol":{"major":1,"minor":3},"body":{"type":"quit"}}"#
        );
        // And it round-trips.
        let back: ProtocolEnvelope<ClientCommand> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, env);
    }
}
