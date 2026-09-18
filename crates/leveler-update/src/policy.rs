//! The `[update]` configuration, as a plain policy value.
//!
//! Two fields, no channel and no boolean algebra:
//!
//! - `auto_update` — whether start-up checks and installs by itself.
//! - `check_interval_hours` — how often a successful check is repeated.
//!
//! Manual `leveler update` and the TUI's `/update` ignore both: an explicit
//! request always checks.

/// How updates are allowed to happen without being asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpdatePolicy {
    /// Check for and install a newer stable release at start-up.
    pub auto_update: bool,
    /// Hours between successful checks. Clamped to at least one.
    pub check_interval_hours: u64,
}

impl Default for UpdatePolicy {
    fn default() -> Self {
        Self {
            auto_update: true,
            check_interval_hours: 1,
        }
    }
}

impl UpdatePolicy {
    /// The effective interval, never below one hour, so a `0` in the config
    /// cannot turn into a check on every start.
    pub fn interval_hours(&self) -> u64 {
        self.check_interval_hours.max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_auto_update_hourly() {
        let policy = UpdatePolicy::default();
        assert!(policy.auto_update);
        assert_eq!(policy.interval_hours(), 1);
    }

    #[test]
    fn a_zero_interval_is_clamped() {
        let policy = UpdatePolicy {
            auto_update: true,
            check_interval_hours: 0,
        };
        assert_eq!(policy.interval_hours(), 1);
    }
}
