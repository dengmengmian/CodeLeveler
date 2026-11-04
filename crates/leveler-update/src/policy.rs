//! The `[update]` configuration, as a plain policy value.
//!
//! Two fields, no channel and no boolean algebra:
//!
//! - `auto_update` — whether start-up checks and installs by itself.
//! - `check_interval_hours` — how often a successful check is repeated;
//!   `0` checks on every start.
//!
//! Manual `leveler update` and the TUI's `/update` ignore both: an explicit
//! request always checks.

/// How updates are allowed to happen without being asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpdatePolicy {
    /// Check for and install a newer stable release at start-up.
    pub auto_update: bool,
    /// Hours between successful checks; `0` checks on every start.
    pub check_interval_hours: u64,
}

impl Default for UpdatePolicy {
    fn default() -> Self {
        Self {
            auto_update: true,
            check_interval_hours: 0,
        }
    }
}

impl UpdatePolicy {
    /// The effective interval. Zero is valid and means every start is due.
    pub fn interval_hours(&self) -> u64 {
        self.check_interval_hours
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_check_on_every_start() {
        let policy = UpdatePolicy::default();
        assert!(policy.auto_update);
        assert_eq!(policy.interval_hours(), 0);
    }

    #[test]
    fn a_zero_interval_is_a_valid_policy() {
        let policy = UpdatePolicy {
            auto_update: true,
            check_interval_hours: 0,
        };
        assert_eq!(policy.interval_hours(), 0);
    }
}
