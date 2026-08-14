//! The one error type every browser operation returns.
//!
//! Each variant is a distinct, actionable failure mode — a tool maps these to a
//! model-visible `ToolOutput::error` so the agent knows *what* went wrong (a
//! stale ref is not a crashed runtime is not a denied navigation). Never a bare
//! "browser failed".

use std::fmt;

/// A structured browser failure. Categorized so tools and the agent can react
/// precisely; the `Display` text is safe to surface to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserError {
    /// No usable browser at all (no system browser found and managed runtime
    /// absent/could-not-be-prepared). Distinct from a *launch* failure.
    Unavailable(String),
    /// Managed runtime download/install failed (network, extraction, etc.).
    RuntimeInstallFailed(String),
    /// A managed runtime is present but incomplete/corrupt (missing executable,
    /// bad metadata) — the reaper/installer should repair it.
    RuntimeCorrupt(String),
    /// A browser executable path was expected but not found on disk.
    ExecutableNotFound(String),
    /// The browser process failed to launch (bad flags, sandbox, permissions).
    LaunchFailed(String),
    /// The isolated project profile directory is unusable.
    ProfileUnavailable(String),
    /// The driver IPC channel dropped (EOF/broken pipe) — distinct from a
    /// full runtime crash; a reconnect/restart may recover it.
    DriverDisconnected(String),
    /// The driver process (or the browser it owns) crashed. After this the
    /// runtime may restart, and all prior refs are invalid.
    RuntimeCrashed(String),
    /// An action exceeded its deadline. `stage` names which phase timed out
    /// (bootstrap/install/launch/navigate/action/wait) so the agent can tell a
    /// slow page from a stuck runtime.
    ActionTimeout { stage: String, message: String },
    /// A ref no longer identifies its element (structural page change, new
    /// generation, navigation). NEVER silently retargeted — a BLOCKER-level
    /// safety invariant.
    RefStale(String),
    /// The page/tab the operation targeted is closed.
    PageClosed(String),
    /// The action reached the page but could not complete (element not
    /// actionable, obscured, detached, driver-reported failure).
    ActionFailed(String),
    /// The navigation/action was refused by policy (network denied, blocked
    /// host/SSRF, permission). Carries the human reason.
    Denied(String),
}

impl BrowserError {
    /// A short, stable machine tag for logs/metrics (never includes the
    /// free-text detail, so it is safe to aggregate on).
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Unavailable(_) => "unavailable",
            Self::RuntimeInstallFailed(_) => "runtime_install_failed",
            Self::RuntimeCorrupt(_) => "runtime_corrupt",
            Self::ExecutableNotFound(_) => "executable_not_found",
            Self::LaunchFailed(_) => "launch_failed",
            Self::ProfileUnavailable(_) => "profile_unavailable",
            Self::DriverDisconnected(_) => "driver_disconnected",
            Self::RuntimeCrashed(_) => "runtime_crashed",
            Self::ActionTimeout { .. } => "action_timeout",
            Self::RefStale(_) => "ref_stale",
            Self::PageClosed(_) => "page_closed",
            Self::ActionFailed(_) => "action_failed",
            Self::Denied(_) => "denied",
        }
    }

    /// Whether the runtime may still be usable after this error. A stale ref or
    /// a denied action leaves the runtime healthy; a crash/disconnect does not.
    pub fn runtime_still_healthy(&self) -> bool {
        !matches!(
            self,
            Self::RuntimeCrashed(_) | Self::DriverDisconnected(_) | Self::RuntimeCorrupt(_)
        )
    }
}

impl fmt::Display for BrowserError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(m) => write!(f, "no usable browser: {m}"),
            Self::RuntimeInstallFailed(m) => write!(f, "browser runtime install failed: {m}"),
            Self::RuntimeCorrupt(m) => write!(f, "browser runtime is corrupt: {m}"),
            Self::ExecutableNotFound(m) => write!(f, "browser executable not found: {m}"),
            Self::LaunchFailed(m) => write!(f, "browser failed to launch: {m}"),
            Self::ProfileUnavailable(m) => write!(f, "browser profile unavailable: {m}"),
            Self::DriverDisconnected(m) => write!(f, "browser driver disconnected: {m}"),
            Self::RuntimeCrashed(m) => write!(f, "browser runtime crashed: {m}"),
            Self::ActionTimeout { stage, message } => {
                write!(f, "browser {stage} timed out: {message}")
            }
            Self::RefStale(m) => write!(f, "browser ref is stale (page changed): {m}"),
            Self::PageClosed(m) => write!(f, "browser page is closed: {m}"),
            Self::ActionFailed(m) => write!(f, "browser action failed: {m}"),
            Self::Denied(m) => write!(f, "browser action denied: {m}"),
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
            BrowserError::ActionTimeout {
                stage: "navigate".into(),
                message: "slow".into()
            }
            .kind(),
            "action_timeout"
        );
    }

    #[test]
    fn health_distinguishes_recoverable_from_fatal() {
        assert!(BrowserError::RefStale("x".into()).runtime_still_healthy());
        assert!(BrowserError::Denied("x".into()).runtime_still_healthy());
        assert!(!BrowserError::RuntimeCrashed("x".into()).runtime_still_healthy());
        assert!(!BrowserError::DriverDisconnected("x".into()).runtime_still_healthy());
    }
}
