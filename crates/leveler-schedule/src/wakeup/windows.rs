//! Windows Task Scheduler backend.
//!
//! The task is a per-user task with `InteractiveToken` and
//! `LeastPrivilege`: no administrator rights, no stored password, and it runs
//! only while that user has an interactive session. That is the honest limit
//! of unattended GUI work on Windows — Session 0 has no desktop and the task
//! scheduler cannot fabricate one.
//!
//! `schtasks.exe` is the registration authority. The task repeats every
//! minute, ignores a second instance while one is running, and is removed on
//! disable so it is not re-registered at the next logon.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{
    WakeupError, WakeupRegistration, WakeupSpec, WakeupStatus, label_for, task_scheduler_xml,
};

const SCHTASKS: &str = "schtasks.exe";
const WHOAMI: &str = "whoami.exe";

pub(super) async fn enable(spec: &WakeupSpec) -> Result<WakeupStatus, WakeupError> {
    let label = label_for(&spec.home);
    let user = current_user().await?;
    let xml = task_scheduler_xml(spec, &label, &user);
    let task_file = std::env::temp_dir().join(format!(
        "leveler-schedule-{}-{}.xml",
        std::process::id(),
        leveler_core::new_uuid_string()
    ));
    // Task Scheduler's /XML expects UTF-16 with a BOM; `schtasks` rejects a
    // plain UTF-8 file on some versions.
    let utf16: Vec<u8> = xml
        .encode_utf16()
        .flat_map(|unit| unit.to_le_bytes())
        .collect();
    let mut body = Vec::with_capacity(utf16.len() + 2);
    body.extend_from_slice(&[0xFF, 0xFE]);
    body.extend_from_slice(&utf16);
    tokio::fs::write(&task_file, &body).await?;

    let created = command(
        SCHTASKS,
        &[
            "/Create",
            "/F",
            "/TN",
            &label,
            "/XML",
            &task_file.to_string_lossy(),
        ],
    )
    .await;
    let _ = tokio::fs::remove_file(&task_file).await;
    let created = created?;
    if !created.success {
        // No half registration: delete a task the scheduler may have created
        // despite the error.
        let _ = command(SCHTASKS, &["/Delete", "/F", "/TN", &label]).await;
        return Err(WakeupError::command(
            format!("{SCHTASKS} /Create"),
            created.status,
            created.stderr,
        ));
    }

    let status = status(&spec.home).await?;
    if !status.registered {
        return Err(WakeupError::Invalid(format!(
            "Task Scheduler accepted {label} but does not report it"
        )));
    }
    Ok(status)
}

pub(super) async fn disable(home: &Path) -> Result<WakeupStatus, WakeupError> {
    let label = label_for(home);
    let deleted = command(SCHTASKS, &["/Delete", "/F", "/TN", &label]).await?;
    if !deleted.success {
        // Deleting a task that does not exist is the desired end state.
        let observed = status(home).await?;
        if !observed.registered {
            return Ok(observed);
        }
        return Err(WakeupError::command(
            format!("{SCHTASKS} /Delete"),
            deleted.status,
            deleted.stderr,
        ));
    }
    let observed = status(home).await?;
    if observed.registered {
        return Err(WakeupError::Invalid(format!(
            "Task Scheduler still reports {label} after deletion"
        )));
    }
    Ok(observed)
}

pub(super) async fn status(home: &Path) -> Result<WakeupStatus, WakeupError> {
    let label = label_for(home);
    let query = command(SCHTASKS, &["/Query", "/TN", &label, "/XML"]).await?;
    let registered = query.success;
    let mut registration = None;
    if registered {
        registration = Some(registration_from_xml(&query.stdout));
    }
    // Task Scheduler registers and enables in one operation; a task that
    // exists is a task that can launch. The verbose listing adds last-result
    // facts when the OS locale is English.
    let verbose = command(SCHTASKS, &["/Query", "/TN", &label, "/FO", "LIST", "/V"]).await;
    let (last_exit_code, run_count, detail) = match verbose {
        Ok(output) if output.success => (
            parse_list_value(&output.stdout, "Last Result").and_then(|value| value.parse().ok()),
            None,
            None,
        ),
        Ok(output) if registered => (
            None,
            None,
            Some(format!(
                "task exists but its verbose state could not be read: {}",
                output.stderr.trim()
            )),
        ),
        Ok(_) => (None, None, None),
        Err(error) => (
            None,
            None,
            Some(format!("could not query Task Scheduler: {error}")),
        ),
    };
    Ok(WakeupStatus {
        backend: "task-scheduler",
        supported: true,
        label,
        registration_path: None,
        registered,
        loaded: registered,
        registration,
        last_exit_code,
        run_count,
        detail,
    })
}

fn registration_from_xml(xml: &str) -> WakeupRegistration {
    let command = element_text(xml, "Command")
        .map(PathBuf::from)
        .unwrap_or_default();
    let arguments = element_text(xml, "Arguments")
        .map(|raw| split_windows_arguments(&raw))
        .unwrap_or_default();
    let working_directory = element_text(xml, "WorkingDirectory").map(PathBuf::from);
    WakeupRegistration {
        executable: command,
        arguments,
        interval: element_text(xml, "Interval")
            .as_deref()
            .and_then(parse_iso8601_duration),
        environment: BTreeMap::new(),
        working_directory,
    }
}

fn element_text(xml: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml_unescape(xml[start..end].trim()))
}

fn xml_unescape(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn parse_list_value(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        (name.trim() == key).then(|| value.trim().to_owned())
    })
}

fn parse_iso8601_duration(raw: &str) -> Option<Duration> {
    let raw = raw.trim().strip_prefix("PT")?;
    let (value, unit) = raw.split_at(raw.find(|ch: char| !ch.is_ascii_digit())?);
    let value: u64 = value.parse().ok()?;
    match unit {
        "S" => Some(Duration::from_secs(value)),
        "M" => Some(Duration::from_secs(value * 60)),
        "H" => Some(Duration::from_secs(value * 3600)),
        _ => None,
    }
}

/// Split a `CommandLineToArgvW` argument string just well enough to display
/// and compare what Task Scheduler stored. The scheduler itself is the parser
/// that matters; this is a diagnostic projection.
fn split_windows_arguments(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut chars = raw.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => quoted = !quoted,
            '\\' if chars.peek() == Some(&'"') => {
                current.push('\\');
                chars.next();
                current.push('"');
            }
            ch if ch.is_whitespace() && !quoted => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            }
            ch => current.push(ch),
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

async fn current_user() -> Result<String, WakeupError> {
    let output = command(WHOAMI, &[]).await?;
    if !output.success {
        return Err(WakeupError::command(WHOAMI, output.status, output.stderr));
    }
    let user = output.stdout.trim();
    if user.is_empty() {
        return Err(WakeupError::Invalid(
            "`whoami.exe` returned an empty user name".into(),
        ));
    }
    Ok(user.to_owned())
}

struct Output {
    success: bool,
    status: String,
    stdout: String,
    stderr: String,
}

async fn command(program: &str, args: &[&str]) -> Result<Output, WakeupError> {
    let output = tokio::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .await?;
    Ok(Output {
        success: output.status.success(),
        status: output.status.to_string(),
        stdout: super::windows_decode_output(&output.stdout),
        stderr: super::windows_decode_output(&output.stderr),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_task_xml_round_trips_into_a_registration() {
        let xml = r#"<Task><Actions><Exec>
            <Command>C:\Program Files\Leveler\leveler.exe</Command>
            <Arguments>schedule dispatch --source os --config-dir "C:\Users\A B\configs"</Arguments>
            <WorkingDirectory>C:\Users\A B\.leveler</WorkingDirectory>
            </Exec></Actions><Triggers><TimeTrigger><Repetition><Interval>PT1M</Interval></Repetition></TimeTrigger></Triggers></Task>"#;
        let registration = registration_from_xml(xml);
        assert_eq!(
            registration.executable,
            PathBuf::from(r"C:\Program Files\Leveler\leveler.exe")
        );
        assert_eq!(
            registration.arguments,
            vec![
                "schedule",
                "dispatch",
                "--source",
                "os",
                "--config-dir",
                r"C:\Users\A B\configs"
            ]
        );
        assert_eq!(registration.interval, Some(Duration::from_secs(60)));
        assert_eq!(
            registration.working_directory,
            Some(PathBuf::from(r"C:\Users\A B\.leveler"))
        );
    }

    #[test]
    fn iso8601_durations_parse() {
        assert_eq!(
            parse_iso8601_duration("PT60S"),
            Some(Duration::from_secs(60))
        );
        assert_eq!(
            parse_iso8601_duration("PT1M"),
            Some(Duration::from_secs(60))
        );
        assert_eq!(
            parse_iso8601_duration("PT2H"),
            Some(Duration::from_secs(7200))
        );
        assert_eq!(parse_iso8601_duration("P1D"), None);
    }
}
