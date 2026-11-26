//! Windows write confinement primitives.
//!
//! CodeLeveler's filesystem permission model restricts **writes only**: a tool
//! may read any path, and the OS boundary confines what it may change. On
//! Windows the mechanism that spells exactly that is Mandatory Integrity
//! Control: a Low-integrity process reads normally (the default mandatory
//! policy is NO_WRITE_UP, never NO_READ_UP) and cannot write to any object
//! labelled Medium or above. Authorized write roots are labelled Low so the
//! confined child — and only inside them — can write.
//!
//! This crate is the audited `unsafe` boundary the rest of the workspace calls
//! through. `leveler-execution` stays `deny(unsafe_code)`:
//! - the [`leveler-confine`](../leveler_confine/index.html) binary launches a
//!   child under a Low-integrity token, and
//! - [`apply_low_integrity_label`] / [`restore_integrity_label`] move a write
//!   root's mandatory label and put it back.
//!
//! Non-Windows builds compile to honest `Unsupported` errors so the workspace
//! still builds and tests on every host.

use std::io;
use std::path::Path;

#[cfg(windows)]
mod windows;

/// S-1-16-4096 — the Low mandatory level.
pub const LOW_INTEGRITY_SID: &str = "S-1-16-4096";

/// S-1-16-8192 — the Medium mandatory level.
pub const MEDIUM_INTEGRITY_SID: &str = "S-1-16-8192";

/// The mandatory label on one filesystem object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntegrityLabel {
    /// No explicit label. Windows treats such an object as Medium.
    Inherited,
    /// An explicit `SYSTEM_MANDATORY_LABEL_ACE`.
    Explicit {
        /// String form of the label SID, e.g. `S-1-16-4096`.
        sid: String,
        /// `ACE_HEADER.AceFlags` (inheritance).
        ace_flags: u8,
        /// The mandatory policy mask (`NO_WRITE_UP` and friends).
        mask: u32,
    },
}

impl IntegrityLabel {
    /// Whether this label already allows a Low-integrity subject to write.
    pub fn is_low(&self) -> bool {
        matches!(self, Self::Explicit { sid, .. } if sid == LOW_INTEGRITY_SID)
    }
}

#[cfg(not(windows))]
fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "Windows integrity labels are only available on Windows hosts",
    )
}

/// Read the mandatory label currently on `path`.
pub fn integrity_label(path: &Path) -> io::Result<IntegrityLabel> {
    #[cfg(windows)]
    {
        windows::read_label(path)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Err(unsupported())
    }
}

/// Label `path` Low (inheritable by new children) and return the label it had,
/// so the caller can put it back. Idempotent: a root already labelled Low is
/// left alone and reported as such.
pub fn apply_low_integrity_label(path: &Path) -> io::Result<IntegrityLabel> {
    #[cfg(windows)]
    {
        windows::apply_low(path)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Err(unsupported())
    }
}

/// Label `path` Medium (inheritable by new children) and return the label it
/// had, so the caller can put it back.
///
/// This is how a protected file or directory inside a Low-labelled write root
/// stays unwritable for the confined child: Mandatory Integrity Control denies
/// the Low subject write-up, so the policy tree keeps the level it had while
/// the rest of the workspace can be written.
pub fn apply_medium_integrity_label(path: &Path) -> io::Result<IntegrityLabel> {
    #[cfg(windows)]
    {
        windows::apply_medium(path)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Err(unsupported())
    }
}

/// Put back the label [`apply_low_integrity_label`] replaced.
pub fn restore_integrity_label(path: &Path, previous: &IntegrityLabel) -> io::Result<()> {
    #[cfg(windows)]
    {
        windows::restore(path, previous)
    }
    #[cfg(not(windows))]
    {
        let _ = (path, previous);
        Err(unsupported())
    }
}

/// Create (when missing) and validate an owner-only state directory.
///
/// This is the Windows counterpart of the Unix `0700` directory check the
/// persistent Execution Host performs: the directory must be owned by the
/// current user, carry no reparse point, and its DACL may grant access only to
/// that user, SYSTEM and the Administrators group. A missing directory is
/// created with exactly that DACL (inheritance protected); an existing
/// directory that does not already satisfy it is refused rather than silently
/// rewritten.
pub fn secure_private_state_dir(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        windows::secure_private_state_dir(path)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Err(unsupported())
    }
}

/// Validate one regular file inside a private state directory.
///
/// The file must not be a reparse point, must be owned by the current user and
/// must carry the owner-only DACL the state directory establishes.
pub fn verify_private_state_file(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        windows::verify_private_state_file(path)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Err(unsupported())
    }
}

/// Apply and verify an owner-only, inheritance-protected DACL on one regular
/// file.
///
/// This is the Windows counterpart of `0600` for a credential file the host
/// writes outside a private state directory: the file must not be a reparse
/// point, and after this call only the current user, SYSTEM and the
/// Administrators group may read or change it. The Unix build reports
/// `Unsupported`; callers keep their mode-bit path there.
pub fn secure_private_file(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        windows::secure_private_file(path)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Err(unsupported())
    }
}

/// Create a new regular file whose owner-only, inheritance-protected DACL is
/// applied atomically with creation.
///
/// The returned handle is opened without sharing so the caller can write
/// plaintext before any other process can open the staged name. The caller
/// must flush and close it before renaming it into place. On non-Windows hosts
/// this reports `Unsupported`; callers keep their mode-bit path there.
pub fn create_private_file(path: &Path) -> io::Result<std::fs::File> {
    #[cfg(windows)]
    {
        windows::create_private_file(path)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Err(unsupported())
    }
}

/// Move `source` to `target`, optionally replacing an existing target.
///
/// On Windows the move preserves the source file's security descriptor, which
/// is what makes a private staged file stay private after the commit. An
/// existing target should be tightened before the move because Windows'
/// `ReplaceFileW` has the opposite behavior (it keeps the replaced file's
/// DACL) and must not be used for this purpose.
pub fn move_private_file(source: &Path, target: &Path, replace: bool) -> io::Result<()> {
    #[cfg(windows)]
    {
        windows::move_file(source, target, replace)
    }
    #[cfg(not(windows))]
    {
        let _ = (source, target, replace);
        Err(unsupported())
    }
}

/// Read a process's creation time as its stable start token.
///
/// `ERROR_INVALID_PARAMETER` means no process with that id exists;
/// `ERROR_ACCESS_DENIED` means existence could not be established for this
/// token. Callers must distinguish those two answers from "the process is
/// gone" — only the first is proof of absence.
pub fn process_creation_time(pid: u32) -> io::Result<u64> {
    #[cfg(windows)]
    {
        windows::process_creation_time(pid)
    }
    #[cfg(not(windows))]
    {
        let _ = pid;
        Err(unsupported())
    }
}

/// Whether `pid` names a live (not terminated) process.
///
/// A terminated process object may remain queryable while a handle keeps it
/// alive; looking at the exit code is what separates "running" from "exited,
/// not yet reaped". `Err` is not evidence either way unless its code is
/// [`ERROR_INVALID_PARAMETER_CODE`].
pub fn process_is_running(pid: u32) -> io::Result<bool> {
    #[cfg(windows)]
    {
        windows::process_is_running(pid)
    }
    #[cfg(not(windows))]
    {
        let _ = pid;
        Err(unsupported())
    }
}

/// The raw `ERROR_INVALID_PARAMETER` code [`process_creation_time`] reports
/// for a pid that names no process.
pub const ERROR_INVALID_PARAMETER_CODE: i32 = 87;

/// The raw `ERROR_ACCESS_DENIED` code a process probe reports when the token
/// may not query that process.
pub const ERROR_ACCESS_DENIED_CODE: i32 = 5;

/// A kill-on-close Job Object binding the current process and every
/// descendant. Held by a persistent owner for its whole life; when the owner
/// dies — gracefully or abruptly — the kernel closes the last handle and
/// terminates the whole tree. A supervisor that must not leave children
/// behind refuses to start when this cannot be established.
#[cfg(windows)]
pub use windows::ProcessTreeJob;

/// An open handle that denies delete sharing on one filesystem object.
///
/// Mandatory Integrity Control does not govern deletion (NTFS checks the
/// parent directory's label), so a Low child in a Low workspace can delete a
/// Medium-labelled file. This fence closes that: while the trusted runtime
/// holds the handle, any open requesting DELETE — the first step of delete,
/// rename and replace — fails with a sharing violation. Reads and writes stay
/// possible, so the integrity label remains the content boundary.
#[cfg(windows)]
pub use windows::PathGuard;

/// Open a delete-sharing fence on `path`.
#[cfg(windows)]
pub fn guard_path(path: &Path) -> io::Result<PathGuard> {
    windows::guard_path(path)
}

/// Where `cmd.exe`'s own parsing takes over.
///
/// `cmd /C <tail>` hands everything after the switch to cmd, which parses it
/// with its own rules — and a backslash is not a quote escape there. Quoting
/// that tail the way Win32 argv is quoted turns
/// `powershell -Command "Start-Sleep -Seconds 5"` into
/// `powershell -Command \"Start-Sleep -Seconds 5\"`, and the quotes arrive at
/// the program as characters: PowerShell printed the command instead of
/// running it. The tail has to reach cmd exactly as the caller wrote it.
///
/// Returns the index of the first argument cmd parses itself, when there is
/// one.
pub fn cmd_tail_start(program: &str, args: &[String]) -> Option<usize> {
    let basename = program
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(program)
        .to_ascii_lowercase();
    if !matches!(basename.as_str(), "cmd" | "cmd.exe") {
        return None;
    }
    let switch = args
        .iter()
        .position(|arg| arg.eq_ignore_ascii_case("/C") || arg.eq_ignore_ascii_case("/K"))?;
    (switch + 1 < args.len()).then_some(switch + 1)
}

/// Whether this build can launch a Low-integrity child at all.
pub fn low_integrity_supported() -> bool {
    cfg!(windows)
}

/// A filesystem object's stable incarnation on a Windows volume.
///
/// The file id is the volume's own identity for the object, not a spelling of
/// its path: it survives renames and case changes, and a delete/recreate (or a
/// reparse-point retarget) produces a different id. `creation_time` is mixed
/// in because Windows may reuse an id after the original object is gone; the
/// pair is what the Unix side calls dev/inode/birth time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileObjectIdentity {
    /// Volume serial number from `FILE_ID_INFO`.
    pub volume_serial: u64,
    /// 128-bit file id (ReFS uses all 128 bits; NTFS fills the low 64).
    pub file_id: [u8; 16],
    /// Object creation time as a Windows FILETIME tick count.
    pub creation_time: u64,
}

impl FileObjectIdentity {
    /// Lowercase hexadecimal id for the identity string.
    pub fn file_id_hex(&self) -> String {
        let mut hex = String::with_capacity(32);
        for byte in self.file_id {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
        }
        hex
    }
}

/// Read the stable identity of the object `path` currently names.
///
/// Reparse points are followed: the identity is the object a read or write
/// would reach, not the link that points at it. The filesystem must provide
/// persistent file ids (NTFS or ReFS) on a local volume; anything else is an
/// `Unsupported` error rather than a path-based guess.
pub fn file_object_identity(path: &Path) -> io::Result<FileObjectIdentity> {
    #[cfg(windows)]
    {
        windows::file_identity_at(path)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Err(unsupported())
    }
}

/// Read the stable identity from an already-open handle.
///
/// The handle is borrowed: the caller keeps ownership. This is the TOCTOU-free
/// form for a descriptor the caller has already opened and will mutate.
#[cfg(windows)]
pub fn file_object_identity_of_raw_handle(
    handle: std::os::windows::io::RawHandle,
) -> io::Result<FileObjectIdentity> {
    windows::file_identity_of_handle(handle as windows::RawHandle)
}

/// Whether an identity can be taken from a handle on this host.
pub fn file_identity_supported() -> bool {
    cfg!(windows)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a junction through a small batch file.
    ///
    /// The direct `cmd /c mklink /J <link> <target>` argv form is rejected on
    /// this host with "Access is denied" even though the same mklink line in a
    /// batch file succeeds; the test cares about the junction, not cmd's
    /// argument reconstruction.
    /// Returns `None` when this host's process policy refuses reparse-point
    /// creation from an unsigned test binary. The product behavior is still
    /// exercised by `scripts/secret_broker_windows_acceptance.ps1`, which
    /// creates the junction from the signed script host and passes it to the
    /// acceptance test.
    #[cfg(windows)]
    fn create_junction(
        link: &std::path::Path,
        target: &std::path::Path,
    ) -> Option<std::process::Output> {
        let parent = link.parent().unwrap_or_else(|| std::path::Path::new("."));
        let script = parent.join(format!(
            "mklink-{}.cmd",
            link.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| std::process::id().to_string())
        ));
        std::fs::write(&script, "@echo off\r\nmklink /J \"%~1\" \"%~2\"\r\n").unwrap();
        let output = std::process::Command::new("cmd")
            .arg("/c")
            .arg(&script)
            .arg(link)
            .arg(target)
            .output()
            .unwrap();
        let _ = std::fs::remove_file(&script);
        if !output.status.success() {
            eprintln!(
                "skip: this host refused reparse-point creation from the unsigned test \
                 process (status {:?}); the same check runs from \
                 scripts/secret_broker_windows_acceptance.ps1",
                output.status.code()
            );
            return None;
        }
        Some(output)
    }

    #[cfg(windows)]
    #[test]
    fn a_fresh_state_directory_is_created_owner_only_and_validates() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state").join("execution-host");
        secure_private_state_dir(&state).expect("fresh state dir");
        assert!(state.is_dir());
        let ready = state.join("ready.json");
        std::fs::write(&ready, b"{}").unwrap();
        verify_private_state_file(&ready).expect("ready file inherits the private DACL");
        // Re-validating an existing private directory is idempotent.
        secure_private_state_dir(&state).expect("existing state dir");
    }

    #[cfg(windows)]
    #[test]
    fn a_state_directory_that_grants_everyone_access_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("broad");
        std::fs::create_dir_all(&state).unwrap();
        let status = std::process::Command::new("icacls")
            .arg(&state)
            .args(["/grant", "*S-1-1-0:(OI)(CI)F"])
            .status()
            .unwrap();
        assert!(status.success(), "icacls must succeed");
        let error = secure_private_state_dir(&state).expect_err("broad DACL must be refused");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{error}");
    }

    #[cfg(windows)]
    #[test]
    fn a_junction_where_the_state_directory_should_be_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("real");
        let link = temp.path().join("link");
        std::fs::create_dir_all(&target).unwrap();
        let Some(status) = create_junction(&link, &target) else {
            return;
        };
        assert!(status.status.success(), "mklink /J must succeed");
        let error = secure_private_state_dir(&link).expect_err("junction must be refused");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{error}");
    }

    #[cfg(windows)]
    #[test]
    fn a_reattached_reparse_point_ready_file_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        secure_private_state_dir(&state).unwrap();
        let target = temp.path().join("elsewhere");
        std::fs::create_dir_all(&target).unwrap();
        let link = state.join("ready.json");
        let Some(status) = create_junction(&link, &target) else {
            return;
        };
        assert!(status.status.success(), "mklink /J must succeed");
        let error = verify_private_state_file(&link).expect_err("reparse point must be refused");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{error}");
    }

    #[cfg(windows)]
    #[test]
    fn process_creation_time_identifies_a_live_process_and_refuses_a_dead_pid() {
        let mine = process_creation_time(std::process::id()).expect("own process");
        assert!(mine > 0);
        let mut child = std::process::Command::new("cmd")
            .args(["/c", "exit", "0"])
            .spawn()
            .unwrap();
        let pid = child.id();
        child.wait().unwrap();
        // Give the kernel a moment to release the id; absence is reported as
        // ERROR_INVALID_PARAMETER, never as a generic failure.
        let mut last = None;
        for _ in 0..50 {
            match process_creation_time(pid) {
                Err(error) if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER_CODE) => {
                    return;
                }
                other => {
                    last = other.ok();
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            }
        }
        // A recycled id would be a different process; the only acceptable
        // alternative to ERROR_INVALID_PARAMETER is a different creation time.
        assert_ne!(last, Some(mine), "a dead pid must not keep its identity");
    }

    #[test]
    fn a_low_label_is_recognised_by_its_sid() {
        assert!(
            IntegrityLabel::Explicit {
                sid: LOW_INTEGRITY_SID.to_string(),
                ace_flags: 3,
                mask: 1,
            }
            .is_low()
        );
        assert!(!IntegrityLabel::Inherited.is_low());
        assert!(
            !IntegrityLabel::Explicit {
                sid: "S-1-16-8192".to_string(),
                ace_flags: 3,
                mask: 1,
            }
            .is_low()
        );
    }

    #[test]
    fn cmd_parses_its_own_tail_and_nothing_else_does() {
        let args = |values: &[&str]| -> Vec<String> {
            values.iter().map(|value| value.to_string()).collect()
        };
        assert_eq!(
            cmd_tail_start("cmd", &args(&["/C", "echo hi"])),
            Some(1),
            "everything after /C belongs to cmd"
        );
        assert_eq!(
            cmd_tail_start(r"C:\Windows\System32\cmd.exe", &args(&["/k", "dir"])),
            Some(1),
            "the switch and the program name are matched case-insensitively"
        );
        assert_eq!(
            cmd_tail_start("cmd", &args(&["/C"])),
            None,
            "a switch with nothing after it has no tail"
        );
        assert_eq!(
            cmd_tail_start("powershell", &args(&["-Command", "x"])),
            None,
            "only cmd parses its own tail"
        );
        assert_eq!(cmd_tail_start("cmd", &args(&["echo", "hi"])), None);
    }

    #[test]
    fn support_tracks_the_host() {
        assert_eq!(low_integrity_supported(), cfg!(windows));
    }

    #[cfg(not(windows))]
    #[test]
    fn off_windows_every_label_call_is_honestly_unsupported() {
        let path = Path::new("/tmp");
        assert_eq!(
            integrity_label(path).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        assert_eq!(
            apply_low_integrity_label(path).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        assert_eq!(
            restore_integrity_label(path, &IntegrityLabel::Inherited)
                .unwrap_err()
                .kind(),
            io::ErrorKind::Unsupported
        );
        assert_eq!(
            apply_medium_integrity_label(path).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        assert_eq!(
            file_object_identity(path).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        assert!(!file_identity_supported());
    }

    #[test]
    fn a_file_identity_renders_its_full_128_bit_id() {
        let identity = FileObjectIdentity {
            volume_serial: 7,
            file_id: [
                0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
                0xee, 0xff,
            ],
            creation_time: 9,
        };
        assert_eq!(
            identity.file_id_hex(),
            "00112233445566778899aabbccddeeff".to_string()
        );
    }

    #[cfg(windows)]
    mod windows_identity {
        use super::*;
        use std::path::Path;

        fn junction(link: &Path, target: &Path) -> bool {
            let parent = link.parent().unwrap_or_else(|| Path::new("."));
            let script = parent.join("mklink-identity.cmd");
            std::fs::write(&script, "@echo off\r\nmklink /J \"%~1\" \"%~2\"\r\n").unwrap();
            let output = std::process::Command::new("cmd")
                .arg("/C")
                .arg(&script)
                .arg(link)
                .arg(target)
                .output()
                .unwrap();
            let _ = std::fs::remove_file(&script);
            if !output.status.success() {
                eprintln!(
                    "skip: this host refused reparse-point creation from the unsigned test \
                     process; the signed acceptance script covers the junction identity"
                );
                return false;
            }
            true
        }

        #[test]
        fn a_rename_and_a_case_only_spelling_change_keep_the_object_identity() {
            let dir = tempfile::tempdir().unwrap();
            let original = dir.path().join("notes.txt");
            std::fs::write(&original, "body").unwrap();
            let before = file_object_identity(&original).unwrap();
            assert_eq!(
                file_object_identity(dir.path()).unwrap().volume_serial,
                before.volume_serial
            );

            let renamed = dir.path().join("renamed.txt");
            std::fs::rename(&original, &renamed).unwrap();
            assert_eq!(file_object_identity(&renamed).unwrap(), before);

            let different_case = dir.path().join("RENAMED.TXT");
            assert_eq!(file_object_identity(&different_case).unwrap(), before);
        }

        #[test]
        fn deleting_and_recreating_the_name_produces_a_new_incarnation() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("victim.txt");
            std::fs::write(&path, "first").unwrap();
            let before = file_object_identity(&path).unwrap();
            std::fs::remove_file(&path).unwrap();
            std::fs::write(&path, "second").unwrap();
            let after = file_object_identity(&path).unwrap();
            assert_ne!(
                before, after,
                "a replacement must not inherit the previous grant"
            );
        }

        #[test]
        fn a_junction_binds_the_target_object_and_retargeting_is_detected() {
            let dir = tempfile::tempdir().unwrap();
            let first = dir.path().join("first");
            let second = dir.path().join("second");
            std::fs::create_dir(&first).unwrap();
            std::fs::create_dir(&second).unwrap();
            let link = dir.path().join("link");
            if !junction(&link, &first) {
                return;
            }
            let target_identity = file_object_identity(&first).unwrap();
            assert_eq!(
                file_object_identity(&link).unwrap(),
                target_identity,
                "a junction is the object it reaches, not its own spelling"
            );
            std::fs::remove_dir(&link).unwrap();
            if !junction(&link, &second) {
                return;
            }
            assert_ne!(file_object_identity(&link).unwrap(), target_identity);
        }

        #[test]
        fn chinese_and_space_bearing_paths_resolve() {
            let dir = tempfile::tempdir().unwrap();
            let nested = dir.path().join("中文 目录");
            std::fs::create_dir(&nested).unwrap();
            let file = nested.join("文件 一.txt");
            std::fs::write(&file, "内容").unwrap();
            assert_eq!(
                file_object_identity(&nested).unwrap(),
                file_object_identity(&nested).unwrap()
            );
            assert_eq!(
                file_object_identity(&file).unwrap(),
                file_object_identity(&file).unwrap()
            );
        }

        #[test]
        fn an_open_handle_reports_the_same_identity_as_its_path() {
            use std::os::windows::io::AsRawHandle;
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("held.txt");
            std::fs::write(&path, "held").unwrap();
            let file = std::fs::File::open(&path).unwrap();
            let from_handle = file_object_identity_of_raw_handle(file.as_raw_handle()).unwrap();
            assert_eq!(from_handle, file_object_identity(&path).unwrap());
        }

        #[test]
        fn a_relative_path_cannot_prove_which_volume_holds_it() {
            let error = file_object_identity(Path::new("relative.txt")).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::Unsupported, "{error}");
        }
    }
}

#[cfg(all(test, windows))]
mod private_file_tests {
    use super::{
        create_private_file, move_private_file, secure_private_file, verify_private_state_file,
    };
    use std::io::Write;
    use std::path::Path;

    fn broad_directory(root: &Path) -> std::path::PathBuf {
        let dir = root.join("broad");
        std::fs::create_dir_all(&dir).unwrap();
        let status = std::process::Command::new("icacls")
            .arg(&dir)
            .args(["/grant", "*S-1-1-0:(OI)(CI)R"])
            .status()
            .unwrap();
        assert!(status.success(), "icacls must succeed");
        dir
    }

    #[test]
    fn a_staged_file_does_not_inherit_a_broad_parent_acl() {
        let temp = tempfile::tempdir().unwrap();
        let dir = broad_directory(temp.path());
        let staged = dir.join("stage.tmp");
        let mut file = create_private_file(&staged).expect("create staged file");
        file.write_all(b"CANARY-SECRET-DO-NOT-EXPOSE").unwrap();
        file.sync_all().unwrap();
        drop(file);
        verify_private_state_file(&staged).expect("the staged file must be private from creation");
    }

    #[test]
    fn a_private_stage_moved_over_a_broad_target_stays_private() {
        let temp = tempfile::tempdir().unwrap();
        let dir = broad_directory(temp.path());
        let target = dir.join(".env");
        std::fs::write(&target, "OLD=1\n").unwrap();
        // The target started with the broad inherited DACL.
        assert!(
            verify_private_state_file(&target).is_err(),
            "the probe target must start broad"
        );
        let staged = dir.join("stage.tmp");
        let mut file = create_private_file(&staged).expect("create staged file");
        file.write_all(b"NEW=1\n").unwrap();
        file.sync_all().unwrap();
        drop(file);
        // The measured Windows rule: an existing destination is tightened
        // before the move because ReplaceFile would keep its DACL.
        secure_private_file(&target).expect("tighten the existing target");
        move_private_file(&staged, &target, true).expect("move over the target");
        verify_private_state_file(&target).expect("the committed file must be private");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "NEW=1\n");
        assert!(!staged.exists(), "the staged name is gone after the move");
    }

    #[test]
    fn a_no_replace_move_refuses_an_existing_target() {
        let temp = tempfile::tempdir().unwrap();
        let staged = temp.path().join("stage.tmp");
        let target = temp.path().join("exists.tmp");
        let mut file = create_private_file(&staged).unwrap();
        file.write_all(b"new").unwrap();
        drop(file);
        std::fs::write(&target, "old").unwrap();
        let error = move_private_file(&staged, &target, false).expect_err("must refuse");
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists, "{error}");
        assert_eq!(std::fs::read_to_string(&staged).unwrap(), "new");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "old");
    }
}
