//! macOS LaunchAgent backend.
//!
//! A per-user LaunchAgent in `~/Library/LaunchAgents` is the smallest
//! registration that survives the terminal and the desktop app without
//! administrator rights. It is loaded into the user's GUI domain, so it runs
//! only while that user is logged in; a logged-out Mac has no user session to
//! execute in, and the occurrence is handled by the misfire policy when the
//! user returns.
//!
//! `StartInterval` is used instead of a calendar trigger because launchd's
//! calendar entries have minute granularity and cannot express the schedule's
//! second-level slots. The job is a cheap idempotent dispatcher pass: it
//! claims only due, unclaimed occurrences.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{
    WakeupError, WakeupRegistration, WakeupSpec, WakeupStatus, label_for, launch_agents_dir,
    xml_escape,
};

const LAUNCHCTL: &str = "/bin/launchctl";
const PLUTIL: &str = "/usr/bin/plutil";
const ID: &str = "/usr/bin/id";

pub(super) async fn enable(spec: &WakeupSpec) -> Result<WakeupStatus, WakeupError> {
    let label = label_for(&spec.home);
    let agents = launch_agents_dir()?;
    tokio::fs::create_dir_all(&agents).await?;
    tokio::fs::create_dir_all(&spec.log_dir).await?;
    let path = agents.join(format!("{label}.plist"));
    let body = plist_contents(spec, &label);
    write_atomic(&path, body.as_bytes()).await?;

    let uid = current_uid().await?;
    let domain = format!("gui/{uid}");
    let service = format!("{domain}/{label}");
    // Removing an older load first makes enable idempotent. A bootout failure
    // is normal when the job is not loaded; bootstrap is the authority.
    let _ = launchctl(&["bootout", &service]).await;
    let _ = launchctl(&["enable", &service]).await;
    let bootstrap = launchctl(&["bootstrap", &domain, &path.to_string_lossy()]).await?;
    if !bootstrap.success {
        // Do not leave a half registration that would be auto-loaded at the
        // next login after the user was told enabling failed.
        let _ = tokio::fs::remove_file(&path).await;
        return Err(WakeupError::command(
            format!("{LAUNCHCTL} bootstrap"),
            bootstrap.status,
            bootstrap.stderr,
        ));
    }

    let status = status(&spec.home).await?;
    if !status.loaded {
        return Err(WakeupError::Invalid(format!(
            "launchd accepted {label} but does not report it as loaded{}",
            status
                .detail
                .as_deref()
                .map(|detail| format!(": {detail}"))
                .unwrap_or_default()
        )));
    }
    Ok(status)
}

pub(super) async fn disable(home: &Path) -> Result<WakeupStatus, WakeupError> {
    let label = label_for(home);
    let path = launch_agents_dir()?.join(format!("{label}.plist"));
    if let Ok(uid) = current_uid().await {
        let service = format!("gui/{uid}/{label}");
        // `bootout` is best-effort: the job may already be unloaded. Removing
        // the file is what stops launchd from loading it at the next login.
        let _ = launchctl(&["bootout", &service]).await;
    }
    match tokio::fs::remove_file(&path).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let observed = status(home).await?;
    if observed.loaded {
        return Err(WakeupError::Invalid(format!(
            "launchd still reports {label} as loaded after bootout; it was not uninstalled"
        )));
    }
    Ok(observed)
}

pub(super) async fn status(home: &Path) -> Result<WakeupStatus, WakeupError> {
    let label = label_for(home);
    let path = launch_agents_dir()?.join(format!("{label}.plist"));
    let registered = path.is_file();
    let mut registration = None;
    let mut detail = None;
    if registered {
        match read_plist(&path).await {
            Ok(value) => registration = Some(registration_from_json(&value)),
            Err(error) => detail = Some(format!("could not read {}: {error}", path.display())),
        }
    }

    let mut loaded = false;
    let mut last_exit_code = None;
    let mut run_count = None;
    match current_uid().await {
        Ok(uid) => {
            let service = format!("gui/{uid}/{label}");
            let output = launchctl(&["print", &service]).await?;
            loaded = output.success;
            if output.success {
                last_exit_code = parse_last_exit(&output.stdout);
                run_count = parse_run_count(&output.stdout);
            } else if registered && !output.stderr.is_empty() {
                detail = Some(output.stderr.trim().to_owned());
            }
        }
        Err(error) => {
            detail = Some(format!("could not determine the current user: {error}"));
        }
    }

    Ok(WakeupStatus {
        backend: "launchd",
        supported: true,
        label,
        registration_path: Some(path),
        registered,
        loaded,
        registration,
        last_exit_code,
        run_count,
        detail,
    })
}

/// The exact plist this build writes. Pure so tests can assert escaping and
/// argument identity without touching launchd.
pub(crate) fn plist_contents(spec: &WakeupSpec, label: &str) -> String {
    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str(
        "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n",
    );
    out.push_str("<plist version=\"1.0\">\n<dict>\n");
    out.push_str(&format!(
        "  <key>Label</key>\n  <string>{}</string>\n",
        xml_escape(label)
    ));
    out.push_str("  <key>ProgramArguments</key>\n  <array>\n");
    out.push_str(&format!(
        "    <string>{}</string>\n",
        xml_escape(&spec.executable.to_string_lossy())
    ));
    for argument in &spec.arguments {
        out.push_str(&format!("    <string>{}</string>\n", xml_escape(argument)));
    }
    out.push_str("  </array>\n");
    if !spec.environment.is_empty() {
        out.push_str("  <key>EnvironmentVariables</key>\n  <dict>\n");
        for (key, value) in &spec.environment {
            out.push_str(&format!(
                "    <key>{}</key>\n    <string>{}</string>\n",
                xml_escape(key),
                xml_escape(value)
            ));
        }
        out.push_str("  </dict>\n");
    }
    out.push_str(&format!(
        "  <key>WorkingDirectory</key>\n  <string>{}</string>\n",
        xml_escape(&spec.home.to_string_lossy())
    ));
    out.push_str("  <key>RunAtLoad</key>\n  <true/>\n");
    out.push_str(&format!(
        "  <key>StartInterval</key>\n  <integer>{}</integer>\n",
        spec.interval.as_secs()
    ));
    out.push_str("  <key>ThrottleInterval</key>\n  <integer>10</integer>\n");
    out.push_str("  <key>ProcessType</key>\n  <string>Background</string>\n");
    out.push_str("  <key>AbandonProcessGroup</key>\n  <true/>\n");
    let log = spec.log_dir.join("dispatch.log");
    out.push_str(&format!(
        "  <key>StandardOutPath</key>\n  <string>{}</string>\n",
        xml_escape(&log.to_string_lossy())
    ));
    out.push_str(&format!(
        "  <key>StandardErrorPath</key>\n  <string>{}</string>\n",
        xml_escape(&log.to_string_lossy())
    ));
    out.push_str("</dict>\n</plist>\n");
    out
}

fn registration_from_json(value: &serde_json::Value) -> WakeupRegistration {
    let arguments: Vec<String> = value
        .get("ProgramArguments")
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let (executable, arguments) = arguments
        .split_first()
        .map(|(first, rest)| (PathBuf::from(first), rest.to_vec()))
        .unwrap_or_else(|| (PathBuf::new(), Vec::new()));
    let interval = value
        .get("StartInterval")
        .and_then(|value| value.as_u64())
        .map(Duration::from_secs);
    let environment = value
        .get("EnvironmentVariables")
        .and_then(|value| value.as_object())
        .map(|object| {
            object
                .iter()
                .filter_map(|(key, value)| {
                    value.as_str().map(|value| (key.clone(), value.to_owned()))
                })
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    let working_directory = value
        .get("WorkingDirectory")
        .and_then(|value| value.as_str())
        .map(PathBuf::from);
    WakeupRegistration {
        executable,
        arguments,
        interval,
        environment,
        working_directory,
    }
}

fn parse_last_exit(print_output: &str) -> Option<i32> {
    print_output.lines().find_map(|line| {
        let line = line.trim();
        line.strip_prefix("last exit code = ")
            .and_then(|value| value.trim().parse().ok())
    })
}

fn parse_run_count(print_output: &str) -> Option<u64> {
    print_output.lines().find_map(|line| {
        let line = line.trim();
        line.strip_prefix("runs = ")
            .and_then(|value| value.trim().parse().ok())
    })
}

async fn read_plist(path: &Path) -> Result<serde_json::Value, WakeupError> {
    let output = command(PLUTIL, &["-convert", "json", "-o", "-", &path.to_string_lossy()]).await?;
    if !output.success {
        return Err(WakeupError::command(
            PLUTIL,
            output.status,
            output.stderr,
        ));
    }
    serde_json::from_str(&output.stdout)
        .map_err(|error| WakeupError::Invalid(format!("invalid launchd plist JSON: {error}")))
}

async fn current_uid() -> Result<u32, WakeupError> {
    let output = command(ID, &["-u"]).await?;
    if !output.success {
        return Err(WakeupError::command(ID, output.status, output.stderr));
    }
    output
        .stdout
        .trim()
        .parse()
        .map_err(|error| WakeupError::Invalid(format!("invalid uid from `{ID} -u`: {error}")))
}

async fn launchctl(args: &[&str]) -> Result<Output, WakeupError> {
    command(LAUNCHCTL, args).await
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
        .await
        .map_err(|error| {
            WakeupError::Io(std::io::Error::new(
                error.kind(),
                format!("could not run {program}: {error}"),
            ))
        })?;
    Ok(Output {
        success: output.status.success(),
        status: output.status.to_string(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

async fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), WakeupError> {
    let temporary = path.with_file_name(format!(
        ".{}.{}.{}.tmp",
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "leveler".into()),
        std::process::id(),
        leveler_core::new_uuid_string()
    ));
    tokio::fs::write(&temporary, bytes).await?;
    match tokio::fs::rename(&temporary, path).await {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = tokio::fs::remove_file(&temporary).await;
            Err(error.into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> WakeupSpec {
        WakeupSpec {
            home: PathBuf::from("/tmp/a home/中文"),
            executable: PathBuf::from("/opt/leveler/bin/leveler"),
            arguments: vec![
                "schedule".into(),
                "dispatch".into(),
                "--source".into(),
                "os".into(),
            ],
            environment: BTreeMap::from([
                ("PATH".into(), "/usr/bin:/bin".into()),
                ("LEVELER_HOME".into(), "/tmp/a home/中文".into()),
            ]),
            interval: Duration::from_secs(60),
            log_dir: PathBuf::from("/tmp/a home/中文/logs/schedule"),
        }
    }

    #[test]
    fn the_plist_preserves_arguments_and_escapes_paths() {
        let body = plist_contents(&spec(), "com.leveler.schedule.test");
        assert!(body.contains("<string>/opt/leveler/bin/leveler</string>"));
        assert!(body.contains("<string>--source</string>"));
        assert!(body.contains("<string>os</string>"));
        assert!(body.contains("<string>/tmp/a home/中文</string>"));
        assert!(body.contains("<key>StartInterval</key>\n  <integer>60</integer>"));
        assert!(body.contains("<key>RunAtLoad</key>\n  <true/>"));
        assert!(body.contains("<string>/usr/bin:/bin</string>"));
        assert!(!body.contains("&amp;lt;"), "double escaping");
    }

    #[test]
    fn a_plist_round_trips_the_registration_fields() {
        let value: serde_json::Value = serde_json::json!({
            "ProgramArguments": ["/opt/bin/leveler", "schedule", "dispatch"],
            "StartInterval": 60,
            "EnvironmentVariables": {"PATH": "/usr/bin:/bin"},
            "WorkingDirectory": "/home/u/.leveler"
        });
        let registration = registration_from_json(&value);
        assert_eq!(registration.executable, PathBuf::from("/opt/bin/leveler"));
        assert_eq!(registration.arguments, vec!["schedule", "dispatch"]);
        assert_eq!(registration.interval, Some(Duration::from_secs(60)));
        assert_eq!(registration.environment.get("PATH").unwrap(), "/usr/bin:/bin");
        assert_eq!(
            registration.working_directory,
            Some(PathBuf::from("/home/u/.leveler"))
        );
    }

    #[test]
    fn launchctl_print_facts_are_parsed() {
        let output = "\tgui/501/com.leveler.schedule.abc = {\n\
                      \t\tactive count = 0\n\
                      \t\truns = 7\n\
                      \t\tlast exit code = 0\n\
                      \t}\n";
        assert_eq!(parse_last_exit(output), Some(0));
        assert_eq!(parse_run_count(output), Some(7));
        assert_eq!(parse_last_exit("nothing here"), None);
    }
}
