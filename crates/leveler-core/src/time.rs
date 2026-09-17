//! Time primitives. A thin alias over `chrono` UTC so the rest of the codebase
//! never imports `chrono` directly and we can swap the backing clock later.

use chrono::{DateTime, Utc};

/// A UTC timestamp used across sessions, events and artifacts.
pub type Timestamp = DateTime<Utc>;

/// The current wall-clock time in UTC.
pub fn now() -> Timestamp {
    Utc::now()
}
