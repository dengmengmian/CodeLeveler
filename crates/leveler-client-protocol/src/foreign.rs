//! Cross-agent session import: local discovery and native continuation.
//!
//! A foreign session is read-only source evidence. Importing one creates a
//! normal CodeLeveler session that records where it came from; it never makes
//! the source agent's runtime the owner of the new conversation.

use serde::{Deserialize, Serialize};

/// The agent whose local session store is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum UiForeignTool {
    /// Anthropic Claude Code.
    ClaudeCode,
    /// OpenAI Codex.
    Codex,
}

impl UiForeignTool {
    /// Wire value used by persisted origin rows.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
        }
    }

    /// Product name shown to a person.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude Code",
            Self::Codex => "Codex",
        }
    }

    /// Parse a persisted wire value. `claude` is accepted because the crate's
    /// own model accepts it; the canonical value stays `claude-code`.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "claude-code" | "claude" => Some(Self::ClaudeCode),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }

    /// The slash command that opens this source's picker.
    pub fn slash_command(self) -> &'static str {
        match self {
            Self::ClaudeCode => "/resume-claude",
            Self::Codex => "/resume-codex",
        }
    }
}

/// One locally discovered source session, as a picker needs it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiForeignSession {
    pub tool: UiForeignTool,
    /// The source agent's own session id.
    pub native_id: String,
    pub title: String,
    /// Working directory recorded by the source session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// RFC 3339 UTC, derived from the source file.
    pub updated_at: String,
    pub size_bytes: u64,
}

/// The durable source identity of an imported CodeLeveler session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiForeignOrigin {
    pub tool: UiForeignTool,
    pub native_id: String,
    /// The exact source file the read-only history tool addresses. The client
    /// shows this as provenance; it never rewrites it.
    pub source_path: String,
    /// Working directory recorded by the source session, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_cwd: Option<String>,
    pub imported_at: String,
}
