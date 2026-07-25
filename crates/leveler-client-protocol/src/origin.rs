//! Where a command came from.
//!
//! The runtime treats a local terminal and a paired phone identically once a
//! command is admitted — the same `deliver_protocol` path, the same idempotency
//! receipts. Origin is what lets the layers *around* that path tell them apart:
//! the audit log records who issued a command, and the approval timeout only
//! applies pressure when nobody local can answer.
//!
//! It is not an authorization decision. The remote capability gate has already
//! run by the time a command carries `Remote`; origin describes provenance, and
//! nothing should grant privilege by inspecting it.

use serde::{Deserialize, Serialize};

/// Who issued a command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ClientOrigin {
    /// A UI on this machine: the TUI, or the loopback web client.
    Local,
    /// A paired remote device, named by the id the host accepted at pairing.
    Remote { device_id: String },
    /// The host itself acted because a policy fired rather than because a
    /// person chose — today, a remote-only approval that timed out.
    ///
    /// Distinct from `Remote` on purpose: the audit trail should not read as
    /// though the phone denied something its user never saw.
    RemoteTimeout { device_id: String },
}

impl ClientOrigin {
    /// Whether this origin came from off the machine.
    pub fn is_remote(&self) -> bool {
        matches!(
            self,
            ClientOrigin::Remote { .. } | ClientOrigin::RemoteTimeout { .. }
        )
    }

    /// The device involved, when there is one.
    pub fn device_id(&self) -> Option<&str> {
        match self {
            ClientOrigin::Local => None,
            ClientOrigin::Remote { device_id } | ClientOrigin::RemoteTimeout { device_id } => {
                Some(device_id)
            }
        }
    }

    /// A short, stable label for audit lines and metrics.
    pub fn label(&self) -> &'static str {
        match self {
            ClientOrigin::Local => "local",
            ClientOrigin::Remote { .. } => "remote",
            ClientOrigin::RemoteTimeout { .. } => "remote_timeout",
        }
    }
}

impl Default for ClientOrigin {
    /// Local. An origin that was never set describes an in-process caller, and
    /// defaulting the other way would mislabel local activity as remote.
    fn default() -> Self {
        ClientOrigin::Local
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins_serialize_with_a_kind_tag() {
        assert_eq!(
            serde_json::to_string(&ClientOrigin::Local).unwrap(),
            r#"{"kind":"local"}"#
        );
        assert_eq!(
            serde_json::to_string(&ClientOrigin::Remote {
                device_id: "dev_1".to_string()
            })
            .unwrap(),
            r#"{"kind":"remote","device_id":"dev_1"}"#
        );
        assert_eq!(
            serde_json::to_string(&ClientOrigin::RemoteTimeout {
                device_id: "dev_1".to_string()
            })
            .unwrap(),
            r#"{"kind":"remote_timeout","device_id":"dev_1"}"#
        );
    }

    /// A timeout-issued denial must not be attributable to the phone's user as
    /// though they made the call.
    #[test]
    fn a_timeout_is_distinguishable_from_a_human_remote_decision() {
        let human = ClientOrigin::Remote {
            device_id: "dev_1".to_string(),
        };
        let policy = ClientOrigin::RemoteTimeout {
            device_id: "dev_1".to_string(),
        };

        assert_ne!(human, policy);
        assert_ne!(human.label(), policy.label());
        assert_eq!(human.device_id(), policy.device_id());
        assert!(human.is_remote() && policy.is_remote());
    }

    #[test]
    fn the_default_origin_is_local() {
        assert_eq!(ClientOrigin::default(), ClientOrigin::Local);
        assert!(!ClientOrigin::default().is_remote());
        assert_eq!(ClientOrigin::Local.device_id(), None);
    }
}
