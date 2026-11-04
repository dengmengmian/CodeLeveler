//! Typed desktop failures.
//!
//! A tool result must say which authority refused (`permission_denied`), which
//! session fact is missing (`no_interactive_session`), or which observation is
//! stale. It must never turn "I could not look" into "nothing changed".

use serde::{Deserialize, Serialize};

use crate::types::Bounds;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionKind {
    Accessibility,
    ScreenRecording,
    Automation,
    InputMonitoring,
}

impl PermissionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Accessibility => "accessibility",
            Self::ScreenRecording => "screen_recording",
            Self::Automation => "automation",
            Self::InputMonitoring => "input_monitoring",
        }
    }
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum CuaError {
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("no interactive desktop session: {0}")]
    NoInteractiveSession(String),
    #[error("permission denied: {kind} ({detail})", kind = kind.as_str())]
    PermissionDenied {
        kind: PermissionKind,
        detail: String,
    },
    #[error("application not found: {0}")]
    AppNotFound(String),
    #[error("window not found: {0}")]
    WindowNotFound(u64),
    #[error("unknown element reference `{0}`; observe the window again")]
    UnknownReference(String),
    #[error("stale element reference `{reference}`: {reason}")]
    StaleReference { reference: String, reason: String },
    #[error("element changed since it was observed: {0}")]
    ElementChanged(String),
    #[error(
        "window {window} moved or resized since it was observed (was {observed:?}, now {current:?}); observe it again"
    )]
    WindowMoved {
        window: u64,
        observed: Bounds,
        current: Bounds,
    },
    #[error("focus conflict: {target} is not focused (frontmost: {frontmost}); pass activate=true to bring it forward")]
    FocusConflict { target: String, frontmost: String },
    #[error("could not bring {0} to the front")]
    FocusUnavailable(String),
    #[error("elevation required: {0}")]
    ElevationRequired(String),
    #[error("operation timed out: {0}")]
    Timeout(String),
    #[error("cancelled")]
    Cancelled,
    #[error("{backend} backend: {detail}")]
    Backend {
        backend: &'static str,
        detail: String,
    },
    #[error("invalid action: {0}")]
    InvalidAction(String),
}

pub type CuaResult<T> = Result<T, CuaError>;
