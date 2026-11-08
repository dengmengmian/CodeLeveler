//! Safe OS process identity for background tasks.
//!
//! A PID is not an identity: it is reused. A recovered runtime may only treat
//! a historical process as the same task when the platform can also reproduce
//! a start token captured at spawn. When it cannot, the only honest answer is
//! Unknown, never a signal to the PID.

/// What the current OS says about a historical process identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessIdentity {
    Matches,
    Exited,
    Mismatch { current: String },
    Unknown,
}

/// Capture a platform start token for `pid`, if the OS exposes one.
///
/// The macOS branch calls `proc_pidinfo` with a POD `proc_bsdinfo` buffer and
/// checks the returned byte count before reading it; no pointer is retained.
#[allow(unsafe_code)]
pub fn process_start_identity(pid: u32) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        use nix::libc;
        // proc_pidinfo avoids spawning `ps` (which a confined host may deny).
        // The start time is wall-clock seconds + microseconds since boot.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let written = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDTBSDINFO,
                0,
                &mut info as *mut _ as *mut libc::c_void,
                size,
            )
        };
        if written == size {
            return Some(format!(
                "macos:{}:{}",
                info.pbi_start_tvsec, info.pbi_start_tvusec
            ));
        }
        None
    }
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let rest = stat.rsplit_once(')')?.1;
        let fields: Vec<&str> = rest.split_whitespace().collect();
        fields.get(19).map(|s| s.to_string())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = pid;
        None
    }
}

/// Compare a historical `(pid, start_token)` with the current OS.
pub fn verify_process_identity(pid: u32, expected_start: &str) -> ProcessIdentity {
    match process_start_identity(pid) {
        None => ProcessIdentity::Exited,
        Some(current) if current == expected_start => ProcessIdentity::Matches,
        Some(current) => ProcessIdentity::Mismatch { current },
    }
}
