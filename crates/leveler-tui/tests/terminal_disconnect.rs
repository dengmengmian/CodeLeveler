//! The input event stream must return a terminal read error after PTY hangup.
//! A detached PTY does not send SIGHUP, so input readiness must settle itself.

#[cfg(unix)]
#[test]
fn input_stream_reports_a_disconnected_pty() {
    const CHILD: &str = "LEVELER_TEST_INPUT_DISCONNECT";
    if std::env::var_os(CHILD).is_some() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                use futures::StreamExt;
                let mut events = crossterm::event::EventStream::new();
                let mut pending = Box::pin(events.next());
                assert!(futures::poll!(pending.as_mut()).is_pending());
                drop(pending);
                eprintln!("input-ready");
                assert!(matches!(events.next().await, Some(Err(_))));
            });
        return;
    }
    let script = r#"
import os, pty, subprocess, sys
master, slave = pty.openpty()
child = subprocess.Popen([sys.argv[1], '--exact', 'input_stream_reports_a_disconnected_pty', '--nocapture'],
    stdin=slave, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    env=dict(os.environ, LEVELER_TEST_INPUT_DISCONNECT='1'), start_new_session=True)
os.close(slave)
assert child.stderr.readline().strip() == b'input-ready'
os.close(master)
try:
    out, err = child.communicate(timeout=3)
except subprocess.TimeoutExpired:
    child.kill()
    out, err = child.communicate()
    sys.stderr.write('terminal input did not report PTY hangup\n')
    sys.exit(124)
sys.stdout.buffer.write(out)
sys.stderr.buffer.write(err)
sys.exit(child.returncode)
"#;
    let result = std::process::Command::new("python3")
        .args(["-c", script])
        .arg(std::env::current_exe().unwrap())
        .output()
        .expect("python PTY helper");
    assert!(
        result.status.success(),
        "input must report EOF/EIO instead of spinning: status {:?}, stderr {}",
        result.status,
        String::from_utf8_lossy(&result.stderr)
    );
}
