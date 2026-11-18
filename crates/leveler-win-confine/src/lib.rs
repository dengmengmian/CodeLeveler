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

        fn junction(link: &Path, target: &Path) {
            let output = std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "mklink /J failed: {}",
                String::from_utf8_lossy(&output.stdout)
            );
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
            junction(&link, &first);
            let target_identity = file_object_identity(&first).unwrap();
            assert_eq!(
                file_object_identity(&link).unwrap(),
                target_identity,
                "a junction is the object it reaches, not its own spelling"
            );
            std::fs::remove_dir(&link).unwrap();
            junction(&link, &second);
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
