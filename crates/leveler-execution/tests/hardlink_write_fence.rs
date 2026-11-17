//! Shell write-fence limitation: a hardlink alias.
//!
//! The OS write fence is path-based. A confined shell may read broadly — that
//! is the product's documented read trade-off — and may write inside the
//! workspace. On a filesystem that supports hardlinks, `ln <outside-file>
//! <workspace-path>` names the SAME inode under a workspace path, and writing
//! that path changes the outside file while every path check still passes.
//!
//! This test records the limitation with a real canary on a real sandbox. It
//! does not claim the fence catches inode aliases. If a future owner adds
//! inode-level mediation, this test must be changed to assert the denial; the
//! assertion failure is the signal that the documented boundary moved.

#![cfg(unix)]

use std::path::{Path, PathBuf};
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

async fn confined(
    runner: &CommandRunner,
    workspace: &Path,
    program: &str,
    args: &[&str],
) -> ProcessOutput {
    let mut request = ProcessRequest::new(
        program,
        args.iter().map(|arg| arg.to_string()).collect(),
        workspace.to_path_buf(),
    );
    request.timeout = Duration::from_secs(60);
    request.write_scope = WriteScope::Workspace {
        root: workspace.to_path_buf(),
    };
    request.unrestricted_execution = false;
    runner
        .run(request, CancellationToken::new())
        .await
        .unwrap_or_else(|error| panic!("{program} {args:?} failed to run: {error:?}"))
}

fn detail(out: &ProcessOutput) -> String {
    format!(
        "exit={:?} stdout={:?} stderr={:?}",
        out.exit_code, out.stdout, out.stderr
    )
}

/// The canary is the observable: the outside file's bytes change. A path
/// check on the workspace link says nothing about the inode it names.
#[tokio::test]
async fn a_hardlink_alias_inside_the_workspace_writes_the_outside_inode() {
    let workspace = tempfile::tempdir().expect("workspace");
    let home = tempfile::tempdir().expect("home");
    let outside = tempfile::tempdir().expect("outside");
    let workspace = workspace
        .path()
        .canonicalize()
        .expect("canonical workspace");
    let outside_file: PathBuf = outside.path().canonicalize().unwrap().join("canary.txt");
    std::fs::write(&outside_file, b"ORIGINAL-CANARY").expect("canary");
    let link = workspace.join("alias.txt");

    // Skip where hardlinks cannot be created at all (e.g. a filesystem that
    // refuses them or a cross-device temp layout). The limitation below is
    // about the path model, not about every filesystem.
    let probe = std::process::Command::new("ln")
        .arg(&outside_file)
        .arg(&link)
        .status();
    match probe {
        Ok(status) if status.success() => {
            std::fs::remove_file(&link).expect("remove probe link");
        }
        _ => {
            eprintln!("hardlinks are unavailable here; the canary cannot run");
            return;
        }
    }

    let runner = runner(home.path());

    // Step 1: can a CONFINED shell create the alias at all? macOS seatbelt
    // treats the new link as a write to the source inode's metadata, so this
    // is denied even though the link path is inside the writable workspace.
    let create = format!("ln '{}' '{}'", outside_file.display(), link.display());
    let created = confined(&runner, &workspace, "sh", &["-c", &create]).await;
    #[cfg(target_os = "macos")]
    {
        assert!(
            !created.success(),
            "seatbelt denies link creation across the fence; if this changed, \
             the inode-alias limitation is wider than documented: {}",
            detail(&created)
        );
        assert!(
            created.stderr.contains("Operation not permitted"),
            "the refusal must be the OS fence, not a coincidental failure: {}",
            detail(&created)
        );
    }

    // Step 2: an alias that already exists (created before confinement, by a
    // Full session, or by the user) names the same inode. Path checks cannot
    // tell the workspace name from the outside file, so a confined write
    // through it reaches the outside bytes. This is the residual limitation.
    std::fs::hard_link(&outside_file, &link).expect("pre-existing alias");
    let write = format!("printf 'PWNED-BY-ALIAS' > '{}'", link.display());
    let wrote = confined(&runner, &workspace, "sh", &["-c", &write]).await;
    let content = std::fs::read_to_string(&outside_file).expect("read canary");
    assert!(
        wrote.success() && content == "PWNED-BY-ALIAS",
        "a pre-existing alias is the documented residual limitation; if this \
         changed, update SECURITY.md and the shell fence documentation: {}; \
         content={content:?}",
        detail(&wrote)
    );
    let _ = std::fs::remove_file(&link);
}
