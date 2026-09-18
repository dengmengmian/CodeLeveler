//! When the last update check happened.
//!
//! This is machine state, not user configuration, so it lives under the
//! global home's `state/` directory next to the other durable runtime state —
//! never in `config.toml`, which the user owns and edits.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// How long to wait after a *failed* check before trying again. Short enough
/// that a transient outage recovers promptly, long enough that restarting the
/// product in a loop does not hammer GitHub.
pub const RETRY_BACKOFF_SECS: u64 = 300;

/// Unix seconds now, saturating at 0 before the epoch.
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Persisted update-check bookkeeping.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateState {
    /// When a release query last *succeeded*. The interval is measured from
    /// here, so an offline period never counts as "recently checked".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_success_unix_secs: Option<u64>,
    /// When a check was last *attempted*, successful or not — the anti-hammer
    /// guard for a product that is restarted often.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_attempt_unix_secs: Option<u64>,
}

impl UpdateState {
    /// Whether a start-up check is due at `now`.
    ///
    /// Due when the successful check is older than `interval_hours` (clamped to
    /// at least one hour) and the previous attempt is older than the retry
    /// backoff.
    pub fn is_due(&self, interval_hours: u64, now: u64) -> bool {
        let interval = interval_hours.max(1) * 3600;
        let success_fresh = self
            .last_success_unix_secs
            .is_some_and(|t| now.saturating_sub(t) < interval);
        let retry_too_soon = self
            .last_attempt_unix_secs
            .is_some_and(|t| now.saturating_sub(t) < RETRY_BACKOFF_SECS);
        !success_fresh && !retry_too_soon
    }

    pub fn mark_attempt(&mut self, now: u64) {
        self.last_attempt_unix_secs = Some(now);
    }

    pub fn mark_success(&mut self, now: u64) {
        self.last_success_unix_secs = Some(now);
        self.last_attempt_unix_secs = Some(now);
    }
}

/// `<leveler-home>/state/update.json`, or `None` when no home is known.
pub fn state_path() -> Option<PathBuf> {
    let home = leveler_core::LevelerHome::resolve(leveler_core::environment());
    Some(home.state_dir().join("update.json"))
}

/// Load the state, or a default when absent. A malformed file is treated as
/// absent — bookkeeping must never block the product from starting.
pub fn load() -> UpdateState {
    let Some(path) = state_path() else {
        return UpdateState::default();
    };
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Persist the state. Best-effort: a failed write only means the next start
/// checks again, which is harmless.
pub fn save(state: &UpdateState) {
    let Some(path) = state_path() else {
        return;
    };
    if let Some(parent) = path.parent()
        && std::fs::create_dir_all(parent).is_err()
    {
        return;
    }
    let Ok(text) = serde_json::to_string_pretty(state) else {
        return;
    };
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_first_run_is_due() {
        assert!(UpdateState::default().is_due(1, 1_000_000));
    }

    #[test]
    fn a_fresh_success_skips_the_check() {
        let state = UpdateState {
            last_success_unix_secs: Some(1_000_000),
            last_attempt_unix_secs: Some(1_000_000),
        };
        assert!(!state.is_due(1, 1_000_000 + 3599));
        assert!(state.is_due(1, 1_000_000 + 3600));
    }

    #[test]
    fn a_failed_attempt_retries_after_the_backoff_not_an_hour() {
        // No successful check, but a recent attempt: not due yet...
        let state = UpdateState {
            last_success_unix_secs: None,
            last_attempt_unix_secs: Some(1_000_000),
        };
        assert!(!state.is_due(1, 1_000_000 + 60));
        // ...but due again after the backoff, so an outage recovers promptly.
        assert!(state.is_due(1, 1_000_000 + RETRY_BACKOFF_SECS));
    }

    #[test]
    fn a_zero_interval_is_clamped_to_one_hour() {
        let state = UpdateState {
            last_success_unix_secs: Some(1_000_000),
            last_attempt_unix_secs: Some(1_000_000),
        };
        assert!(!state.is_due(0, 1_000_000 + 60));
        assert!(state.is_due(0, 1_000_000 + 3600));
    }

    #[test]
    fn marking_success_also_marks_the_attempt() {
        let mut state = UpdateState::default();
        state.mark_success(42);
        assert_eq!(state.last_success_unix_secs, Some(42));
        assert_eq!(state.last_attempt_unix_secs, Some(42));
    }
}
