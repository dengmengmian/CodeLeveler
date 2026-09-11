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

/// A command that writes `out_lines` padded lines to stdout and `err_lines` to
/// stderr, for tests that need both streams to overflow an output budget.
///
/// The Unix spelling is `/bin/sh -c awk`, which says nothing about the
/// requirement and does not exist on Windows.
pub fn dual_stream_command(out_lines: usize, err_lines: usize) -> (String, Vec<String>) {
    const OUT: &str = "OUT padding padding padding padding padding";
    const ERR: &str = "ERR padding padding padding padding padding";
    if cfg!(windows) {
        let script = format!(
            "for($i=0;$i -lt {out_lines};$i++){{Write-Output ('{OUT}'+$i)}};\
             for($j=0;$j -lt {err_lines};$j++){{[Console]::Error.WriteLine('{ERR}'+$j)}}"
        );
        (
            "powershell".to_string(),
            vec![
                "-NoProfile".to_string(),
                "-NonInteractive".to_string(),
                "-Command".to_string(),
                script,
            ],
        )
    } else {
        let script = format!(
            "awk 'BEGIN {{ for (i = 0; i < {out_lines}; i++) print \"{OUT}\" i; \
             for (j = 0; j < {err_lines}; j++) print \"{ERR}\" j > \"/dev/stderr\" }}'"
        );
        ("/bin/sh".to_string(), vec!["-c".to_string(), script])
    }
}

/// [`sleep_command`] spelled as one shell line, for tests that drive the user
/// shell rather than a [`ProcessRequest`].
pub fn sleep_shell_line(seconds: u32) -> String {
    let (program, args) = sleep_command(seconds);
    let mut line = program;
    for argument in args {
        line.push(' ');
        if argument.contains(' ') {
            line.push('"');
            line.push_str(&argument);
            line.push('"');
        } else {
            line.push_str(&argument);
        }
    }
    line
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

    #[test]
    fn the_dual_stream_fixture_names_both_line_counts() {
        let (program, args) = dual_stream_command(11, 7);
        assert!(!program.is_empty());
        let joined = args.join(" ");
        assert!(joined.contains("11") && joined.contains('7'), "{joined}");
    }

    #[test]
    fn the_shell_line_carries_the_same_command() {
        let line = sleep_shell_line(7);
        let (program, _) = sleep_command(7);
        assert!(line.starts_with(&program), "{line}");
        assert!(line.contains('7'), "{line}");
    }
}
