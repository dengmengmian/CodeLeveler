//! `leveler-confine` — the Windows write-confinement launcher.
//!
//! The macOS and Linux sandboxes are argv wrappers (`sandbox-exec`, `bwrap`),
//! so `leveler-execution` confines a command by rewriting its argv and then
//! spawning it the one ordinary way. Windows has no such wrapper in the box,
//! and a token cannot be handed to `std::process::Command`, so this binary is
//! that wrapper: it lowers its own token to Low integrity and launches the real
//! command under it. Foreground and background commands therefore share one
//! spawn path, one Job Object and one confinement — the shape the other two
//! platforms already have.
//!
//! Reads stay unrestricted (Mandatory Integrity Control denies write-up, never
//! read-up). Writes land only where the caller has labelled a write root Low
//! (see `leveler_win_confine::apply_low_integrity_label`).
//!
//! Usage: `leveler-confine -- <program> [args...]`. The child inherits this
//! process's stdio, working directory and environment, and its exact exit code
//! is re-raised here.

#[cfg(not(windows))]
fn main() {
    eprintln!("leveler-confine: Windows-only; this host confines writes with seatbelt/bwrap");
    std::process::exit(2);
}

#[cfg(windows)]
fn main() {
    windows_main::run()
}

#[cfg(windows)]
mod windows_main {
    use std::ffi::{OsStr, OsString};
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::ptr::{null, null_mut};

    use windows_sys::Win32::Foundation::{
        HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation,
    };
    use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;
    use windows_sys::Win32::Security::{
        DuplicateTokenEx, GetLengthSid, SECURITY_ATTRIBUTES, SID_AND_ATTRIBUTES,
        SecurityImpersonation, SetTokenInformation, TOKEN_ADJUST_DEFAULT, TOKEN_ALL_ACCESS,
        TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
        TokenIntegrityLevel, TokenPrimary,
    };
    use windows_sys::Win32::System::Console::GetStdHandle;
    use windows_sys::Win32::System::Threading::ExitProcess;
    use windows_sys::Win32::System::Threading::{
        CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, GetCurrentProcess, GetExitCodeProcess,
        INFINITE, OpenProcessToken, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOW,
        WaitForSingleObject,
    };

    /// `SE_GROUP_INTEGRITY`.
    const SE_GROUP_INTEGRITY: u32 = 0x20;
    /// Exit code for "the launcher could not start the command at all".
    const LAUNCH_FAILED: u32 = 127;

    pub(super) fn run() {
        let argv: Vec<OsString> = std::env::args_os().skip(1).collect();
        let command: &[OsString] = match argv.first() {
            Some(first) if first == "--" => &argv[1..],
            _ => &argv[..],
        };
        let Some(program) = command.first() else {
            fail("usage: leveler-confine -- <program> [args...]");
        };

        let Some(executable) = resolve_executable(Path::new(program)) else {
            fail(&format!(
                "cannot find the program `{}` on PATH",
                program.to_string_lossy()
            ));
        };

        let token = match low_integrity_token() {
            Ok(token) => token,
            Err(message) => fail(&message),
        };

        let mut command_line = build_command_line(command);
        let executable_w = wide(executable.as_os_str());

        let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
        startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        startup.dwFlags = STARTF_USESTDHANDLES;
        startup.hStdInput = std_handle(-10);
        startup.hStdOutput = std_handle(-11);
        startup.hStdError = std_handle(-12);
        let mut info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

        // SAFETY: every pointer below is a live, NUL-terminated buffer owned by
        // this frame; `info` receives the new process and thread handles.
        let spawned = unsafe {
            CreateProcessAsUserW(
                token,
                executable_w.as_ptr(),
                command_line.as_mut_ptr(),
                null::<SECURITY_ATTRIBUTES>(),
                null::<SECURITY_ATTRIBUTES>(),
                1, // inherit the stdio handles above
                CREATE_UNICODE_ENVIRONMENT,
                null(),
                null(),
                &startup,
                &mut info,
            )
        };
        if spawned == 0 {
            fail(&format!(
                "cannot launch `{}` under Low integrity: {}",
                executable.display(),
                std::io::Error::last_os_error()
            ));
        }

        // SAFETY: `info.hProcess` is the process just created.
        unsafe { WaitForSingleObject(info.hProcess, INFINITE) };
        let mut code: u32 = 0;
        // SAFETY: same live handle; `code` is a local out-param.
        unsafe { GetExitCodeProcess(info.hProcess, &mut code) };
        // Re-raise the child's exact status, including NTSTATUS-shaped codes
        // like 0xC0000005 that do not survive a round trip through i32.
        // SAFETY: terminating this process is always sound.
        unsafe { ExitProcess(code) }
    }

    /// One of this process's standard handles, marked inheritable so the child
    /// gets the same pipe. A handle that arrived here by inheritance usually is
    /// already, but "usually" is how a command silently loses its output.
    fn std_handle(id: i32) -> HANDLE {
        // SAFETY: GetStdHandle takes a well-known constant and returns a handle
        // this process already owns.
        let handle = unsafe { GetStdHandle(id as u32) };
        if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
            // SAFETY: `handle` is a live handle owned by this process.
            unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) };
        }
        handle
    }

    fn fail(message: &str) -> ! {
        eprintln!("leveler-confine: {message}");
        // SAFETY: terminating this process is always sound.
        unsafe { ExitProcess(LAUNCH_FAILED) }
    }

    fn wide(value: &OsStr) -> Vec<u16> {
        value.encode_wide().chain(std::iter::once(0)).collect()
    }

    /// Duplicate this process's token and lower it to Low integrity. Lowering
    /// needs no privilege, and a token derived this way is accepted by
    /// `CreateProcessAsUserW` without `SeAssignPrimaryTokenPrivilege`.
    fn low_integrity_token() -> Result<HANDLE, String> {
        let mut current: HANDLE = INVALID_HANDLE_VALUE;
        // SAFETY: `current` is a local out-param for our own process token.
        let opened = unsafe {
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY | TOKEN_ADJUST_DEFAULT,
                &mut current,
            )
        };
        if opened == 0 {
            return Err(format!(
                "cannot open this process's token: {}",
                std::io::Error::last_os_error()
            ));
        }

        let mut lowered: HANDLE = INVALID_HANDLE_VALUE;
        // SAFETY: `current` is a live token handle; `lowered` is an out-param.
        let duplicated = unsafe {
            DuplicateTokenEx(
                current,
                TOKEN_ALL_ACCESS,
                null::<SECURITY_ATTRIBUTES>(),
                SecurityImpersonation,
                TokenPrimary,
                &mut lowered,
            )
        };
        if duplicated == 0 {
            return Err(format!(
                "cannot duplicate this process's token: {}",
                std::io::Error::last_os_error()
            ));
        }

        let sid_w = wide(OsStr::new(leveler_win_confine_low_sid()));
        let mut sid: *mut std::ffi::c_void = null_mut();
        // SAFETY: `sid_w` is NUL-terminated; `sid` receives an allocated SID
        // that stays valid until this process exits.
        if unsafe { ConvertStringSidToSidW(sid_w.as_ptr(), &mut sid) } == 0 {
            return Err(format!(
                "cannot build the Low integrity SID: {}",
                std::io::Error::last_os_error()
            ));
        }
        let label = TOKEN_MANDATORY_LABEL {
            Label: SID_AND_ATTRIBUTES {
                Sid: sid,
                Attributes: SE_GROUP_INTEGRITY,
            },
        };
        // SAFETY: `sid` is a live SID, so GetLengthSid reads a valid header.
        let size =
            std::mem::size_of::<TOKEN_MANDATORY_LABEL>() as u32 + unsafe { GetLengthSid(sid) };
        // SAFETY: `label` outlives the call and describes `size` bytes.
        let set = unsafe {
            SetTokenInformation(
                lowered,
                TokenIntegrityLevel,
                std::ptr::addr_of!(label).cast(),
                size,
            )
        };
        if set == 0 {
            return Err(format!(
                "cannot lower the token to Low integrity: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(lowered)
    }

    fn leveler_win_confine_low_sid() -> &'static str {
        "S-1-16-4096"
    }

    /// Resolve `program` the way `std::process::Command` does — PATH and
    /// PATHEXT, never the working directory. A confined command runs with the
    /// agent-writable workspace as its cwd, so searching it would let a planted
    /// `cargo.exe` win over the real one.
    fn resolve_executable(program: &Path) -> Option<PathBuf> {
        let has_directory = program.components().count() > 1;
        let roots: Vec<PathBuf> = if has_directory {
            vec![PathBuf::new()]
        } else {
            std::env::var_os("PATH")
                .map(|value| std::env::split_paths(&value).collect())
                .unwrap_or_default()
        };
        let extensions: Vec<OsString> = if program.extension().is_some() {
            vec![OsString::new()]
        } else {
            std::env::var_os("PATHEXT")
                .map(|value| {
                    value
                        .to_string_lossy()
                        .split(';')
                        .filter(|part| !part.is_empty())
                        .map(OsString::from)
                        .collect()
                })
                .unwrap_or_else(|| {
                    [".COM", ".EXE", ".BAT", ".CMD"]
                        .into_iter()
                        .map(OsString::from)
                        .collect()
                })
        };
        for root in roots {
            let base = root.join(program);
            for extension in &extensions {
                let candidate = if extension.is_empty() {
                    base.clone()
                } else {
                    let mut name = base.clone().into_os_string();
                    name.push(extension);
                    PathBuf::from(name)
                };
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
        None
    }

    /// MSVC argv quoting — the same rules `std::process::Command` applies, so a
    /// confined command line is byte-identical to the unconfined one. The one
    /// departure is `cmd.exe`'s own tail, which
    /// [`leveler_win_confine::cmd_tail_start`] identifies and which is emitted
    /// exactly as the caller wrote it.
    fn build_command_line(command: &[OsString]) -> Vec<u16> {
        let lossy: Vec<String> = command[1..]
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();
        let tail = leveler_win_confine::cmd_tail_start(&command[0].to_string_lossy(), &lossy)
            .map(|index| index + 1);

        let mut line: Vec<u16> = Vec::new();
        for (index, argument) in command.iter().enumerate() {
            if index > 0 {
                line.push(u16::from(b' '));
            }
            if tail.is_some_and(|tail| index >= tail) {
                line.extend(argument.encode_wide());
            } else {
                append_argument(&mut line, argument, index == 0);
            }
        }
        line.push(0);
        line
    }

    fn append_argument(line: &mut Vec<u16>, argument: &OsStr, force_quotes: bool) {
        let units: Vec<u16> = argument.encode_wide().collect();
        let quote = force_quotes
            || units.is_empty()
            || units
                .iter()
                .any(|unit| *unit == u16::from(b' ') || *unit == u16::from(b'\t'));
        if quote {
            line.push(u16::from(b'"'));
        }
        let mut backslashes = 0usize;
        for unit in units {
            if unit == u16::from(b'\\') {
                backslashes += 1;
            } else {
                if unit == u16::from(b'"') {
                    line.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes + 1));
                }
                backslashes = 0;
            }
            line.push(unit);
        }
        if quote {
            line.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes));
            line.push(u16::from(b'"'));
        }
    }
}
