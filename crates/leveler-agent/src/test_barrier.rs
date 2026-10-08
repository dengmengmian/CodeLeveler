//! Integration-test-only crash barrier for the cancellation terminal window.
//!
//! Inert unless this crate is built with the non-default `test-crash-barrier`
//! feature AND the process carries a marker path directly under its isolated
//! `LEVELER_HOME`. It fires after a cancel has been observed but before the
//! terminal is persisted; the test SIGKILLs the daemon once the marker exists,
//! so once it fires this never returns. Production builds contain no
//! environment-controlled crash path: the `cfg` compiles the whole body away.

#[cfg(feature = "test-crash-barrier")]
pub(crate) fn hit_before_cancel_terminal_persist() {
    let Some(raw_path) = std::env::var_os("LEVELER_TEST_BEFORE_CANCEL_TERMINAL_BARRIER") else {
        return;
    };
    let Some(raw_home) = std::env::var_os("LEVELER_HOME") else {
        return;
    };
    let path = std::path::PathBuf::from(raw_path);
    let home = std::path::PathBuf::from(raw_home);
    let valid_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(".test-crash-barrier-"));
    if path.parent() != Some(home.as_path()) || !valid_name {
        tracing::warn!(path = %path.display(), "ignored invalid test crash barrier path");
        return;
    }
    let write_result = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .and_then(|mut file| {
            std::io::Write::write_all(&mut file, b"before_cancel_terminal_persist\n")
        });
    if let Err(error) = write_result {
        tracing::warn!(%error, path = %path.display(), "could not announce test crash barrier");
        return;
    }
    loop {
        std::thread::park();
    }
}

#[cfg(not(feature = "test-crash-barrier"))]
pub(crate) fn hit_before_cancel_terminal_persist() {}
