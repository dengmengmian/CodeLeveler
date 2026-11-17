//! MCP management over the client protocol.
//!
//! The runtime is the single authority for MCP server connection state and the
//! discovered tool catalog. This module is the typed request/response shape a
//! client uses to read that authority and to perform the management actions the
//! runtime owns — add, edit, remove, enable/disable, reconnect and reload.
//!
//! Configuration is only one input: a server listed here may be disabled, may
//! never have been attempted, or may have failed. The state enum is the only
//! thing a client may present as "connected"; a configured server is not a
//! connection.
//!
//! Connection errors cross the wire already redacted: a reason names what
//! failed (spawn, handshake, tools/list) but never carries an environment value,
//! header value or bearer token.

use serde::{Deserialize, Serialize};

/// What the runtime currently knows about one configured server.
///
/// `NotConnected` is a fact, not a failure: the runtime connects on a turn
/// boundary or on an explicit reconnect. `Unknown` means the runtime cannot
/// answer at all (for example, it has no workspace and therefore cannot launch
/// a stdio server).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum McpConnectionState {
    /// The user disabled this server in configuration.
    Disabled,
    /// Configured and enabled; no connection attempt has completed.
    NotConnected,
    /// An attempt is in progress. The answer is not final.
    Connecting,
    /// The handshake and tools/list completed.
    Connected,
    /// A previously connected server can no longer serve its catalog (for
    /// example, its child process exited). It is not connected now.
    Degraded,
    /// The last attempt failed. The reason is in `last_error`.
    Failed,
    /// The runtime cannot observe this server's state.
    Unknown,
}

impl McpConnectionState {
    /// Whether this state claims a live connection right now.
    pub fn is_connected(self) -> bool {
        matches!(self, Self::Connected)
    }

    /// Whether an answer exists at all (as opposed to "not observed").
    pub fn is_observed(self) -> bool {
        !matches!(self, Self::Unknown)
    }
}

/// The transport a server uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum McpTransport {
    Stdio,
    StreamableHttp,
}

impl McpTransport {
    pub fn label(self) -> &'static str {
        match self {
            Self::Stdio => "stdio",
            Self::StreamableHttp => "streamable_http",
        }
    }
}

/// How a discovered tool relates to the current session's admission rules.
///
/// This is a projection of the live policy, never a permission grant. The
/// execution boundary still decides on each call; a tool advertised here can
/// still be refused by a later policy change or a per-call approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum McpToolAdmission {
    /// The session's profile admits network-risk tools.
    Admitted,
    /// The session's profile routes risky calls to approval.
    RequiresApproval,
    /// The session profile denies network access by default; the call needs an
    /// explicit network grant.
    RequiresNetworkGrant,
    /// The runtime cannot decide from the current facts.
    Unknown,
}

/// One tool a connected server advertised to the runtime.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct McpToolView {
    /// The name the server itself uses.
    pub remote_name: String,
    /// The model-facing name: `mcp__<server>__<tool>`.
    pub exposed_name: String,
    /// The server's own description, bounded by the producer.
    pub description: String,
    /// JSON Schema for the arguments, bounded by the producer.
    pub input_schema: serde_json::Value,
    /// True when the producer truncated the description or schema to keep the
    /// catalog bounded. The full value is not available to a client.
    #[serde(default)]
    pub truncated: bool,
    /// The tool's risk classification; MCP tools are always network-risk.
    pub risk: String,
    pub admission: McpToolAdmission,
}

/// One configured server and its live connection facts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct McpServerView {
    pub name: String,
    pub transport: McpTransport,
    /// A display-safe endpoint: the command for stdio, the URL without
    /// credentials/query for HTTP. Never an environment or header value.
    pub endpoint: String,
    /// The configuration file this entry came from, for display only.
    pub source: String,
    /// The declaration as configured, so an edit form starts from the real
    /// values instead of parsing a display string. Contains name references,
    /// never resolved secrets.
    pub spec: McpServerSpec,
    pub enabled: bool,
    pub state: McpConnectionState,
    /// The protocol version the server negotiated, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    /// Whether the initialize handshake completed.
    #[serde(default)]
    pub initialized: bool,
    /// How many tools the last successful discovery produced. `None` when no
    /// catalog was ever discovered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_count: Option<usize>,
    /// A bounded catalog for the selected server. Other servers in a list
    /// answer carry no tools; a client asks for a detail view explicitly.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<McpToolView>,
    /// True when the runtime bounded the catalog it returned. `tool_count`
    /// remains the authoritative count.
    #[serde(default)]
    pub tools_truncated: bool,
    /// Redacted reason from the last failed attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// RFC3339 timestamps from the runtime clock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_attempt_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connected_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    /// Monotonic connection generation. A later answer with a smaller
    /// generation is stale and must not overwrite this one.
    pub generation: u64,
    /// Seconds until the next automatic retry, when the runtime scheduled one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_in_seconds: Option<u64>,
}

/// One environment/header name reference in a server specification.
///
/// `source` is an environment variable name, never a value. The runtime
/// resolves it at launch; a client must not send a literal secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct McpNameRef {
    pub key: String,
    pub source: String,
}

/// A complete server declaration for add/update.
///
/// The fields are the configuration vocabulary, not a resolved connection: the
/// runtime validates names and references before writing the file. Unknown
/// transport fields are refused rather than silently ignored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct McpServerSpec {
    pub name: String,
    pub transport: McpTransport,
    /// Stdio launch program.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    /// Destination environment key → source environment variable name.
    #[serde(default)]
    pub env: Vec<McpNameRef>,
    /// Streamable HTTP endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Header name → source environment variable name. `Authorization` is
    /// refused here; use `bearer_token_env`.
    #[serde(default)]
    pub headers: Vec<McpNameRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer_token_env: Option<String>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool {
    true
}

/// One management request. Every action answers with the same authoritative
/// list, so a client cannot merge a partial update into stale rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum McpRequest {
    /// Read the current authority. Never connects a server.
    List,
    /// Connect one configured server again, replacing its current connection.
    Reconnect {
        server: String,
    },
    /// Re-read the configuration sources and apply the result to this runtime.
    Reload,
    Add {
        spec: McpServerSpec,
    },
    /// Replace the server named `original_name`. Renaming must not collide.
    Update {
        original_name: String,
        spec: McpServerSpec,
    },
    Remove {
        server: String,
    },
    SetEnabled {
        server: String,
        enabled: bool,
    },
}

/// The answer to one [`McpRequest`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum McpResponse {
    /// A read-only answer.
    Snapshot { servers: Vec<McpServerView> },
    /// A management action changed the authority (or configuration) and this
    /// is the resulting list. `message` is a machine-free summary; the client
    /// localizes its own success wording from the action it sent.
    Changed {
        servers: Vec<McpServerView>,
        message: String,
    },
    /// The action failed. `servers` still carries the latest authority, so a
    /// failed write never destroys the page's rows.
    Error {
        servers: Vec<McpServerView>,
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_configured_server_is_not_connected_by_default() {
        assert!(!McpConnectionState::NotConnected.is_connected());
        assert!(McpConnectionState::Connected.is_connected());
        assert!(!McpConnectionState::Failed.is_connected());
        assert!(!McpConnectionState::Unknown.is_observed());
        assert!(McpConnectionState::Disabled.is_observed());
    }

    #[test]
    fn requests_roundtrip_with_a_stable_tag() {
        let add = McpRequest::Add {
            spec: McpServerSpec {
                name: "fs".into(),
                transport: McpTransport::Stdio,
                command: Some("npx".into()),
                args: vec!["-y".into(), "server".into()],
                env: vec![McpNameRef {
                    key: "GITHUB_TOKEN".into(),
                    source: "GITHUB_TOKEN".into(),
                }],
                url: None,
                headers: Vec::new(),
                bearer_token_env: None,
                enabled: true,
            },
        };
        let json = serde_json::to_string(&add).unwrap();
        assert!(json.contains("\"action\":\"add\""), "{json}");
        assert!(json.contains("\"transport\":\"stdio\""), "{json}");
        assert!(!json.contains("bearer_token_env"), "{json}");
        let back: McpRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, add);
    }

    #[test]
    fn a_response_tag_is_status_not_action() {
        let response = McpResponse::Snapshot {
            servers: Vec::new(),
        };
        let json = serde_json::to_string(&response).unwrap();
        assert_eq!(json, r#"{"status":"snapshot","servers":[]}"#);
    }

    #[test]
    fn a_tool_view_never_carries_an_environment_value() {
        let view = McpToolView {
            remote_name: "echo".into(),
            exposed_name: "mcp__test__echo".into(),
            description: "echo".into(),
            input_schema: serde_json::json!({"type": "object"}),
            truncated: false,
            risk: "network".into(),
            admission: McpToolAdmission::Admitted,
        };
        let json = serde_json::to_string(&view).unwrap();
        assert!(!json.contains("TOKEN"));
        assert!(json.contains("\"admission\":\"admitted\""));
    }
}
