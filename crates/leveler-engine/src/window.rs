//! Replay shape of the deleted supervisor's window-control state.
//!
//! The engine no longer opens work windows on the model's behalf: a turn
//! ends where the model stops or a hard limit stops it, and the guards that
//! used to decide whether another window was earned are gone with the
//! supervisor. Logs written before that still carry `window_state_updated`
//! rows, and every event type must stay decodable or those sessions cannot
//! be replayed — so the payload shape survives here, and nothing reads it.

use serde::{Deserialize, Serialize};

/// The fields old `window_state_updated` rows carry. Never written anymore.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WindowState {
    #[serde(default)]
    pub window_index: u32,
    #[serde(default)]
    pub budget_extensions: u32,
    #[serde(default)]
    pub round_extensions: u32,
    #[serde(default)]
    pub windows_without_progress: u32,
    #[serde(default)]
    pub segment_source_changes: usize,
    #[serde(default)]
    pub segment_close_attempts: usize,
    #[serde(default)]
    pub progress_files_mark: usize,
    #[serde(default)]
    pub progress_ops_mark: u64,
    #[serde(default)]
    pub progress_fresh_verify_mark: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An old log row must still decode, and one with no state at all reads
    /// as the empty shape rather than failing the replay that met it.
    #[test]
    fn old_rows_still_decode() {
        let state: WindowState = serde_json::from_str(
            r#"{"window_index":3,"budget_extensions":2,"windows_without_progress":2}"#,
        )
        .unwrap();
        assert_eq!(state.window_index, 3);
        let empty: WindowState = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, WindowState::default());
    }
}
