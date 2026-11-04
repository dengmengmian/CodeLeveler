//! Operating-system wakeup registration.
//!
//! The schedule store is the authority for *what* should run and *when*. The
//! OS registration is only an opportunity to be started: launchd or Task
//! Scheduler decides whether the machine is awake, when the job actually
//! starts, and what it does with a missed interval. Nothing in this module
//! reads or writes an execution fact.
//!
//! Registration is opt-in. Enabling writes the OS record, loading it into the
//! current user's session, and leaving it there; disabling removes the record
//! so the job cannot be loaded at the next login. The schedule definitions,
//! occurrences and run history are never touched by either operation.
//!
//! The wakeup driver is the ordinary short-lived `schedule dispatch` command.
//! It claims nothing when no slot is due and exits, so an idle registration
//! costs one short process per interval and no resident runtime.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How often the OS registration wakes the dispatcher.
///
/// launchd and Task Scheduler are minute-granularity schedulers. A shorter
/// interval would only spawn more processes; the schedule's own due check is
/// what decides whether work exists. The default misfire grace absorbs several
/// missed wakes before a slot is classified as missed.
pub const WAKE_INTERVAL: Duration = Duration::from_secs(60);

/// One desired OS registration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WakeupSpec {
    /// The CodeLeveler home this registration belongs to. It is part of the
    /// label, so independent homes never overwrite each other's job.
    pub home: PathBuf,
    /// Absolute path to the `leveler` executable to wake.
    pub executable: PathBuf,
    /// Arguments after the executable.
    pub arguments: Vec<String>,
    /// Non-secret environment the job needs. Secrets are never copied into an
    /// OS registration.
    pub environment: BTreeMap<String, String>,
    pub interval: Duration,
    /// Directory for the job's own stdout/stderr.
    pub log_dir: PathBuf,
}

impl WakeupSpec {
    /// Reject a registration that could not run or that would put an
    /// unexpected value into an OS-owned file.
    pub fn validate(&self) -> Result<(), WakeupError> {
        if !self.home.is_absolute() {
            return Err(WakeupError::Invalid(
                "the Leveler home must be an absolute path".into(),
            ));
        }
        if !self.executable.is_absolute() {
            return Err(WakeupError::Invalid(
                "the wakeup executable must be an absolute path".into(),
            ));
        }
        self.executable
            .metadata()
            .map_err(|error| {
                WakeupError::Invalid(format!(
                    "the wakeup executable `{}` is not accessible: {error}",
                    self.executable.display()
                ))
            })?;
        if !self.executable.is_file() {
            return Err(WakeupError::Invalid(format!(
                "the wakeup executable `{}` is not a file",
                self.executable.display()
            )));
        }
        if self.interval < Duration::from_secs(10) {
            return Err(WakeupError::Invalid(
                "the wakeup interval must be at least 10 seconds".into(),
            ));
        }
        for argument in &self.arguments {
            if argument.contains('\0') {
                return Err(WakeupError::Invalid(
                    "a wakeup argument contains a NUL byte".into(),
                ));
            }
        }
        for (key, value) in &self.environment {
            if key.is_empty() || key.contains(['=', '\0']) {
                return Err(WakeupError::Invalid(format!(
                    "invalid wakeup environment key `{key}`"
                )));
            }
            if value.contains('\0') {
                return Err(WakeupError::Invalid(format!(
                    "wakeup environment value for `{key}` contains a NUL byte"
                )));
            }
        }
        Ok(())
    }
}

/// What the OS says is registered, independent of what the schedule store
/// wants. `None` fields mean the OS record exists but did not expose the fact.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WakeupRegistration {
    pub executable: PathBuf,
    pub arguments: Vec<String>,
    pub interval: Option<Duration>,
    pub environment: BTreeMap<String, String>,
    pub working_directory: Option<PathBuf>,
}

/// The observed OS registration state.
#[derive(Debug, Clone)]
pub struct WakeupStatus {
    /// `launchd`, `task-scheduler`, or `unsupported`.
    pub backend: &'static str,
    pub supported: bool,
    pub label: String,
    /// Where the OS record lives, when the platform has a file record.
    pub registration_path: Option<PathBuf>,
    /// The record exists on disk.
    pub registered: bool,
    /// The platform accepted the record into the current session.
    pub loaded: bool,
    pub registration: Option<WakeupRegistration>,
    /// launchd's last exit status, when reported.
    pub last_exit_code: Option<i32>,
    /// launchd's run count, when reported.
    pub run_count: Option<u64>,
    pub detail: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum WakeupError {
    #[error("OS wakeup is not supported on this platform: {0}")]
    Unsupported(String),
    #[error("invalid OS wakeup registration: {0}")]
    Invalid(String),
    #[error("OS wakeup I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{program} failed ({status}): {stderr}")]
    Command {
        program: String,
        status: String,
        stderr: String,
    },
}

impl WakeupError {
    fn command(program: impl Into<String>, status: impl std::fmt::Display, stderr: impl Into<String>) -> Self {
        Self::Command {
            program: program.into(),
            status: status.to_string(),
            stderr: stderr.into(),
        }
    }
}

/// The compile-time backend name, safe to print even when unsupported.
pub const fn backend_name() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "launchd"
    }
    #[cfg(windows)]
    {
        "task-scheduler"
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        "unsupported"
    }
}

/// Whether this build can register a real OS wakeup.
pub const fn is_supported() -> bool {
    cfg!(any(target_os = "macos", windows))
}

/// A stable, filesystem-safe job name derived from the Leveler home.
///
/// A single user may have several homes (a test fixture, a custom
/// `LEVELER_HOME`). Hashing the home keeps their registrations independent
/// instead of letting one enable overwrite another's job.
pub fn label_for(home: &Path) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in home.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("com.leveler.schedule.{hash:016x}")
}

/// Enable or refresh the OS registration. Idempotent.
pub async fn enable(spec: &WakeupSpec) -> Result<WakeupStatus, WakeupError> {
    spec.validate()?;
    platform::enable(spec).await
}

/// Remove the OS registration so it cannot be loaded again at login.
/// Idempotent. Schedule data is untouched.
pub async fn disable(home: &Path) -> Result<WakeupStatus, WakeupError> {
    platform::disable(home).await
}

/// Query the actual OS state. Does not modify anything.
pub async fn status(home: &Path) -> Result<WakeupStatus, WakeupError> {
    platform::status(home).await
}

/// The user's LaunchAgents directory on macOS. Kept here so status and doctor
/// name the same path.
#[cfg(target_os = "macos")]
pub fn launch_agents_dir() -> Result<PathBuf, WakeupError> {
    let home = leveler_core::environment()
        .var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| !home.as_os_str().is_empty())
        .ok_or_else(|| {
            WakeupError::Invalid(
                "HOME is not set; cannot locate the user's LaunchAgents directory".into(),
            )
        })?;
    Ok(home.join("Library").join("LaunchAgents"))
}

/// XML-escape a property-list string. launchd reads a real plist; paths with
/// spaces, Unicode or `&<>` must survive unchanged.
pub(crate) fn xml_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            ch => out.push(ch),
        }
    }
    out
}

/// Quote one argument for Task Scheduler's `<Arguments>` string using the
/// `CommandLineToArgvW` backslash/quote rules. The executable path is not part
/// of this string; it is the separate `<Command>` element.
#[cfg(any(windows, test))]
pub(crate) fn windows_quote_argument(argument: &str) -> String {
    if !argument.is_empty()
        && !argument
            .chars()
            .any(|ch| ch == ' ' || ch == '\t' || ch == '"')
    {
        return argument.to_owned();
    }
    let mut out = String::with_capacity(argument.len() + 2);
    out.push('"');
    let mut backslashes = 0_usize;
    for ch in argument.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            ch => {
                out.push_str(&"\\".repeat(backslashes));
                out.push(ch);
                backslashes = 0;
            }
        }
    }
    out.push_str(&"\\".repeat(backslashes * 2));
    out.push('"');
    out
}

/// Decode console output from `schtasks`. XML export is UTF-16LE (usually
/// with a BOM); list output uses the console code page. `from_utf8_lossy`
/// alone would turn UTF-16 into NUL-interleaved text and make every XML field
/// unparseable.
#[cfg(any(windows, test))]
pub(crate) fn windows_decode_output(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return decode_utf16le(rest);
    }
    if bytes.len() >= 4 && bytes.len() % 2 == 0 {
        let high_nuls = bytes
            .iter()
            .skip(1)
            .step_by(2)
            .filter(|byte| **byte == 0)
            .count();
        // ASCII-dominated UTF-16LE puts a NUL in the high byte of most units.
        if high_nuls * 4 >= bytes.len() {
            return decode_utf16le(bytes);
        }
    }
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(any(windows, test))]
fn decode_utf16le(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    String::from_utf16_lossy(&units)
}

/// Task Scheduler XML. Pure so the shape can be tested on every host; only
/// `win_task::enable` executes `schtasks`.
#[cfg(any(windows, test))]
pub(crate) fn task_scheduler_xml(spec: &WakeupSpec, label: &str, user: &str) -> String {
    let arguments = spec
        .arguments
        .iter()
        .map(|argument| windows_quote_argument(argument))
        .collect::<Vec<_>>()
        .join(" ");
    let minutes = (spec.interval.as_secs() / 60).max(1);
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-16\"?>\n\
         <Task version=\"1.4\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\n\
         \x20 <RegistrationInfo>\n\
         \x20   <Description>CodeLeveler scheduled actions for {home}</Description>\n\
         \x20   <URI>\\{label}</URI>\n\
         \x20 </RegistrationInfo>\n\
         \x20 <Triggers>\n\
         \x20   <TimeTrigger>\n\
         \x20     <Repetition>\n\
         \x20       <Interval>PT{minutes}M</Interval>\n\
         \x20       <StopAtDurationEnd>false</StopAtDurationEnd>\n\
         \x20     </Repetition>\n\
         \x20     <StartBoundary>2000-01-01T00:00:00</StartBoundary>\n\
         \x20     <Enabled>true</Enabled>\n\
         \x20   </TimeTrigger>\n\
         \x20   <LogonTrigger>\n\
         \x20     <Enabled>true</Enabled>\n\
         \x20     <Repetition>\n\
         \x20       <Interval>PT{minutes}M</Interval>\n\
         \x20       <StopAtDurationEnd>false</StopAtDurationEnd>\n\
         \x20     </Repetition>\n\
         \x20   </LogonTrigger>\n\
         \x20 </Triggers>\n\
         \x20 <Principals>\n\
         \x20   <Principal id=\"Author\">\n\
         \x20     <UserId>{user}</UserId>\n\
         \x20     <LogonType>InteractiveToken</LogonType>\n\
         \x20     <RunLevel>LeastPrivilege</RunLevel>\n\
         \x20   </Principal>\n\
         \x20 </Principals>\n\
         \x20 <Settings>\n\
         \x20   <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>\n\
         \x20   <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>\n\
         \x20   <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>\n\
         \x20   <AllowHardTerminate>false</AllowHardTerminate>\n\
         \x20   <StartWhenAvailable>true</StartWhenAvailable>\n\
         \x20   <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>\n\
         \x20   <IdleSettings>\n\
         \x20     <StopOnIdleEnd>false</StopOnIdleEnd>\n\
         \x20     <RestartOnIdle>false</RestartOnIdle>\n\
         \x20   </IdleSettings>\n\
         \x20   <AllowStartOnDemand>true</AllowStartOnDemand>\n\
         \x20   <Enabled>true</Enabled>\n\
         \x20   <Hidden>false</Hidden>\n\
         \x20   <RunOnlyIfIdle>false</RunOnlyIfIdle>\n\
         \x20   <WakeToRun>false</WakeToRun>\n\
         \x20   <ExecutionTimeLimit>PT2H</ExecutionTimeLimit>\n\
         \x20   <Priority>7</Priority>\n\
         \x20 </Settings>\n\
         \x20 <Actions Context=\"Author\">\n\
         \x20   <Exec>\n\
         \x20     <Command>{executable}</Command>\n\
         \x20     <Arguments>{arguments}</Arguments>\n\
         \x20     <WorkingDirectory>{workdir}</WorkingDirectory>\n\
         \x20   </Exec>\n\
         \x20 </Actions>\n\
         </Task>\n",
        home = xml_escape(&spec.home.to_string_lossy()),
        label = xml_escape(label),
        user = xml_escape(user),
        executable = xml_escape(&spec.executable.to_string_lossy()),
        arguments = xml_escape(&arguments),
        workdir = xml_escape(&spec.home.to_string_lossy()),
    )
}

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as platform;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as platform;

#[cfg(not(any(target_os = "macos", windows)))]
mod unsupported;
#[cfg(not(any(target_os = "macos", windows)))]
use unsupported as platform;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_label_is_stable_and_home_specific() {
        let a = label_for(Path::new("/Users/u/.leveler"));
        assert_eq!(a, label_for(Path::new("/Users/u/.leveler")));
        assert_ne!(a, label_for(Path::new("/Users/u/other-home")));
        assert!(a.starts_with("com.leveler.schedule."));
        assert!(a.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '.' || ch == '-'));
    }

    #[test]
    fn plist_strings_escape_every_xml_metacharacter() {
        assert_eq!(
            xml_escape("a & b < c > d \" e ' f"),
            "a &amp; b &lt; c &gt; d &quot; e &apos; f"
        );
        assert_eq!(xml_escape("/tmp/a b/中文"), "/tmp/a b/中文");
    }

    #[test]
    fn a_spec_rejects_a_relative_executable() {
        let spec = WakeupSpec {
            home: PathBuf::from("/tmp/home"),
            executable: PathBuf::from("leveler"),
            arguments: Vec::new(),
            environment: BTreeMap::new(),
            interval: WAKE_INTERVAL,
            log_dir: PathBuf::from("/tmp/home/logs"),
        };
        assert!(matches!(spec.validate(), Err(WakeupError::Invalid(_))));
    }

    #[test]
    fn windows_arguments_follow_command_line_quoting() {
        assert_eq!(windows_quote_argument("dispatch"), "dispatch");
        assert_eq!(
            windows_quote_argument(r"C:\Users\A B\configs"),
            r#""C:\Users\A B\configs""#
        );
        assert_eq!(windows_quote_argument(r#"say "hi""#), r#""say \"hi\"""#);
        // A trailing backslash must be doubled so it does not escape the
        // closing quote.
        assert_eq!(windows_quote_argument(r"C:\a b\"), r#""C:\a b\\""#);
    }

    #[cfg(any(windows, test))]
    #[test]
    fn task_scheduler_output_is_decoded_from_utf16_or_utf8() {
        let xml = "<?xml version=\"1.0\" encoding=\"UTF-16\"?><Task>中文</Task>";
        let mut utf16 = vec![0xFF, 0xFE];
        utf16.extend(xml.encode_utf16().flat_map(u16::to_le_bytes));
        assert_eq!(windows_decode_output(&utf16), xml);
        // Without a BOM, the NUL high bytes still identify UTF-16LE.
        let bare: Vec<u8> = xml.encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(windows_decode_output(&bare), xml);
        assert_eq!(windows_decode_output(b"plain ascii"), "plain ascii");
    }

    #[cfg(any(windows, test))]
    #[test]
    fn the_task_xml_names_the_executable_arguments_and_home() {
        let spec = WakeupSpec {
            home: PathBuf::from(r"C:\Users\A B\.leveler"),
            executable: PathBuf::from(r"C:\Program Files\Leveler\leveler.exe"),
            arguments: vec![
                "schedule".into(),
                "dispatch".into(),
                "--source".into(),
                "os".into(),
                "--config-dir".into(),
                r"C:\Users\A B\configs".into(),
            ],
            environment: BTreeMap::new(),
            interval: WAKE_INTERVAL,
            log_dir: PathBuf::from(r"C:\Users\A B\.leveler\logs\schedule"),
        };
        let xml = task_scheduler_xml(&spec, "CodeLeveler-Schedule.abc", r"DOMAIN\user");
        assert!(xml.contains("<Command>C:\\Program Files\\Leveler\\leveler.exe</Command>"));
        assert!(xml.contains(
            "<Arguments>schedule dispatch --source os --config-dir &quot;C:\\Users\\A B\\configs&quot;</Arguments>"
        ));
        assert!(xml.contains("<WorkingDirectory>C:\\Users\\A B\\.leveler</WorkingDirectory>"));
        assert!(xml.contains("<Interval>PT1M</Interval>"));
        assert!(xml.contains("<LogonType>InteractiveToken</LogonType>"));
        assert!(xml.contains("<RunLevel>LeastPrivilege</RunLevel>"));
        assert!(xml.contains("<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>"));
    }
}
