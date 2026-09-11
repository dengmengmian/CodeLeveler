//! Windows write confinement: the launcher and the write-root leases.
//!
//! CodeLeveler's filesystem model restricts writes and leaves reads alone
//! (see [`crate::WriteScope`]). On Windows the primitive that says exactly
//! that is Mandatory Integrity Control: the child runs at Low integrity, which
//! denies write-up and never denies read-up, and each authorized write root is
//! labelled Low for the life of the command so the child can write there and
//! nowhere else.
//!
//! Two pieces live here:
//! - [`launcher_path`] finds `leveler-confine.exe`, the argv wrapper that
//!   lowers the token — the Windows counterpart of `sandbox-exec` / `bwrap`.
//! - [`lease_write_roots`] labels the write roots and puts their labels back
//!   when the command ends.
//!
//! The module compiles on every host so its bookkeeping is unit-tested
//! everywhere; the Win32 calls behind it are Windows-only.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use leveler_win_confine::IntegrityLabel;

/// The name of the launcher binary, next to the running executable.
const LAUNCHER_NAME: &str = "leveler-confine.exe";

/// Override for hosts that install the launcher somewhere else.
const LAUNCHER_ENV: &str = "LEVELER_CONFINE_BIN";

/// Absolute path to `leveler-confine.exe`, or `None` when this installation
/// does not carry one.
///
/// Resolution, in order: `LEVELER_CONFINE_BIN`, the directory of the running
/// executable (an installed `leveler.exe` ships the launcher beside it), then
/// its parent (a `cargo test` binary lives in `target/<profile>/deps`, one
/// level below where Cargo puts workspace binaries).
pub fn launcher_path() -> Option<&'static Path> {
    static RESOLVED: OnceLock<Option<PathBuf>> = OnceLock::new();
    RESOLVED.get_or_init(resolve_launcher).as_deref()
}

fn resolve_launcher() -> Option<PathBuf> {
    if let Some(configured) = std::env::var_os(LAUNCHER_ENV) {
        let configured = PathBuf::from(configured);
        return configured.is_file().then_some(configured);
    }
    let executable = std::env::current_exe().ok()?;
    let mut directory = executable.parent();
    // The executable's own directory, then one level up. Two candidates is the
    // whole search: further up is someone else's `target/`, not ours.
    for _ in 0..2 {
        let candidate = directory?.join(LAUNCHER_NAME);
        if candidate.is_file() {
            return Some(candidate);
        }
        directory = directory?.parent();
    }
    None
}

/// A held claim on one write root: how many commands are writing under it, and
/// the label to put back when the last one finishes.
#[derive(Debug, Clone)]
struct Held {
    count: usize,
    previous: IntegrityLabel,
}

fn held() -> &'static Mutex<HashMap<String, Held>> {
    static HELD: OnceLock<Mutex<HashMap<String, Held>>> = OnceLock::new();
    HELD.get_or_init(|| Mutex::new(HashMap::new()))
}

fn root_key(root: &Path) -> String {
    root.canonicalize()
        .unwrap_or_else(|_| root.to_path_buf())
        .to_string_lossy()
        .to_ascii_lowercase()
}

/// Labels released when the command that claimed them ends. Held by the
/// spawned [`crate::command::ManagedProcess`], so a background task keeps its
/// write roots writable for exactly as long as it runs.
#[derive(Debug)]
pub struct WriteRootLease {
    roots: Vec<PathBuf>,
    records: PathBuf,
}

impl Drop for WriteRootLease {
    fn drop(&mut self) {
        for root in std::mem::take(&mut self.roots) {
            if let Err(error) = release_root(&root, &self.records) {
                tracing::warn!(
                    %error,
                    root = %root.display(),
                    "failed to restore a write root's integrity label"
                );
            }
        }
    }
}

/// Label every root in `roots` Low so a Low-integrity child may write there.
/// Fails closed: if any root cannot be labelled, the ones already taken are
/// released and the command does not run.
///
/// Only the user's own roots are restored afterwards. CodeLeveler's private
/// scratch and per-workspace tool cache exist to be written by confined
/// commands and nothing else, so they keep their label — relabelling a Cargo
/// registry twice per command would cost far more than it protects.
pub fn lease_write_roots(
    environment: &leveler_core::EnvSnapshot,
    roots: &[PathBuf],
) -> io::Result<WriteRootLease> {
    let records = records_dir(environment);
    std::fs::create_dir_all(&records)?;
    let home = leveler_core::LevelerHome::resolve(environment);
    let mut lease = WriteRootLease {
        roots: Vec::new(),
        records: records.clone(),
    };
    for root in roots {
        acquire_root(root, &records)?;
        if !is_leveler_owned(root, home.root()) {
            lease.roots.push(root.clone());
        }
    }
    Ok(lease)
}

/// Whether `root` is one of CodeLeveler's own directories rather than the
/// user's. Compared component-wise on the normalized keys, so a symlinked or
/// differently-cased home matches and `<home>-old` does not.
fn is_leveler_owned(root: &Path, home: &Path) -> bool {
    Path::new(&root_key(root)).starts_with(Path::new(&root_key(home)))
}

/// Where a root's pre-label state is parked while it is labelled. Not in the
/// workspace: a residue file the agent can edit is not a record of anything,
/// and a repo is not our scratch space.
fn records_dir(environment: &leveler_core::EnvSnapshot) -> PathBuf {
    leveler_core::LevelerHome::resolve(environment)
        .run_dir()
        .join("windows-write-roots")
}

fn record_path(records: &Path, root: &Path) -> PathBuf {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(root_key(root).as_bytes());
    let name: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    records.join(format!("{name}.label"))
}

fn acquire_root(root: &Path, records: &Path) -> io::Result<()> {
    let key = root_key(root);
    let mut map = held().lock().unwrap_or_else(|poison| poison.into_inner());
    if let Some(entry) = map.get_mut(&key) {
        entry.count += 1;
        return Ok(());
    }
    let record = record_path(records, root);
    // A record still on disk means a previous run was killed between labelling
    // this root and restoring it. Its recorded state, not the live (already
    // Low) label, is what has to go back.
    let previous = match std::fs::read_to_string(&record) {
        Ok(text) => decode_label(text.trim()).unwrap_or(IntegrityLabel::Inherited),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            leveler_win_confine::integrity_label(root)?
        }
        Err(error) => return Err(error),
    };
    std::fs::write(&record, encode_label(&previous))?;
    leveler_win_confine::apply_low_integrity_label(root)?;
    map.insert(key, Held { count: 1, previous });
    Ok(())
}

fn release_root(root: &Path, records: &Path) -> io::Result<()> {
    let key = root_key(root);
    let previous = {
        let mut map = held().lock().unwrap_or_else(|poison| poison.into_inner());
        let Some(entry) = map.get_mut(&key) else {
            return Ok(());
        };
        entry.count -= 1;
        if entry.count > 0 {
            return Ok(());
        }
        map.remove(&key).map(|entry| entry.previous)
    };
    let Some(previous) = previous else {
        return Ok(());
    };
    let restored = leveler_win_confine::restore_integrity_label(root, &previous);
    // Only drop the record once the label is actually back, so a failure here
    // still leaves the next run enough to recover from.
    if restored.is_ok() {
        let _ = std::fs::remove_file(record_path(records, root));
    }
    restored
}

fn encode_label(label: &IntegrityLabel) -> String {
    match label {
        IntegrityLabel::Inherited => "inherited".to_string(),
        IntegrityLabel::Explicit {
            sid,
            ace_flags,
            mask,
        } => format!("explicit {sid} {ace_flags} {mask}"),
    }
}

fn decode_label(text: &str) -> Option<IntegrityLabel> {
    let mut parts = text.split_whitespace();
    match parts.next()? {
        "inherited" => Some(IntegrityLabel::Inherited),
        "explicit" => Some(IntegrityLabel::Explicit {
            sid: parts.next()?.to_string(),
            ace_flags: parts.next()?.parse().ok()?,
            mask: parts.next()?.parse().ok()?,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_label_survives_the_record_round_trip() {
        for label in [
            IntegrityLabel::Inherited,
            IntegrityLabel::Explicit {
                sid: "S-1-16-4096".to_string(),
                ace_flags: 3,
                mask: 1,
            },
        ] {
            let encoded = encode_label(&label);
            assert_eq!(decode_label(&encoded), Some(label), "{encoded}");
        }
    }

    #[test]
    fn a_corrupt_record_decodes_to_nothing_rather_than_a_wrong_label() {
        assert_eq!(decode_label(""), None);
        assert_eq!(decode_label("explicit S-1-16-4096"), None);
        assert_eq!(decode_label("something-else"), None);
    }

    #[test]
    fn only_the_users_own_roots_are_restored() {
        let home = Path::new("/leveler-home");
        assert!(is_leveler_owned(
            Path::new("/leveler-home/cache/tools/abc"),
            home
        ));
        assert!(is_leveler_owned(Path::new("/leveler-home"), home));
        assert!(!is_leveler_owned(Path::new("/Users/me/project"), home));
        // A sibling that merely starts with the same characters is not inside.
        assert!(!is_leveler_owned(Path::new("/leveler-home-old"), home));
    }

    #[test]
    fn each_root_gets_its_own_record_file() {
        let records = Path::new("/records");
        let one = record_path(records, Path::new("/a"));
        let two = record_path(records, Path::new("/b"));
        assert_ne!(one, two);
        assert_eq!(one.parent(), Some(records));
        assert!(
            one.extension()
                .is_some_and(|extension| extension == "label"),
            "{one:?}"
        );
    }

    #[test]
    fn the_launcher_is_only_reported_when_it_is_really_there() {
        // The env override must name a real file; a missing one is not a
        // launcher, and reporting it would turn fail-closed into a spawn error
        // much later, with a worse message.
        let resolved = launcher_path();
        if let Some(path) = resolved {
            assert!(path.is_file(), "{path:?}");
        }
    }
}

/// Real Windows confinement canaries. These do not assert on a capability
/// probe: they run a process and look at what it could actually read, write
/// and not write. CI runs them as their own required step.
#[cfg(windows)]
#[cfg(test)]
mod windows_canaries {
    use std::path::PathBuf;

    use tokio_util::sync::CancellationToken;

    use crate::command::{CommandRunner, ProcessRequest};
    use crate::risk::WriteScope;

    fn confined(script: &str, workspace: &std::path::Path) -> ProcessRequest {
        let mut request = ProcessRequest::new(
            "cmd",
            vec!["/C".into(), script.into()],
            workspace.to_path_buf(),
        );
        request.write_scope = WriteScope::Workspace {
            root: workspace.to_path_buf(),
        };
        request
    }

    /// A runner that sees the real host environment. `CommandRunner::new()`
    /// reads the installed process snapshot, which a unit test never installs —
    /// it would resolve no home, no PATH and no temp directory.
    fn host_runner() -> CommandRunner {
        CommandRunner::with_environment(std::sync::Arc::new(leveler_core::EnvSnapshot::new(
            std::env::vars_os(),
            std::env::current_dir().unwrap_or_default(),
            std::env::temp_dir(),
        )))
    }

    fn host_registry() -> crate::background::BackgroundTaskRegistry {
        crate::background::BackgroundTaskRegistry::with_environment(std::sync::Arc::new(
            leveler_core::EnvSnapshot::new(
                std::env::vars_os(),
                std::env::current_dir().unwrap_or_default(),
                std::env::temp_dir(),
            ),
        ))
    }

    fn launcher_or_skip() -> bool {
        if super::launcher_path().is_none() {
            eprintln!(
                "skipping: leveler-confine.exe is not built \
                 (cargo build -p leveler-win-confine)"
            );
            return false;
        }
        true
    }

    /// The whole point of the backend: a coding agent on Windows must still be
    /// able to run the toolchain that lives outside its workspace.
    #[tokio::test]
    async fn a_confined_command_still_reads_and_runs_the_host_toolchain() {
        if !launcher_or_skip() {
            return;
        }
        let workspace = tempfile::tempdir().expect("workspace");
        let runner = host_runner();
        let output = runner
            .run(
                confined("cargo --version", workspace.path()),
                CancellationToken::new(),
            )
            .await
            .expect("confined cargo must run");
        assert_eq!(
            output.exit_code,
            Some(0),
            "cargo --version under confinement: {output:?}"
        );
        assert!(
            output.stdout.contains("cargo"),
            "confined command must see the real cargo: {output:?}"
        );
    }

    #[tokio::test]
    async fn a_confined_command_writes_inside_its_workspace() {
        if !launcher_or_skip() {
            return;
        }
        let workspace = tempfile::tempdir().expect("workspace");
        let runner = host_runner();
        let output = runner
            .run(
                confined("echo inside> inside.txt", workspace.path()),
                CancellationToken::new(),
            )
            .await
            .expect("confined write must run");
        assert_eq!(output.exit_code, Some(0), "{output:?}");
        assert!(workspace.path().join("inside.txt").is_file());
    }

    /// A real repository is not an empty directory. The label has to reach the
    /// files and subdirectories that were already there, or the agent can
    /// create new files and edit nothing it was asked to edit.
    #[tokio::test]
    async fn a_confined_command_edits_files_that_were_already_in_the_workspace() {
        if !launcher_or_skip() {
            return;
        }
        let workspace = tempfile::tempdir().expect("workspace");
        let nested = workspace.path().join("src").join("deep");
        std::fs::create_dir_all(&nested).expect("nested");
        let existing = nested.join("existing.txt");
        std::fs::write(&existing, "before\n").expect("seed");

        let runner = host_runner();
        let output = runner
            .run(
                confined(
                    "echo after>> src\\deep\\existing.txt && echo new> src\\deep\\new.txt",
                    workspace.path(),
                ),
                CancellationToken::new(),
            )
            .await
            .expect("confined edit must run");
        assert_eq!(output.exit_code, Some(0), "{output:?}");
        let body = std::fs::read_to_string(&existing).expect("read back");
        assert!(
            body.contains("before") && body.contains("after"),
            "a pre-existing file must stay editable under confinement: {body:?}"
        );
        assert!(nested.join("new.txt").is_file());
    }

    #[tokio::test]
    async fn a_confined_command_cannot_write_outside_its_workspace() {
        if !launcher_or_skip() {
            return;
        }
        let workspace = tempfile::tempdir().expect("workspace");
        let outside = tempfile::tempdir().expect("outside");
        let target = outside.path().join("escape.txt");
        let runner = host_runner();
        let output = runner
            .run(
                confined(
                    &format!("echo escaped> \"{}\"", target.display()),
                    workspace.path(),
                ),
                CancellationToken::new(),
            )
            .await
            .expect("the command itself must run");
        assert_ne!(output.exit_code, Some(0), "the write must fail: {output:?}");
        assert!(
            !target.exists(),
            "a confined command wrote outside its workspace: {}",
            target.display()
        );
    }

    /// The label is a change to the user's repository, so it has to come back.
    #[tokio::test]
    async fn a_write_root_gets_its_label_back_when_the_command_ends() {
        if !launcher_or_skip() {
            return;
        }
        let workspace = tempfile::tempdir().expect("workspace");
        let before = leveler_win_confine::integrity_label(workspace.path()).expect("label before");
        let runner = host_runner();
        runner
            .run(
                confined("echo done> done.txt", workspace.path()),
                CancellationToken::new(),
            )
            .await
            .expect("confined command");
        let after = leveler_win_confine::integrity_label(workspace.path()).expect("label after");
        assert_eq!(before, after, "the write root's label was left behind");
    }

    /// Background used to be the unconfined path. It is the same path now, so
    /// the same canary has to hold for it.
    #[tokio::test]
    async fn a_confined_background_command_is_confined_too() {
        if !launcher_or_skip() {
            return;
        }
        let workspace = tempfile::tempdir().expect("workspace");
        let outside = tempfile::tempdir().expect("outside");
        let target = outside.path().join("escape.txt");
        let registry = host_registry();

        let id = registry
            .spawn(
                confined(
                    &format!("echo escaped> \"{}\"", target.display()),
                    workspace.path(),
                ),
                None,
            )
            .await
            .expect("a confined background command must spawn, not be refused");
        let snapshot = registry
            .wait(
                &id,
                Some(std::time::Duration::from_secs(30)),
                &CancellationToken::new(),
            )
            .await
            .expect("wait");
        assert_ne!(snapshot.exit_code, Some(0), "{snapshot:?}");
        assert!(
            !target.exists(),
            "a confined background command wrote outside its workspace"
        );

        let inside_id = registry
            .spawn(confined("echo inside> inside.txt", workspace.path()), None)
            .await
            .expect("spawn inside");
        let inside = registry
            .wait(
                &inside_id,
                Some(std::time::Duration::from_secs(30)),
                &CancellationToken::new(),
            )
            .await
            .expect("wait inside");
        assert_eq!(inside.exit_code, Some(0), "{inside:?}");
        assert!(workspace.path().join("inside.txt").is_file());
        let _ = PathBuf::new();
    }
}
