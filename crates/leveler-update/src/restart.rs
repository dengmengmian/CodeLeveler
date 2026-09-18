//! Replace the process with the newly installed binary, preserving the
//! original invocation.
//!
//! Restart is the last step of a start-up or `/update` upgrade. It must be
//! called *after* the terminal has been restored (the TUI's alternate screen
//! would otherwise be inherited), so the CLI drives it once the UI has
//! returned.

use std::ffi::OsString;
use std::path::Path;
use std::process::Command;

use crate::error::UpdateError;

/// Restart the current executable with the arguments this process was given.
///
/// Returns only on failure; on success the process image is replaced (Unix) or
/// the new process is spawned and this one exits (Windows).
pub fn restart() -> Result<(), UpdateError> {
    let exe = std::env::current_exe().map_err(|e| UpdateError::Io(e.to_string()))?;
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    restart_with(&exe, &args)
}

/// Restart `exe` with `args` (not including argv[0]).
pub fn restart_with(exe: &Path, args: &[OsString]) -> Result<(), UpdateError> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // `exec` only returns if it failed: the process is replaced in place,
        // so no stale parent is left behind.
        let error = Command::new(exe).args(args).exec();
        Err(UpdateError::Io(format!("exec {}: {error}", exe.display())))
    }
    #[cfg(windows)]
    {
        // Windows has no `exec`; spawn the new image and retire this process.
        Command::new(exe)
            .args(args)
            .spawn()
            .map_err(|e| UpdateError::Io(format!("spawn {}: {e}", exe.display())))?;
        std::process::exit(0);
    }
}
