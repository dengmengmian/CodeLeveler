//! Windows Execution Host exit-path regression tests.
//!
//! The host joins a kill-on-close Job Object before reading its durable
//! archive. A post-adoption error must still reach `main`; otherwise closing
//! the job handle terminates the host with a clean-looking status and masks the
//! failure.

#![cfg(windows)]

use std::path::PathBuf;
use std::process::Command;

fn leveler_binary() -> PathBuf {
    std::env::var_os("LEVELER_TEST_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_leveler")))
}

#[test]
fn a_host_error_after_job_adoption_is_not_reported_as_a_clean_exit() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let config = temp.path().join("config");
    let repo = temp.path().join("repo");
    let state = temp.path().join("execution-host");
    for dir in [
        &home,
        &repo,
        &state,
        &config.join("providers"),
        &config.join("models"),
    ] {
        std::fs::create_dir_all(dir).unwrap();
    }
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .arg(&repo)
            .status()
            .unwrap()
            .success()
    );
    std::fs::write(
        home.join("config.toml"),
        "default_model = \"mock/m\"\n\n[update]\nauto_update = false\n",
    )
    .unwrap();
    std::fs::write(
        config.join("providers/mock.yaml"),
        "id: mock\nprotocol: openai_chat\nbase_url: http://127.0.0.1:9\n",
    )
    .unwrap();
    std::fs::write(
        config.join("models/m.yaml"),
        "id: m\nprovider: mock\nmodel_id: mock-model\nprotocol: openai_chat\n",
    )
    .unwrap();
    leveler_win_confine::secure_private_state_dir(&state).unwrap();
    // The host has already joined its job when it reads this archive. Invalid
    // JSON is a deterministic post-adoption error.
    std::fs::write(state.join("tasks.json"), b"not-json").unwrap();

    let output = Command::new(leveler_binary())
        .args(["execution-host", "--state-dir"])
        .arg(&state)
        .arg("--repo")
        .arg(&repo)
        .env("LEVELER_HOME", &home)
        .env("LEVELER_CONFIG_DIR", &config)
        .env("LEVELER_EXECUTION_HOST_IDLE_TIMEOUT_SECS", "0")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "the post-adoption error must not be masked as a clean exit: status={:?} stderr={stderr}",
        output.status
    );
    assert!(
        stderr.contains("expected") || stderr.contains("Execution Host"),
        "the failure must name the archive/startup error, got: {stderr}"
    );
}
