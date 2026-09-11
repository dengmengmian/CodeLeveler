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

/// Whether this build can launch a Low-integrity child at all.
pub fn low_integrity_supported() -> bool {
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
    }
}
