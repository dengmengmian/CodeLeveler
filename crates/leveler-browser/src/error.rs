//! The one error type every browser operation returns.
//!
//! Each variant is a distinct, mechanically distinguishable failure — a stale
//! ref is not a disconnected session is not an unsupported backend operation.
//! The `Display` text states the mechanical fact and nothing else: it never
//! tells the model what to do next (§33).

use std::fmt;

/// A structured browser failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserError {
    /// No usable browser: the selected product is not installed, or its
    /// automation prerequisite is not met. Carries the mechanical reason.
    /// Never a fallback to a different product (§34).
    Unavailable(String),
    /// The browser process was found but failed to start or to speak its
    /// protocol.
    LaunchFailed(String),
    /// The isolated project profile directory is unusable.
    ProfileUnavailable(String),
    /// The protocol connection to the browser dropped: the process exited, was
    /// killed, or stopped answering. All prior refs are invalid.
    Disconnected(String),
    /// An operation exceeded its deadline. `stage` names the phase.
    Timeout { stage: String, message: String },
    /// A ref no longer identifies its element (page changed / navigated / a
    /// newer snapshot superseded it). NEVER silently retargeted (§18).
    RefStale(String),
    /// The tab the operation targeted is closed or not owned by this session.
    TabClosed(String),
    /// The operation reached the page but could not complete.
    ActionFailed(String),
    /// The backend's protocol cannot provide this operation (§16/§32). Not a
    /// failure to try harder: the capability does not exist.
    Unsupported(String),
    /// The navigation target is refused by the runtime's link-local/metadata
    /// rule (§21). This is a navigation-target refusal, not a network boundary.
    Denied(String),
    /// The caller's cancellation token fired while the operation was in flight.
    Cancelled,
}

impl BrowserError {
    /// A short, stable machine tag for logs/metrics (never the free text).
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Unavailable(_) => "unavailable",
            Self::LaunchFailed(_) => "launch_failed",
            Self::ProfileUnavailable(_) => "profile_unavailable",
            Self::Disconnected(_) => "disconnected",
            Self::Timeout { .. } => "timeout",
            Self::RefStale(_) => "ref_stale",
            Self::TabClosed(_) => "tab_closed",
            Self::ActionFailed(_) => "action_failed",
            Self::Unsupported(_) => "unsupported",
            Self::Denied(_) => "denied",
            Self::Cancelled => "cancelled",
        }
    }

    /// Whether the live session may still be used after this error. Only a
    /// dropped protocol connection invalidates it; a stale ref, an unsupported
    /// operation and a cancelled call all leave the browser healthy (§23).
    pub fn session_still_live(&self) -> bool {
        !matches!(self, Self::Disconnected(_))
    }
}

impl fmt::Display for BrowserError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(m) => write!(f, "browser backend unavailable: {m}"),
            Self::LaunchFailed(m) => write!(f, "browser failed to launch: {m}"),
            Self::ProfileUnavailable(m) => write!(f, "browser profile unavailable: {m}"),
            Self::Disconnected(m) => write!(f, "browser session disconnected: {m}"),
            Self::Timeout { stage, message } => write!(f, "browser {stage} timed out: {message}"),
            Self::RefStale(m) => write!(f, "stale browser ref: {m}"),
            Self::TabClosed(m) => write!(f, "browser tab is closed: {m}"),
            Self::ActionFailed(m) => write!(f, "browser action failed: {m}"),
            Self::Unsupported(m) => write!(f, "{m}"),
            Self::Denied(m) => write!(f, "browser navigation refused: {m}"),
            Self::Cancelled => write!(f, "browser operation cancelled"),
        }
    }
}

impl std::error::Error for BrowserError {}

/// The canonical result alias for browser operations.
pub type BrowserResult<T> = Result<T, BrowserError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_is_stable_and_detail_free() {
        assert_eq!(BrowserError::RefStale("e12".into()).kind(), "ref_stale");
        assert_eq!(
            BrowserError::Timeout {
                stage: "navigate".into(),
                message: "slow".into()
            }
            .kind(),
            "timeout"
        );
    }

    #[test]
    fn only_a_dropped_connection_invalidates_the_session() {
        assert!(BrowserError::RefStale("x".into()).session_still_live());
        assert!(BrowserError::Unsupported("x".into()).session_still_live());
        assert!(BrowserError::Cancelled.session_still_live());
        assert!(!BrowserError::Disconnected("browser exited".into()).session_still_live());
    }
}
