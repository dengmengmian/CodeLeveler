//! Source-aware read projections of existing durable sessions.
use crate::SessionId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum UiTaskStatus {
    Idle,
    Running,
    WaitingUser,
    Answered,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
    Unknown,
    Blocked,
    Incomplete,
}

/// Latest committed TaskFinished fact. This does not prove a later turn settled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiTaskTerminal {
    pub sequence: i64,
    pub outcome: String,
    pub stop: Option<String>,
    pub reason: Option<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiGlobalTaskSummary {
    pub id: SessionId,
    pub source_id: String,
    pub title: String,
    pub primary_workspace: Option<String>,
    /// Availability is separate from lifecycle; a deleted checkout retains its history.
    pub workspace_available: Option<bool>,
    pub last_activity_at: String,
    pub archived_at: Option<String>,
    pub status: UiTaskStatus,
    pub model: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum UiGlobalTaskSourceErrorKind {
    Discovery,
    Storage,
    Projection,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiGlobalTaskSourceError {
    pub kind: UiGlobalTaskSourceErrorKind,
    pub source_id: String,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiGlobalTaskIndex {
    pub tasks: Vec<UiGlobalTaskSummary>,
    pub source_errors: Vec<UiGlobalTaskSourceError>,
}
