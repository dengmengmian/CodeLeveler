//! macOS probes and the static AppleScript plans.
//!
//! Nothing here launches an application during discovery. Apple Events run
//! only when an explicitly approved action asks for one, and every read of
//! user content happens inside a static script reading a private file.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use leveler_execution::{CommandRunner, ProcessRequest};
use tokio_util::sync::CancellationToken;

use crate::classify::{MailProbe, macos_mail};
use crate::discover::{CapabilityReport, DiscoveryReport, PermissionState, SystemToolReport};
use crate::model::{ActionId, ActionPlan, NativeError, ProviderId};
use crate::protocol::{Payload, write_private};

const MAIL_APP: &str = "/System/Applications/Mail.app";
const OSASCRIPT: &str = "/usr/bin/osascript";
const PGREP: &str = "/usr/bin/pgrep";

/// A static account probe. It reads only a count, never an address or a
/// credential, and runs only when Mail is already running.
const ACCOUNT_PROBE: &str = r#"
tell application "Mail"
	set enabledCount to 0
	repeat with acct in every account
		try
			if enabled of acct is true then set enabledCount to enabledCount + 1
		end try
	end repeat
	return enabledCount
end tell
"#;

const DRAFT_SCRIPT: &str = r#"
on run argv
	if (count of argv) is not 1 then error "payload directory missing" number 2
	set folderPath to item 1 of argv
	set toAddress to my readField(folderPath, "to.txt")
	set subjectText to my readField(folderPath, "subject.txt")
	set bodyText to my readField(folderPath, "body.txt")
	tell application "Mail"
		set draftMessage to make new outgoing message with properties {subject:subjectText, content:bodyText, visible:false}
		tell draftMessage
			make new to recipient at end of to recipients with properties {address:toAddress}
			save
		end tell
	end tell
	return "DRAFT_CREATED"
end run

on readField(folderPath, fileName)
	set filePath to folderPath & "/" & fileName
	try
		return read (POSIX file filePath) as «class utf8»
	on error number -39
		return ""
	end try
end readField
"#;

const COMPOSE_SCRIPT: &str = r#"
on run argv
	if (count of argv) is not 1 then error "payload directory missing" number 2
	set folderPath to item 1 of argv
	set toAddress to my readField(folderPath, "to.txt")
	set subjectText to my readField(folderPath, "subject.txt")
	set bodyText to my readField(folderPath, "body.txt")
	tell application "Mail"
		set composeMessage to make new outgoing message with properties {subject:subjectText, content:bodyText, visible:true}
		tell composeMessage
			make new to recipient at end of to recipients with properties {address:toAddress}
		end tell
		activate
	end tell
	return "COMPOSE_OPENED"
end run

on readField(folderPath, fileName)
	set filePath to folderPath & "/" & fileName
	try
		return read (POSIX file filePath) as «class utf8»
	on error number -39
		return ""
	end try
end readField
"#;

const SEND_SCRIPT: &str = r#"
on run argv
	if (count of argv) is not 1 then error "payload directory missing" number 2
	set folderPath to item 1 of argv
	set toAddress to my readField(folderPath, "to.txt")
	set subjectText to my readField(folderPath, "subject.txt")
	set bodyText to my readField(folderPath, "body.txt")
	tell application "Mail"
		set sendMessage to make new outgoing message with properties {subject:subjectText, content:bodyText, visible:false}
		tell sendMessage
			make new to recipient at end of to recipients with properties {address:toAddress}
			send
		end tell
	end tell
	return "SEND_SUBMITTED"
end run

on readField(folderPath, fileName)
	set filePath to folderPath & "/" & fileName
	try
		return read (POSIX file filePath) as «class utf8»
	on error number -39
		return ""
	end try
end readField
"#;

/// Scan the newest 200 Sent messages. A match is a Sent record, not a delivery
/// receipt; a miss is not proof that nothing was sent.
const SENT_CHECK_SCRIPT: &str = r#"
on run argv
	if (count of argv) is not 1 then error "payload directory missing" number 2
	set folderPath to item 1 of argv
	set toAddress to my readField(folderPath, "to.txt")
	set subjectText to my readField(folderPath, "subject.txt")
	set foundCount to 0
	set scannedCount to 0
	tell application "Mail"
		set sentBox to sent mailbox
		set totalCount to count of messages of sentBox
		set scanLimit to 200
		if totalCount < scanLimit then set scanLimit to totalCount
		repeat with messageIndex from 1 to scanLimit
			set scannedCount to scannedCount + 1
			try
				set candidate to message messageIndex of sentBox
				if subject of candidate is subjectText then
					repeat with recipientRef in to recipients of candidate
						try
							ignoring case
								if (address of recipientRef as string) is toAddress then
									set foundCount to foundCount + 1
									exit repeat
								end if
							end ignoring
						end try
					end repeat
				end if
			end try
		end repeat
	end tell
	return "SENT_FOUND " & foundCount & " SCANNED " & scannedCount & " TOTAL " & totalCount
end run

on readField(folderPath, fileName)
	set filePath to folderPath & "/" & fileName
	try
		return read (POSIX file filePath) as «class utf8»
	on error number -39
		return ""
	end try
end readField
"#;

pub(super) async fn discover(runner: &CommandRunner, probes: &PermissionProbes) -> DiscoveryReport {
    let installed = Path::new(MAIL_APP).exists();
    let (permission, permission_detail) = bounded_permission_probe(
        probes,
        automation_permission,
        PERMISSION_PROBE_TIMEOUT,
    )
    .await;
    let (running, running_error) = if installed {
        match run_probe(runner, PGREP, &["-x", "Mail"], Duration::from_secs(5)).await {
            Ok(output) => match output.exit_code {
                Some(0) => (Some(true), None),
                Some(1) => (Some(false), None),
                other => (
                    None,
                    Some(format!(
                        "pgrep exited with {other:?}: {}",
                        first_line(&output.stderr)
                    )),
                ),
            },
            Err(error) => (None, Some(error)),
        }
    } else {
        (Some(false), None)
    };

    let mut accounts = None;
    let mut probe_error = running_error;
    if installed && permission == PermissionState::Granted && running == Some(true) {
        match run_probe(
            runner,
            OSASCRIPT,
            &["-e", ACCOUNT_PROBE],
            Duration::from_secs(15),
        )
        .await
        {
            Ok(output) if output.exit_code == Some(0) => {
                match output.stdout.trim().parse::<u32>() {
                    Ok(count) => accounts = Some(count),
                    Err(_) => {
                        probe_error = Some(
                            "the account probe returned no count; account configuration is unknown"
                                .into(),
                        )
                    }
                }
            }
            Ok(output) => {
                probe_error = Some(format!(
                    "the account probe exited with {:?}: {}",
                    output.exit_code,
                    first_line(&output.stderr)
                ))
            }
            Err(error) => probe_error = Some(error),
        }
    }

    let capability: CapabilityReport = macos_mail(&MailProbe {
        installed,
        install_path: MAIL_APP.to_string(),
        permission,
        permission_detail,
        running,
        accounts,
        probe_error,
    });

    let mut notes = vec![
        "Discovery never launches an application and never requests automation consent."
            .to_string(),
        "Apple Events authorization belongs to the process that hosts leveler (the terminal or \
         the desktop application), not to the osascript helper alone."
            .to_string(),
        "The macOS default mailto handler is not probed; `default_mail_handler: null` means \
         not reported, not that no handler is registered."
            .to_string(),
    ];
    if !installed {
        notes.push(format!("Apple Mail was not found at {MAIL_APP}"));
    }
    if permission == PermissionState::NotDetermined {
        notes.push(
            "The first approved action asks for Automation consent; nothing is sent before the \
             user grants it."
                .to_string(),
        );
    }

    DiscoveryReport {
        platform: "macos",
        supported: true,
        default_mail_handler: None,
        system_tools: system_tools(),
        capabilities: vec![capability],
        notes,
    }
}

fn system_tools() -> Vec<SystemToolReport> {
    [
        (
            "osascript",
            OSASCRIPT,
            "runs the static AppleScript action plans",
        ),
        ("pgrep", PGREP, "observes whether Mail is already running"),
        (
            "open",
            "/usr/bin/open",
            "opens files and URLs without a shell",
        ),
        (
            "screencapture",
            "/usr/sbin/screencapture",
            "screen capture for Computer Use",
        ),
    ]
    .into_iter()
    .map(|(id, path, purpose)| SystemToolReport {
        id: id.to_string(),
        installed: Path::new(path).exists(),
        path: Some(path.to_string()),
        reason: if Path::new(path).exists() {
            Some(purpose.to_string())
        } else {
            Some(format!("{purpose}; not present at {path}"))
        },
    })
    .collect()
}

async fn run_probe(
    runner: &CommandRunner,
    program: &str,
    args: &[&str],
    timeout: Duration,
) -> Result<leveler_execution::ProcessOutput, String> {
    let mut request = ProcessRequest::new(
        program,
        args.iter().map(|arg| (*arg).to_string()).collect(),
        std::env::temp_dir(),
    );
    request.timeout = timeout;
    request.max_output_bytes = 64 * 1024;
    runner
        .run(request, CancellationToken::new())
        .await
        .map_err(|error| format!("{program} could not run: {error}"))
}

fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("no diagnostic")
        .chars()
        .take(200)
        .collect()
}

pub(super) fn plan(action: ActionId, payload: Payload) -> Result<ActionPlan, NativeError> {
    let directory = tempfile::Builder::new()
        .prefix("leveler-native-")
        .tempdir()
        .map_err(|error| NativeError::Prepare {
            action: action.as_str(),
            detail: format!("could not create a private payload directory: {error}"),
        })?;
    let write = |name: &str, value: &str| {
        write_private(&directory.path().join(name), value.as_bytes()).map_err(|error| {
            NativeError::Prepare {
                action: action.as_str(),
                detail: format!("could not write {name}: {error}"),
            }
        })
    };
    match &payload {
        Payload::Message { to, subject, body } => {
            write("to.txt", to)?;
            write("subject.txt", subject)?;
            write("body.txt", body)?;
        }
        Payload::Check { to, subject } => {
            write("to.txt", to)?;
            write("subject.txt", subject)?;
        }
    }

    let (script, timeout) = match action {
        ActionId::MailDraftCreate => (DRAFT_SCRIPT, Duration::from_secs(90)),
        ActionId::MailSendOpenUi => (COMPOSE_SCRIPT, Duration::from_secs(90)),
        ActionId::MailSendSubmit => (SEND_SCRIPT, Duration::from_secs(120)),
        ActionId::MailSentCheck => (SENT_CHECK_SCRIPT, Duration::from_secs(240)),
    };
    let folder = directory.path().to_string_lossy().to_string();
    Ok(ActionPlan {
        action,
        provider: ProviderId::MacosMail,
        program: OSASCRIPT.to_string(),
        args: vec!["-e".to_string(), script.to_string(), folder],
        cwd: directory.path().to_path_buf(),
        timeout,
        _payload: directory,
    })
}

/// The synchronous TCC check can wait on tccd while a consent prompt for the
/// same target is unresolved. Discovery must report that state, not hang, so
/// the FFI runs on a detached thread and the async side waits at most
/// `PERMISSION_PROBE_TIMEOUT`. macOS cannot cancel that thread, so a probe
/// that outlives its caller is *abandoned*, not stopped: it keeps one slot
/// until tccd answers. One replacement is allowed once the abandoned call has
/// had its whole timeout again, because a grant made after the original call
/// started must still be observable. Further probes are refused while two
/// calls are outstanding, so a permanently broken tccd leaks at most two
/// threads instead of one per discovery. State is per capability handle; a
/// fresh process is never blocked by an old one.
const PERMISSION_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_ABANDONED_PERMISSION_PROBES: usize = 1;

#[derive(Debug, Clone, Default)]
pub(super) struct PermissionProbes {
    state: Arc<Mutex<ProbeState>>,
}

#[derive(Debug, Default)]
struct ProbeState {
    next_generation: u64,
    active: Option<u64>,
    abandoned: usize,
    /// When the active probe's caller gave up, and how long that probe was
    /// given. A replacement waits one more full window for tccd to answer.
    timed_out: Option<(Instant, Duration)>,
}

impl PermissionProbes {
    /// Reserve the one active probe slot. `Err` is a fail-closed `Unknown`
    /// reason: another call is still outstanding and may yet answer.
    fn admit(&self) -> Result<u64, String> {
        let mut state = self.lock();
        if state.active.is_some() {
            let replaceable = state
                .timed_out
                .is_some_and(|(at, timeout)| at.elapsed() >= timeout);
            if !replaceable {
                return Err(
                    "another automation permission probe is still waiting for a system response"
                        .into(),
                );
            }
            if state.abandoned >= MAX_ABANDONED_PERMISSION_PROBES {
                return Err(format!(
                    "{} automation permission probes are still waiting for a system response; \
                     refusing to start another",
                    state.abandoned + 1
                ));
            }
            state.abandoned += 1;
            state.timed_out = None;
        }
        state.next_generation += 1;
        state.active = Some(state.next_generation);
        Ok(state.next_generation)
    }

    fn note_timed_out(&self, generation: u64, timeout: Duration) {
        let mut state = self.lock();
        if state.active == Some(generation) {
            state.timed_out = Some((Instant::now(), timeout));
        }
    }

    /// Called exactly once per probe thread, including on panic via
    /// [`ProbeGuard`]. A generation that is no longer active was already
    /// counted as abandoned when its replacement started.
    fn finish(&self, generation: u64) {
        let mut state = self.lock();
        if state.active == Some(generation) {
            state.active = None;
            state.timed_out = None;
        } else {
            state.abandoned = state.abandoned.saturating_sub(1);
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ProbeState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

struct ProbeGuard {
    probes: PermissionProbes,
    generation: u64,
    armed: bool,
}

impl ProbeGuard {
    fn finish_now(&mut self) {
        if self.armed {
            self.armed = false;
            self.probes.finish(self.generation);
        }
    }
}

impl Drop for ProbeGuard {
    fn drop(&mut self) {
        self.finish_now();
    }
}

async fn bounded_permission_probe(
    probes: &PermissionProbes,
    probe: fn() -> (PermissionState, String),
    timeout: Duration,
) -> (PermissionState, String) {
    let generation = match probes.admit() {
        Ok(generation) => generation,
        Err(reason) => return (PermissionState::Unknown, reason),
    };
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let mut guard = ProbeGuard {
        probes: probes.clone(),
        generation,
        armed: true,
    };
    let spawned = std::thread::Builder::new()
        .name("tcc-permission-probe".into())
        .spawn(move || {
            let result = probe();
            guard.finish_now();
            let _ = sender.send(result);
        });
    if let Err(error) = spawned {
        return (
            PermissionState::Unknown,
            format!("could not start the automation permission probe: {error}"),
        );
    }
    match tokio::time::timeout(timeout, receiver).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => (
            PermissionState::Unknown,
            "the automation permission probe ended without a result".into(),
        ),
        Err(_) => {
            probes.note_timed_out(generation, timeout);
            (
                PermissionState::Unknown,
                format!(
                    "AEDeterminePermissionToAutomateTarget did not return within {}ms; a consent \
                     prompt may still be waiting",
                    timeout.as_millis()
                ),
            )
        }
    }
}

/// `AEDeterminePermissionToAutomateTarget` with `askUserIfNeeded = false`.
/// It reports the current grant without prompting; the actual action is what
/// asks the user when consent is still undetermined.
fn automation_permission() -> (PermissionState, String) {
    #[repr(C)]
    struct AeDesc {
        descriptor_type: u32,
        data_handle: *mut std::ffi::c_void,
    }

    // Apple Events / ApplicationServices is the documented OS boundary for
    // this query. The call is read-only: askUserIfNeeded is false, so it can
    // neither prompt nor change a TCC decision.
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AECreateDesc(
            type_code: u32,
            data: *const std::ffi::c_void,
            data_size: isize,
            result: *mut AeDesc,
        ) -> i32;
        fn AEDisposeDesc(desc: *mut AeDesc) -> i32;
        fn AEDeterminePermissionToAutomateTarget(
            target: *const AeDesc,
            event_class: u32,
            event_id: u32,
            ask_user: u8,
        ) -> i32;
    }

    const TYPE_APPLICATION_BUNDLE_ID: u32 = u32::from_be_bytes(*b"bund");
    const TYPE_WILD_CARD: u32 = u32::from_be_bytes(*b"****");
    const MAIL_BUNDLE_ID: &[u8] = b"com.apple.mail";

    let mut target = AeDesc {
        descriptor_type: 0,
        data_handle: std::ptr::null_mut(),
    };
    // SAFETY: the descriptor is uninitialized stack memory owned by this
    // function, `data` points to a live byte slice of the stated length, and
    // the call either fills `target` or leaves it empty; a non-zero status is
    // handled before any other call sees it.
    let created = unsafe {
        AECreateDesc(
            TYPE_APPLICATION_BUNDLE_ID,
            MAIL_BUNDLE_ID.as_ptr().cast(),
            MAIL_BUNDLE_ID.len() as isize,
            &mut target,
        )
    };
    if created != 0 {
        return (
            PermissionState::Unknown,
            format!("AECreateDesc failed with {created}"),
        );
    }
    // SAFETY: `target` was initialized by AECreateDesc and is disposed exactly
    // once below; the query is read-only because ask_user is 0.
    let status = unsafe {
        AEDeterminePermissionToAutomateTarget(&target, TYPE_WILD_CARD, TYPE_WILD_CARD, 0)
    };
    // SAFETY: same descriptor, not used after this call.
    unsafe {
        AEDisposeDesc(&mut target);
    }
    match status {
        0 => (
            PermissionState::Granted,
            "automation permission granted".into(),
        ),
        -1743 => (
            PermissionState::Denied,
            "automation permission denied (-1743)".into(),
        ),
        -1744 => (
            PermissionState::NotDetermined,
            "automation consent not determined (-1744); the first action asks".into(),
        ),
        other => (
            PermissionState::Unknown,
            format!("AEDeterminePermissionToAutomateTarget returned {other}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    fn message() -> Payload {
        Payload::Message {
            to: "controlled@example.com".into(),
            subject: "报表 — \"quoted\"".into(),
            body: "line one\nline two $(rm -rf /) `whoami` — 中文\r\nend".into(),
        }
    }

    /// The exact user text lives in private files, never in the script or in
    /// argv, so AppleScript quoting cannot be escaped by message content.
    #[test]
    fn user_text_reaches_private_files_but_never_the_argv() {
        let payload = message();
        let Payload::Message { to, subject, body } = payload.clone() else {
            unreachable!("message payload");
        };
        let plan = plan(ActionId::MailSendSubmit, payload).expect("a plan");

        assert_eq!(plan.program(), OSASCRIPT);
        assert_eq!(plan.action(), ActionId::MailSendSubmit);
        assert_eq!(plan.provider(), ProviderId::MacosMail);
        assert_eq!(plan.timeout(), Duration::from_secs(120));
        assert_eq!(plan.args().len(), 3);
        assert_eq!(plan.args()[0], "-e");
        assert_eq!(plan.args()[1], SEND_SCRIPT);
        for text in [&to, &subject, &body] {
            assert!(
                !plan.args().iter().any(|arg| arg.contains(text.as_str())),
                "user text leaked into argv: {text:?}"
            );
        }

        let read = |name: &str| std::fs::read_to_string(plan.cwd().join(name)).expect(name);
        assert_eq!(read("to.txt"), to);
        assert_eq!(read("subject.txt"), subject);
        assert_eq!(read("body.txt"), body);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(plan.cwd().join("body.txt"))
                .expect("body file")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn each_action_maps_to_its_own_static_script_and_timeout() {
        for (action, script, timeout) in [
            (
                ActionId::MailDraftCreate,
                DRAFT_SCRIPT,
                Duration::from_secs(90),
            ),
            (
                ActionId::MailSendOpenUi,
                COMPOSE_SCRIPT,
                Duration::from_secs(90),
            ),
            (
                ActionId::MailSendSubmit,
                SEND_SCRIPT,
                Duration::from_secs(120),
            ),
            (
                ActionId::MailSentCheck,
                SENT_CHECK_SCRIPT,
                Duration::from_secs(240),
            ),
        ] {
            let payload = match action {
                ActionId::MailSentCheck => Payload::Check {
                    to: "controlled@example.com".into(),
                    subject: "s".into(),
                },
                _ => message(),
            };
            let plan = plan(action, payload).expect("a plan");
            assert_eq!(plan.action(), action);
            assert_eq!(plan.args()[1], script, "{}", action.as_str());
            assert_eq!(plan.timeout(), timeout, "{}", action.as_str());
        }
    }

    #[test]
    fn a_check_plan_carries_no_body_file() {
        let plan = plan(
            ActionId::MailSentCheck,
            Payload::Check {
                to: "controlled@example.com".into(),
                subject: "s".into(),
            },
        )
        .expect("a plan");
        assert!(plan.cwd().join("to.txt").is_file());
        assert!(plan.cwd().join("subject.txt").is_file());
        assert!(!plan.cwd().join("body.txt").exists());
    }

    /// Reproduces the live hang: macOS can leave the permission FFI waiting on
    /// tccd while a consent prompt is unresolved. Discovery must return an
    /// explicit unknown within the bound, and a second probe must not queue a
    /// second thread behind the same prompt.
    #[tokio::test]
    async fn a_stalled_permission_probe_returns_unknown_without_blocking() {
        fn slow() -> (PermissionState, String) {
            std::thread::sleep(Duration::from_millis(300));
            (PermissionState::Granted, "late".into())
        }

        let probes = PermissionProbes::default();
        let started = std::time::Instant::now();
        let (state, detail) =
            bounded_permission_probe(&probes, slow, Duration::from_millis(50)).await;
        assert_eq!(state, PermissionState::Unknown);
        assert!(detail.contains("did not return"), "{detail}");
        assert!(started.elapsed() < Duration::from_secs(1));

        let (second, reason) =
            bounded_permission_probe(&probes, slow, Duration::from_millis(50)).await;
        assert_eq!(second, PermissionState::Unknown);
        assert!(reason.contains("still waiting"), "{reason}");

        tokio::time::sleep(Duration::from_millis(400)).await;
        let (state, detail) = bounded_permission_probe(
            &probes,
            || (PermissionState::Granted, "ok".into()),
            Duration::from_secs(1),
        )
        .await;
        assert_eq!(state, PermissionState::Granted, "{detail}");
    }

    static RELEASE_STUCK_PROBE: AtomicBool = AtomicBool::new(false);

    fn stuck() -> (PermissionState, String) {
        while !RELEASE_STUCK_PROBE.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(5));
        }
        (
            PermissionState::Granted,
            "the original probe finally returned".into(),
        )
    }

    fn granted() -> (PermissionState, String) {
        (PermissionState::Granted, "fresh probe after consent".into())
    }

    /// A `tccd` call can stay blocked after the caller's timeout, and macOS
    /// offers no way to cancel it. It must not own the only probe slot
    /// forever: a later discovery after the user grants consent must be able
    /// to start a fresh probe and observe the new state. The abandoned calls
    /// stay bounded rather than growing by one thread per discovery.
    #[tokio::test]
    async fn a_stuck_probe_is_bounded_and_a_later_grant_is_observable() {
        let probes = PermissionProbes::default();
        RELEASE_STUCK_PROBE.store(false, Ordering::Release);

        let (state, _) =
            bounded_permission_probe(&probes, stuck, Duration::from_millis(50)).await;
        assert_eq!(state, PermissionState::Unknown);

        tokio::time::sleep(Duration::from_millis(60)).await;
        let (state, detail) =
            bounded_permission_probe(&probes, granted, Duration::from_secs(1)).await;
        assert_eq!(state, PermissionState::Granted, "{detail}");

        // The original stuck thread still owns the replacement allowance.
        // Another stuck probe may start, but the next admission is refused
        // instead of leaking one more thread per discovery.
        let (state, _) =
            bounded_permission_probe(&probes, stuck, Duration::from_millis(50)).await;
        assert_eq!(state, PermissionState::Unknown);
        tokio::time::sleep(Duration::from_millis(60)).await;
        let (state, reason) =
            bounded_permission_probe(&probes, stuck, Duration::from_millis(50)).await;
        assert_eq!(state, PermissionState::Unknown);
        assert!(reason.contains("refusing to start another"), "{reason}");

        // When tccd finally answers the original call, the abandoned count
        // clears and discovery can observe the grant again.
        RELEASE_STUCK_PROBE.store(true, Ordering::Release);
        tokio::time::sleep(Duration::from_millis(60)).await;
        let (state, detail) =
            bounded_permission_probe(&probes, granted, Duration::from_secs(1)).await;
        assert_eq!(state, PermissionState::Granted, "{detail}");
    }
}
