//! The runtime control directory is not readable from a confined shell.
//!
//! The per-repository Unix socket now requires a per-runtime access token.
//! Tokens and sockets live under `<LevelerHome>/run/sockets`. If a confined
//! command could read the token (or reach the socket despite the mask), the
//! token would be a shareable credential. macOS Seatbelt denies reads of the
//! directory; Linux bwrap mounts an empty tmpfs over it. This canary drives the
//! real `CommandRunner`, so a regression in either backend is visible on that
//! platform instead of only in the profile string.
//!
//! The server-side token handshake and Linux mount-namespace check are covered
//! by `leveler-local-transport`'s Unix tests.

#![cfg(unix)]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use leveler_execution::{CommandRunner, ProcessOutput, ProcessRequest, WriteScope};
use tokio_util::sync::CancellationToken;

fn runner(home: &Path) -> CommandRunner {
    let mut values: Vec<(std::ffi::OsString, std::ffi::OsString)> = std::env::vars_os().collect();
    values.push((
        std::ffi::OsString::from("LEVELER_HOME"),
        home.as_os_str().to_os_string(),
    ));
    CommandRunner::with_environment(Arc::new(leveler_core::EnvSnapshot::new(
        values,
        std::env::current_dir().unwrap_or_default(),
        std::env::temp_dir(),
    )))
}

async fn confined(runner: &CommandRunner, workspace: &Path, host_home: &Path) -> ProcessOutput {
    let mut request = ProcessRequest::new(
        "sh",
        vec![
            "-c".to_string(),
            concat!(
                "if cat \"$HOST_HOME/run/sockets/runtime.token\" >/dev/null 2>&1; ",
                "then echo TOKEN_READ; else echo TOKEN_DENIED; fi; ",
                "if [ -e \"$HOST_HOME/run/sockets/runtime.sock\" ]; ",
                "then echo SOCK_VISIBLE; else echo SOCK_HIDDEN; fi; ",
                "printf CONTROL > control.txt && cat control.txt"
            )
            .to_string(),
        ],
        workspace.to_path_buf(),
    );
    request.timeout = Duration::from_secs(60);
    request.write_scope = WriteScope::Workspace {
        root: workspace.to_path_buf(),
    };
    request.authority_env = vec![("HOST_HOME".to_string(), host_home.display().to_string())];
    runner
        .run(request, CancellationToken::new())
        .await
        .unwrap_or_else(|error| panic!("confined control canary failed to run: {error:?}"))
}

#[tokio::test]
async fn a_confined_shell_cannot_read_the_runtime_token_or_socket() {
    let home = tempfile::tempdir().expect("home");
    let root = tempfile::tempdir().expect("workspace");
    let host_home = home.path().canonicalize().expect("canonical home");
    let workspace = root.path().canonicalize().expect("canonical workspace");
    let sockets = host_home.join("run/sockets");
    std::fs::create_dir_all(&sockets).unwrap();
    std::fs::write(sockets.join("runtime.token"), "a".repeat(64).as_bytes()).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(sockets.join("runtime.sock")).unwrap();

    let out = confined(&runner(&host_home), &workspace, &host_home).await;
    let detail = format!(
        "exit={:?} stdout={:?} stderr={:?}",
        out.exit_code, out.stdout, out.stderr
    );
    assert_eq!(out.exit_code, Some(0), "{detail}");
    assert!(
        out.stdout.contains("TOKEN_DENIED"),
        "the confined shell must not read the runtime token: {detail}"
    );
    assert!(
        !out.stdout.contains(&"a".repeat(64)),
        "no token byte may reach the command output: {detail}"
    );
    assert!(
        out.stdout.contains("SOCK_HIDDEN"),
        "the confined shell must not see the control socket: {detail}"
    );
    assert!(
        out.stdout.contains("CONTROL"),
        "the control file proves ordinary workspace writes still work: {detail}"
    );

    // The socket stays live and owned by this test; the mask is in the child.
    assert!(listener.set_nonblocking(true).is_ok());
}

/// The default home is created by the first bind; this canary also proves the
/// control mask applies when the directory pre-exists (the real runtime case).
#[tokio::test]
async fn the_mask_covers_an_existing_sockets_directory() {
    let home = tempfile::tempdir().expect("home");
    let root = tempfile::tempdir().expect("workspace");
    let host_home = home.path().canonicalize().expect("canonical home");
    let workspace = root.path().canonicalize().expect("canonical workspace");
    let sockets = host_home.join("run/sockets");
    std::fs::create_dir_all(&sockets).unwrap();
    std::fs::write(sockets.join("runtime.token"), "SENTINEL-TOKEN").unwrap();

    let out = confined(&runner(&host_home), &workspace, &host_home).await;
    let detail = format!(
        "exit={:?} stdout={:?} stderr={:?}",
        out.exit_code, out.stdout, out.stderr
    );
    assert_eq!(out.exit_code, Some(0), "{detail}");
    assert!(
        out.stdout.contains("TOKEN_DENIED") && !out.stdout.contains("SENTINEL-TOKEN"),
        "an existing control directory stays hidden: {detail}"
    );
}
