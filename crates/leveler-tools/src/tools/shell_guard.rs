//! Preflight guards for hang-prone shell strings the model invents.
//!
//! Shared by [`super::shell_command`] and foreground [`super::run_command`] when
//! the program is a shell wrapper (`sh -c '…'`). Fail closed with a recoverable
//! error **before spawn** so a bad command cannot trap a turn for minutes.

/// Refuse hang-prone / self-defeating shell strings before spawn.
///
/// Models often invent `python app.py & sleep 2 # curl …` which either hangs
/// the turn (foreground wait on a process group) or silently comments out the
/// health check. Surface a recoverable error so the agent retries correctly.
pub(crate) fn refuse_shell_script(cmd: &str) -> Option<String> {
    let cmd = cmd.trim();
    if cmd.is_empty() {
        return None;
    }
    #[cfg(not(windows))]
    {
        if has_unix_job_control_background(cmd) {
            return Some(crate::recoverable::permission_refused(
                "refused job-control backgrounding in a foreground shell tool \
                 (`cmd &`, `nohup`, `disown`). That pattern traps or orphans \
                 long-lived processes and is how turns hang on \
                 `python app.py & sleep …`.",
                "use `run_command` with `background=true` for servers/watchers, \
                 then a separate tool call for curl/health checks (no `&`, no \
                 `#` after the real pipeline).",
            ));
        }
    }
    if let Some(detail) = comment_swallows_trailing_command(cmd) {
        return Some(crate::recoverable::permission_refused(
            &format!(
                "refused a `#` comment that would swallow trailing command-like \
                 text ({detail}). Everything after `#` never runs."
            ),
            "put the full pipeline as real shell (no mid-line `#` before \
             curl/wget/…), or split into separate tool calls.",
        ));
    }
    None
}

/// If `program` is a shell wrapper and `args` carry a `-c`/`/C` script body,
/// apply [`refuse_shell_script`] so `run_command(sh, ["-c", bad])` cannot bypass
/// the `shell_command` guard.
pub(crate) fn refuse_run_command_shell_bypass(program: &str, args: &[String]) -> Option<String> {
    use leveler_execution::{is_shell_wrapper_program, shell_c_script};
    if !is_shell_wrapper_program(program) {
        return None;
    }
    let script = shell_c_script(args)?;
    refuse_shell_script(script)
}

/// True when the script uses Unix job-control backgrounding that must not run
/// inside a foreground tool.
///
/// Not matched: `&&`, `||`, `&>`, `>&`, `2>&1`, `<&`, or `&` inside quotes /
/// after `#` comments.
#[cfg(not(windows))]
fn has_unix_job_control_background(cmd: &str) -> bool {
    let lower = cmd.to_ascii_lowercase();
    if shell_token_present(&lower, "nohup") || shell_token_present(&lower, "disown") {
        return true;
    }

    let bytes = cmd.as_bytes();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_single {
            if b == b'\'' {
                in_single = false;
            }
            i += 1;
            continue;
        }
        if in_double {
            if b == b'\\' && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            if b == b'"' {
                in_double = false;
            }
            i += 1;
            continue;
        }
        match b {
            b'\'' => {
                in_single = true;
                i += 1;
            }
            b'"' => {
                in_double = true;
                i += 1;
            }
            b'#' => break,
            b'&' => {
                let prev = bytes[..i].iter().rev().find(|c| !c.is_ascii_whitespace());
                let next = bytes[i + 1..]
                    .iter()
                    .find(|c| !c.is_ascii_whitespace())
                    .copied();
                if next == Some(b'&') {
                    i += 2;
                    continue;
                }
                if next == Some(b'>') || prev == Some(&b'>') || prev == Some(&b'<') {
                    i += 1;
                    continue;
                }
                return true;
            }
            _ => i += 1,
        }
    }
    false
}

#[cfg(not(windows))]
fn shell_token_present(haystack_lower: &str, token: &str) -> bool {
    let mut rest = haystack_lower;
    while let Some(idx) = rest.find(token) {
        let before_ok = idx == 0
            || (!rest.as_bytes()[idx - 1].is_ascii_alphanumeric()
                && rest.as_bytes()[idx - 1] != b'_');
        let after = idx + token.len();
        let after_ok = after >= rest.len()
            || (!rest.as_bytes()[after].is_ascii_alphanumeric() && rest.as_bytes()[after] != b'_');
        if before_ok && after_ok {
            return true;
        }
        rest = &rest[idx + 1..];
    }
    false
}

fn comment_swallows_trailing_command(cmd: &str) -> Option<String> {
    let bytes = cmd.as_bytes();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_single {
            if b == b'\'' {
                in_single = false;
            }
            i += 1;
            continue;
        }
        if in_double {
            if b == b'\\' && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            if b == b'"' {
                in_double = false;
            }
            i += 1;
            continue;
        }
        if b == b'\'' {
            in_single = true;
            i += 1;
            continue;
        }
        if b == b'"' {
            in_double = true;
            i += 1;
            continue;
        }
        if b == b'#' {
            let tail = cmd[i + 1..].trim();
            if tail.is_empty() {
                return None;
            }
            if comment_tail_looks_like_command(tail) {
                let preview: String = tail.chars().take(48).collect();
                return Some(format!("`#{preview}…`"));
            }
            return None;
        }
        i += 1;
    }
    None
}

fn comment_tail_looks_like_command(tail: &str) -> bool {
    const CMDS: &[&str] = &[
        "curl", "wget", "http", "https", "python", "python3", "node", "npm", "pnpm", "yarn",
        "cargo", "go", "ruby", "php", "bash", "sh ", "zsh", "make", "docker", "kubectl", "git ",
        "nc ", "ncat", "openssl", "ss ", "lsof", "ps ",
    ];
    let lower = tail.to_ascii_lowercase();
    CMDS.iter().any(|c| {
        lower
            .find(c)
            .is_some_and(|idx| idx == 0 || !lower.as_bytes()[idx - 1].is_ascii_alphanumeric())
    })
}

/// Canonical anti-pattern from the production hang (AI-generated).
#[cfg(test)]
pub(crate) const HANG_ANTI_PATTERN: &str =
    "python3 app.py & sleep 2 # 检查 HTML 是否正常返回 curl -s http://127.0.0.1:5000";

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn refuses_server_background_sleep_comment_curl_anti_pattern() {
        let err = refuse_shell_script(HANG_ANTI_PATTERN).expect("must refuse");
        assert!(
            err.contains("background") || err.contains("job-control") || err.contains('&'),
            "{err}"
        );
        assert!(
            err.contains("run_command") && err.contains("background=true"),
            "must tell the model the correct next step: {err}"
        );
        assert!(err.contains("[recoverable]"), "{err}");
    }

    #[test]
    fn refuse_is_instant_not_a_hang() {
        // Measurement: preflight must not spawn and must return in well under
        // a second (the original failure mode was multi-minute hangs).
        let start = Instant::now();
        for _ in 0..200 {
            assert!(refuse_shell_script(HANG_ANTI_PATTERN).is_some());
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(100),
            "200 refuses took {elapsed:?}; must stay pure CPU preflight"
        );
    }

    #[test]
    fn refuses_comment_that_swallows_curl_without_ampersand() {
        let cmd = "sleep 1 # then curl -s http://127.0.0.1:5000/health";
        let err = refuse_shell_script(cmd).expect("must refuse swallowed curl");
        assert!(
            err.contains('#') || err.contains("comment") || err.contains("swallow"),
            "{err}"
        );
        assert!(err.contains("[recoverable]"), "{err}");
    }

    #[test]
    fn allows_normal_pipelines_and_logical_and() {
        assert!(refuse_shell_script("echo hi && echo ok").is_none());
        assert!(refuse_shell_script("cargo test -q 2>&1").is_none());
        assert!(refuse_shell_script("curl -s http://127.0.0.1:5000/").is_none());
        assert!(refuse_shell_script("python3 -c 'print(1)'").is_none());
        assert!(refuse_shell_script("echo hi # just a note").is_none());
    }

    #[cfg(not(windows))]
    #[test]
    fn refuses_nohup_and_trailing_ampersand() {
        assert!(refuse_shell_script("nohup python3 app.py").is_some());
        assert!(refuse_shell_script("python3 app.py &").is_some());
        assert!(refuse_shell_script("make -j4 && echo done").is_none());
    }

    #[test]
    fn run_command_sh_c_cannot_bypass_guard() {
        let err = refuse_run_command_shell_bypass("sh", &["-c".into(), HANG_ANTI_PATTERN.into()])
            .expect("sh -c must be guarded");
        assert!(err.contains("background=true"), "{err}");
    }

    #[test]
    fn run_command_non_shell_is_not_scanned_as_script() {
        assert!(
            refuse_run_command_shell_bypass("python3", &["app.py".into()]).is_none(),
            "argv form is not a shell script body"
        );
    }
}
