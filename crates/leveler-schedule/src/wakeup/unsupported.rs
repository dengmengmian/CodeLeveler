//! Compile-compatible stub for platforms without a real OS wakeup backend in
//! this phase. It reports absence explicitly and refuses to pretend that a
//! registration exists.

use std::path::Path;

use super::{WakeupError, WakeupSpec, WakeupStatus, label_for};

const REASON: &str = "no OS wakeup backend is implemented for this platform; use `leveler schedule serve` while a terminal is available";

pub(super) async fn enable(_spec: &WakeupSpec) -> Result<WakeupStatus, WakeupError> {
    Err(WakeupError::Unsupported(REASON.into()))
}

pub(super) async fn disable(home: &Path) -> Result<WakeupStatus, WakeupError> {
    Ok(WakeupStatus {
        backend: "unsupported",
        supported: false,
        label: label_for(home),
        registration_path: None,
        registered: false,
        loaded: false,
        registration: None,
        last_exit_code: None,
        run_count: None,
        detail: Some(REASON.into()),
    })
}

pub(super) async fn status(home: &Path) -> Result<WakeupStatus, WakeupError> {
    disable(home).await
}
