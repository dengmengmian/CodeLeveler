#![cfg(unix)]

use leveler_execution::process_identity::{
    ProcessIdentity, process_start_identity, verify_process_identity,
};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn a_real_child_identity_matches_until_it_exits() {
    let mut child = Command::new("sh")
        .args(["-c", "sleep 30"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let pid = child.id();
    let start = process_start_identity(pid).expect("macOS/Linux start token");
    assert_eq!(
        verify_process_identity(pid, &start),
        ProcessIdentity::Matches
    );
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(
        verify_process_identity(pid, &start),
        ProcessIdentity::Exited
    );
}

#[test]
fn a_reused_pid_with_a_different_start_token_is_a_mismatch() {
    let mut child = Command::new("sh")
        .args(["-c", "sleep 30"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let identity = verify_process_identity(child.id(), "definitely-not-this-process");
    assert!(
        matches!(identity, ProcessIdentity::Mismatch { .. }),
        "{identity:?}"
    );
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn runtime_child() {
    let Ok(state) = std::env::var("R3_IDENTITY_STATE") else {
        return;
    };
    let mut child = Command::new("sh")
        .args(["-c", "sleep 30"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let pid = child.id();
    let start = process_start_identity(pid).unwrap();
    std::fs::write(&state, format!("{pid}\n{start}\n")).unwrap();
    std::thread::sleep(Duration::from_secs(60));
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn a_runtime_sigkill_leaves_a_verifiable_background_identity() {
    use std::os::unix::process::ExitStatusExt;
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let exe = std::env::current_exe().unwrap();
    let mut runtime = Command::new(exe)
        .args(["runtime_child", "--exact"])
        .env("R3_IDENTITY_STATE", &state)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !state.exists() {
        assert!(Instant::now() < deadline, "child did not publish identity");
        std::thread::sleep(Duration::from_millis(20));
    }
    let text = std::fs::read_to_string(&state).unwrap();
    let mut lines = text.lines();
    let pid: u32 = lines.next().unwrap().parse().unwrap();
    let start = lines.next().unwrap().to_string();
    let runtime_pid = runtime.id();
    eprintln!("R3_IDENTITY runtime_pid={runtime_pid} background_pid={pid} start={start:?}");
    let status = Command::new("kill")
        .args(["-9", &runtime_pid.to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    let status = runtime.wait().unwrap();
    assert_eq!(status.signal(), Some(9));
    assert_eq!(
        verify_process_identity(pid, &start),
        ProcessIdentity::Matches,
        "the background process survived the runtime SIGKILL with the same identity"
    );
    let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
}
