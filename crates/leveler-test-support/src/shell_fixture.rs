//! Trivial processes a test needs, spelled for the host it runs on.
//!
//! A test that wants "a process that prints a line" or "a process that stays
//! alive" is not asking for `echo` or `sleep` — those are Unix spellings of the
//! requirement, and on Windows neither is a program at all (`echo` is a `cmd`
//! builtin; there is no `sleep`). Keeping the spelling here means a Windows
//! failure says something about the product rather than about the fixture.

/// A command that prints `text` on stdout and exits 0.
pub fn echo_command(text: &str) -> (String, Vec<String>) {
    if cfg!(windows) {
        (
            "cmd".to_string(),
            vec!["/C".to_string(), format!("echo {text}")],
        )
    } else {
        ("echo".to_string(), vec![text.to_string()])
    }
}

/// A command that stays alive for about `seconds`, then exits.
///
/// Windows needs a blocking primitive that wants neither a console nor a
/// socket: `timeout` refuses redirected input, and `ping` needs the network a
/// confined profile may not grant.
pub fn sleep_command(seconds: u32) -> (String, Vec<String>) {
    if cfg!(windows) {
        (
            "powershell".to_string(),
            vec![
                "-NoProfile".to_string(),
                "-NonInteractive".to_string(),
                "-Command".to_string(),
                format!("Start-Sleep -Seconds {seconds}"),
            ],
        )
    } else {
        ("sleep".to_string(), vec![seconds.to_string()])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_fixture_names_a_program_this_host_really_has() {
        let (program, args) = echo_command("hi");
        assert!(!program.is_empty());
        assert!(args.iter().any(|arg| arg.contains("hi")));
        let (program, args) = sleep_command(2);
        assert!(!program.is_empty());
        assert!(args.iter().any(|arg| arg.contains('2')));
    }
}
