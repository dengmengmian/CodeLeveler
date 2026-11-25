//! Credentials the user hands to the runtime instead of to the model.
//!
//! A client that recognized (or was explicitly told) that a piece of text is a
//! credential sends it through [`SecretCapture`] beside the message, with a
//! placeholder standing in for the value in the message text. The runtime
//! replaces each placeholder with an opaque `[secret:sec_…]` token before the
//! message is persisted or shown to a model.
//!
//! The wire type deliberately carries no `Display`, and its [`Debug`] prints a
//! redaction marker: a command is logged, echoed in an error, or captured in a
//! tracing span far more often than anyone expects, and none of those paths may
//! reproduce the value.

use serde::{Deserialize, Serialize};

/// A credential value in transit from the client that owns it to the runtime
/// vault. The only way to read it is [`Self::expose`], which exists for the
/// capture path alone.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SecretValue(String);

impl SecretValue {
    /// Wrap a value for one capture.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Read the value. Callers are limited to the vault capture path.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Byte length, for bounds checks that must not read the value.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether this value is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for SecretValue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SecretValue(••••)")
    }
}

/// One value a client is handing over, and the placeholder it occupies in the
/// message text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SecretCapture {
    /// A client-chosen marker that appears exactly once in `content`.
    pub placeholder: String,
    /// The credential value.
    pub value: SecretValue,
    /// The field or header name it was found under, when the client knows it.
    /// A name only — never the value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_hint: Option<String>,
}

impl SecretCapture {
    /// Build a capture for `placeholder`.
    pub fn new(placeholder: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            placeholder: placeholder.into(),
            value: SecretValue::new(value),
            key_hint: None,
        }
    }

    /// Attach the field/header name the credential was found under.
    pub fn with_key_hint(mut self, hint: Option<String>) -> Self {
        self.key_hint = hint;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_prints_the_value() {
        let capture =
            SecretCapture::new("{{a}}", "hunter2-abc123").with_key_hint(Some("API_KEY".into()));
        let debug = format!("{capture:?}");
        assert!(!debug.contains("hunter2-abc123"), "{debug}");
        assert!(debug.contains("••••"), "{debug}");
        assert!(debug.contains("API_KEY"), "{debug}");
    }

    #[test]
    fn the_wire_carries_the_value_and_a_command_debug_does_not() {
        let capture = SecretCapture::new("{{a}}", "hunter2-abc123");
        let json = serde_json::to_string(&capture).unwrap();
        assert!(json.contains("hunter2-abc123"), "{json}");
        let decoded: SecretCapture = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.value.expose(), "hunter2-abc123");

        let command = crate::ClientCommand::CaptureSecret {
            session_id: crate::SessionId::new("s"),
            request_id: crate::CommandId::new("c"),
            value: SecretValue::new("hunter2-abc123"),
            key_hint: None,
        };
        let debug = format!("{command:?}");
        assert!(!debug.contains("hunter2-abc123"), "{debug}");
    }
}
