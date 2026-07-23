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
    #[cfg(windows)]
    {
        if has_windows_background_start(cmd) {
            return Some(crate::recoverable::permission_refused(
                "refused Windows process detaching in a foreground shell tool \
                 (`start …`, `Start-Process`). Detached processes cannot be \
                 reaped by this tool and orphan on timeout.",
                "use `run_command` with `background=true` for servers/watchers, \
                 then a separate tool call for health checks.",
            ));
        }
    }
    if let Some(token) = sensitive_shell_token(cmd) {
        return Some(crate::recoverable::permission_refused(
            &format!(
                "refused a command touching a credential-bearing file (`{token}`). \
                 .env*, key/cert files, .ssh/.aws paths, credentials.json and \
                 similar are blocked from tool access at every layer."
            ),
            "if the task truly needs that value, ask the user to supply it or \
             wire it via configuration instead of reading the secret file.",
        ));
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

/// Best-effort refusal of commands that touch credential-bearing files
/// (`cat .env`, `openssl rsa -in server.pem`, `~/.ssh/id_rsa`, …), so the
/// command layer enforces the same product semantics as the workspace layer
/// (`read_file(".env")` is already Denied). Token-based and quote-aware — a
/// quoted prose string mentioning `.env` is not a path. This is a guard rail
/// for the honest-model path, not a security boundary: the OS-level secret
/// scrubbing and env_clear remain the hard line.
fn sensitive_shell_token(cmd: &str) -> Option<String> {
    let mut chars = cmd.chars();
    let mut in_single = false;
    let mut in_double = false;
    let mut token = String::new();
    while let Some(c) = chars.next() {
        if in_single {
            if c == '\'' {
                in_single = false;
            }
            continue;
        }
        if in_double {
            if c == '\\' {
                chars.next();
                continue;
            }
            if c == '"' {
                in_double = false;
            }
            continue;
        }
        match c {
            '\'' => in_single = true,
            '"' => in_double = true,
            // Comment tail never executes; the `#`-swallow guard owns that case.
            '#' => break,
            c if c.is_whitespace() => {
                if let Some(hit) = take_sensitive(&mut token) {
                    return Some(hit);
                }
            }
            _ => token.push(c),
        }
    }
    take_sensitive(&mut token)
}

/// Drain `token`; return it if it points at a sensitive path.
fn take_sensitive(token: &mut String) -> Option<String> {
    let t = std::mem::take(token);
    let trimmed = t.trim_matches(|c: char| matches!(c, '(' | ')' | '<' | '>' | '|' | ';' | '&'));
    if !trimmed.is_empty() && arg_touches_sensitive_path(trimmed) {
        return Some(trimmed.to_string());
    }
    None
}

/// Whether one shell token / argv argument points at a credential-bearing
/// path. Shares the workspace layer's file-name rule set; `.git` is
/// deliberately not matched here (git tooling passes `.git` paths
/// legitimately — its protection stays at the workspace layer).
fn arg_touches_sensitive_path(arg: &str) -> bool {
    // `--file=.env` style flags: also check the value after the last `=`.
    let value = arg.rsplit('=').next().unwrap_or(arg);
    for cand in [arg, value] {
        let path = std::path::Path::new(cand);
        for comp in path.components() {
            if let std::path::Component::Normal(os) = comp
                && matches!(os.to_string_lossy().as_ref(), ".ssh" | ".aws")
            {
                return true;
            }
        }
        if let Some(file) = path.file_name().map(|f| f.to_string_lossy())
            && leveler_execution::is_sensitive_file_name(&file)
        {
            return true;
        }
    }
    false
}

/// Per-argument variant for structured `run_command` argv: each arg is one
/// path candidate (never token-split, so a prose `-m` message stays allowed —
/// a multi-word string has no separator, hence no sensitive file name).
pub(crate) fn refuse_sensitive_args(args: &[String]) -> Option<String> {
    let hit = args.iter().map(|a| a.trim()).find(|a| {
        !a.is_empty() && !a.contains(char::is_whitespace) && arg_touches_sensitive_path(a)
    })?;
    Some(crate::recoverable::permission_refused(
        &format!(
            "refused a command argument pointing at a credential-bearing file \
             (`{hit}`). .env*, key/cert files, .ssh/.aws paths, \
             credentials.json and similar are blocked from tool access at \
             every layer."
        ),
        "if the task truly needs that value, ask the user to supply it or \
         wire it via configuration instead of reading the secret file.",
    ))
}

/// Windows background/hang anti-patterns: cmd's `start` (segment-initial) and
/// PowerShell `Start-Process` detach processes the foreground tool cannot
/// reap — the Windows counterpart of Unix `&`/`nohup`. Not matched: `npm
/// start` and other uses where `start` is an argument, not the command.
#[cfg_attr(not(windows), allow(dead_code))]
fn has_windows_background_start(cmd: &str) -> bool {
    let lower = cmd.to_ascii_lowercase();
    if shell_token_present(&lower, "start-process") {
        return true;
    }
    lower
        .split(['&', '|', ';', '\n'])
        .filter_map(|seg| seg.split_whitespace().next())
        .any(|first| matches!(first, "start" | "start.exe"))
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
        assert!(err.contains("[recoverable]"), "{err}");
        // Unix catches the `&` job-control backgrounding first and points at
        // `run_command background=true`. That detection is Unix-only; on Windows
        // the same string is refused by the `#`-comment guard instead.
        #[cfg(not(windows))]
        {
            assert!(
                err.contains("background") || err.contains("job-control") || err.contains('&'),
                "{err}"
            );
            assert!(
                err.contains("run_command") && err.contains("background=true"),
                "must tell the model the correct next step: {err}"
            );
        }
        #[cfg(windows)]
        {
            assert!(err.contains("comment"), "{err}");
            assert!(err.contains("split into separate tool calls"), "{err}");
        }
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
        // `.expect` already proves the bypass is guarded; the guidance string is
        // Unix-specific (`background=true`) vs. the Windows `#`-comment refusal.
        #[cfg(not(windows))]
        assert!(err.contains("background=true"), "{err}");
        #[cfg(windows)]
        assert!(err.contains("comment"), "{err}");
    }

    #[test]
    fn refuses_sensitive_files_in_shell_strings() {
        for cmd in [
            "cat .env",
            "cat config/.env.production",
            "openssl rsa -in server.pem",
            "cat ~/.ssh/id_rsa",
            "cat credentials.json",
            "head .npmrc",
            "grep token < .netrc",
        ] {
            assert!(refuse_shell_script(cmd).is_some(), "{cmd} must be refused");
        }
        // No false positives on ordinary work, quoted prose, or comments.
        for cmd in [
            "cargo test -q",
            "git commit -m \"document .env handling\"",
            "echo done # .env note",
            "ls src && cargo build --release",
        ] {
            assert!(refuse_shell_script(cmd).is_none(), "{cmd} must be allowed");
        }
    }

    #[test]
    fn refuses_sensitive_argv_paths_but_not_prose_args() {
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(refuse_sensitive_args(&s(&[".env"])).is_some());
        assert!(refuse_sensitive_args(&s(&["rsa", "-in", "server.key"])).is_some());
        assert!(refuse_sensitive_args(&s(&["--file=.env"])).is_some());
        assert!(refuse_sensitive_args(&s(&["/home/u/.aws/config", "x"])).is_some());
        // A prose arg mentioning .env is a message, not a path to it.
        assert!(refuse_sensitive_args(&s(&["commit", "-m", "document .env handling"])).is_none());
        assert!(refuse_sensitive_args(&s(&["build", "--release"])).is_none());
    }

    #[test]
    fn detects_windows_background_start_but_not_npm_start() {
        assert!(has_windows_background_start("start notepad.exe"));
        assert!(has_windows_background_start("cd x && start /B server.exe"));
        assert!(has_windows_background_start(
            "Start-Process python -ArgumentList app.py"
        ));
        assert!(!has_windows_background_start("npm start"));
        assert!(!has_windows_background_start("npm run start && echo ok"));
        assert!(!has_windows_background_start("cargo build --release"));
    }

    #[test]
    fn run_command_non_shell_is_not_scanned_as_script() {
        assert!(
            refuse_run_command_shell_bypass("python3", &["app.py".into()]).is_none(),
            "argv form is not a shell script body"
        );
    }
}
