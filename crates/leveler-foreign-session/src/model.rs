//! Source-neutral values returned by the adapters.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The foreign agent whose store a session lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ForeignTool {
    /// Anthropic Claude Code's local JSONL transcript store.
    ClaudeCode,
    /// OpenAI Codex's local rollout JSONL store.
    Codex,
}

impl ForeignTool {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude Code",
            Self::Codex => "Codex",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "claude-code" | "claude" => Some(Self::ClaudeCode),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }

    pub(crate) fn default_dir_name(self) -> &'static str {
        match self {
            Self::ClaudeCode => ".claude",
            Self::Codex => ".codex",
        }
    }
}

/// What a picker needs to describe one foreign session without opening it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignSessionSummary {
    pub tool: ForeignTool,
    pub native_id: String,
    /// Store-derived display title. Never empty.
    pub title: String,
    pub cwd: Option<PathBuf>,
    pub branch: Option<String>,
    /// RFC 3339 UTC, derived from the file's modification time.
    pub updated_at: String,
    /// The exact file that was read. The history tool may only touch this.
    pub source_path: PathBuf,
    pub size_bytes: u64,
}

/// Why a foreign session could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ForeignSessionError {
    #[error("会话 ID 无效: {id}")]
    InvalidId { id: String },
    #[error("{tool} 中找不到会话 {id}")]
    NotFound { tool: &'static str, id: String },
    #[error("会话存储目录不可用: {path}")]
    RootUnavailable { path: PathBuf },
    #[error("无权读取会话文件: {path}")]
    Permission { path: PathBuf },
    #[error("读取会话文件失败: {path}: {detail}")]
    Io { path: PathBuf, detail: String },
    #[error("会话文件已损坏: {path}: {detail}")]
    Corrupt { path: PathBuf, detail: String },
    #[error("不支持的会话格式: {path}: {detail}")]
    Incompatible { path: PathBuf, detail: String },
    #[error("不支持的会话存储格式: {path}: {detail}")]
    UnsupportedFormat { path: PathBuf, detail: String },
}

impl ForeignSessionError {
    /// A short, user-facing category for the TUI's error line.
    pub fn category(&self) -> &'static str {
        match self {
            Self::InvalidId { .. } => "invalid-id",
            Self::NotFound { .. } => "not-found",
            Self::RootUnavailable { .. } => "root-unavailable",
            Self::Permission { .. } => "permission",
            Self::Io { .. } => "io",
            Self::Corrupt { .. } => "corrupt",
            Self::Incompatible { .. } => "incompatible",
            Self::UnsupportedFormat { .. } => "unsupported-format",
        }
    }
}

/// The role of one indexable event, without its body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForeignEventKind {
    User,
    Assistant,
    ToolCall,
    ToolResult,
    System,
}

impl ForeignEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::ToolCall => "tool_call",
            Self::ToolResult => "tool_result",
            Self::System => "system",
        }
    }
}

/// One index row: stable ordinal, source line, kind, and a bounded preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignEventRef {
    /// 1-based ordinal among meaningful events, stable while the file is.
    pub index: u32,
    /// 1-based source line.
    pub line: u64,
    pub kind: ForeignEventKind,
    pub preview: String,
    /// Number of bytes in the event body before preview truncation.
    pub bytes: usize,
    pub timestamp: Option<String>,
    /// A sub-agent sidechain record, not the main conversation.
    pub sidechain: bool,
}

/// Full text for one event, bounded per event and per call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignEventDetail {
    pub index: u32,
    pub line: u64,
    pub kind: ForeignEventKind,
    pub text: String,
    pub truncated: bool,
    pub timestamp: Option<String>,
    pub sidechain: bool,
    pub is_error: bool,
    pub call_id: Option<String>,
    pub tool_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignEventSearchHit {
    pub event: ForeignEventRef,
    pub snippet: String,
}

/// Aggregate counts, so a brief can state its evidence base.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ForeignSessionStats {
    pub records: usize,
    pub malformed_lines: usize,
    pub skipped_records: usize,
    pub user_messages: usize,
    pub assistant_messages: usize,
    pub tool_calls: usize,
    pub tool_results: usize,
    pub errors: usize,
    pub sidechain_events: usize,
    pub compactions: usize,
    pub line_truncated: usize,
}

/// A tool call with no matching result in the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignOpenCall {
    pub call_id: String,
    pub name: String,
    /// 1-based index of the call event.
    pub index: u32,
}

/// The whole index for one session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignSessionIndex {
    pub summary: ForeignSessionSummary,
    pub events: Vec<ForeignEventRef>,
    pub stats: ForeignSessionStats,
    pub warnings: Vec<String>,
    /// The event cap was reached; later records were not indexed.
    pub truncated: bool,
}

/// The bounded continuation digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignSessionDigest {
    pub summary: ForeignSessionSummary,
    pub stats: ForeignSessionStats,
    pub warnings: Vec<String>,
    /// First genuine user request, verbatim but bounded.
    pub first_user_message: Option<String>,
    /// Early user follow-ups, bounded, in source order.
    pub early_user_messages: Vec<String>,
    /// Last assistant text observed; a claim, never a verified fact.
    pub last_assistant_message: Option<String>,
    /// Last few events of any kind.
    pub recent: Vec<ForeignEventRef>,
    /// Calls with no recorded result at parse time.
    pub open_tool_calls: Vec<ForeignOpenCall>,
    /// Last error-looking tool result or system error.
    pub last_error: Option<String>,
    /// The source recorded an aborted turn.
    pub turn_aborted: bool,
}
