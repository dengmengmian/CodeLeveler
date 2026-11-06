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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use super::*;
    use crate::wakeup::WAKE_INTERVAL;

    #[tokio::test]
    async fn the_unsupported_backend_reports_absence_instead_of_pretending() {
        let home = Path::new("/tmp/leveler-unsupported-wakeup-test");
        let spec = WakeupSpec {
            home: home.to_path_buf(),
            executable: PathBuf::from("/bin/true"),
            arguments: Vec::new(),
            environment: BTreeMap::new(),
            interval: WAKE_INTERVAL,
            log_dir: home.join("logs"),
        };

        assert!(!crate::wakeup::is_supported());
        assert_eq!(crate::wakeup::backend_name(), "unsupported");

        match crate::wakeup::enable(&spec).await {
            Err(WakeupError::Unsupported(reason)) => assert_eq!(reason, REASON),
            other => panic!("unsupported enable must report Unsupported, got {other:?}"),
        }

        let status = crate::wakeup::status(home).await.expect("status");
        assert_eq!(status.backend, "unsupported");
        assert!(!status.supported);
        assert_eq!(status.label, label_for(home));
        assert!(!status.registered);
        assert!(!status.loaded);
        assert!(status.registration.is_none());
        assert!(status.registration_path.is_none());
        assert_eq!(status.detail.as_deref(), Some(REASON));

        let disabled = crate::wakeup::disable(home).await.expect("disable");
        assert!(!disabled.registered);
        assert_eq!(disabled.detail.as_deref(), Some(REASON));
    }
}
