//! Shared wall clock for memory state transitions.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Supplies non-negative Unix seconds to every memory component.
pub trait MemoryClock: std::fmt::Debug + Send + Sync {
    fn now_unix_seconds(&self) -> i64;
}

pub type SharedMemoryClock = Arc<dyn MemoryClock>;

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemMemoryClock;

impl MemoryClock for SystemMemoryClock {
    fn now_unix_seconds(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| {
                i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
            })
    }
}

pub fn system_memory_clock() -> SharedMemoryClock {
    Arc::new(SystemMemoryClock)
}
