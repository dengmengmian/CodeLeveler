//! `$EDITOR` round trip for the composer (Ctrl+X Ctrl+E, `/editor`).
//!
//! Writing a long prompt inside a few-line composer is the worst part of the
//! input box, so the draft goes out to a real editor and comes back. The TUI is
//! suspended for the duration: the editor owns the terminal, and taking it back
//! afterwards is what keeps the screen from being left in raw mode.
//!
//! The command line is run through the shell (`sh -c` / `cmd /C`) exactly like
//! git does, so `EDITOR="code -w"`, quoted paths, and shell aliases all behave
//! the way they do everywhere else on the system.

use std::io::{self, Stdout, Write};
use std::path::Path;
use std::process::Command;

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::terminal::{
    DisableLineWrap, EnableLineWrap, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::{cursor, execute};

/// Fallback when neither `$VISUAL` nor `$EDITOR` is set. `vi` is guaranteed on
/// POSIX systems; Windows always has Notepad.
#[cfg(windows)]
const DEFAULT_EDITOR: &str = "notepad";
#[cfg(not(windows))]
const DEFAULT_EDITOR: &str = "vi";

/// Which command line to run, following the same precedence as git and the
/// shell: `$VISUAL` wins over `$EDITOR`; blank values count as unset.
fn editor_command_line(visual: Option<&str>, editor: Option<&str>) -> String {
    for candidate in [visual, editor] {
        if let Some(value) = candidate.map(str::trim).filter(|v| !v.is_empty()) {
            return value.to_string();
        }
    }
    DEFAULT_EDITOR.to_string()
}

/// Read the configured editor from the process environment.
fn configured_editor() -> String {
    let env = leveler_core::environment();
    editor_command_line(env.var("VISUAL").as_deref(), env.var("EDITOR").as_deref())
}

/// Open `text` in the user's editor and return what they left behind.
///
/// Blocks until the editor exits. `Err` means the editor could not be run or
/// exited non-zero — the caller keeps the draft untouched in that case. The
/// returned text is whatever is in the file, including empty: deleting
/// everything is how a user abandons a prompt.
pub fn edit_text(text: &str) -> Result<String, String> {
    let command_line = configured_editor();
    let dir = tempfile::Builder::new()
        .prefix("leveler-prompt-")
        .tempdir()
        .map_err(|e| format!("无法创建临时目录：{e}"))?;
    // `.md` so editors pick sane wrapping and highlighting for prose.
    let path = dir.path().join("PROMPT_EDITMSG.md");
    std::fs::write(&path, text).map_err(|e| format!("无法写入临时文件：{e}"))?;

    let status = run_editor(&command_line, &path)?;
    if !status {
        return Err(format!("`{command_line}` 以非零状态退出，草稿保持不变"));
    }
    std::fs::read_to_string(&path).map_err(|e| format!("无法读回临时文件：{e}"))
}

/// Run the editor command with `path` appended, through the platform shell.
fn run_editor(command_line: &str, path: &Path) -> Result<bool, String> {
    let mut command = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.arg("/C")
            .arg(format!("{command_line} \"{}\"", path.display()));
        c
    } else {
        let mut c = Command::new("sh");
        // `"$1"` keeps paths with spaces intact without re-quoting the user's
        // own command line.
        c.arg("-c")
            .arg(format!("{command_line} \"$1\""))
            .arg("sh")
            .arg(path);
        c
    };
    let status = command
        .status()
        .map_err(|e| format!("无法启动 `{command_line}`：{e}"))?;
    Ok(status.success())
}

/// Hand the terminal to a child process: leave the alternate screen and undo
/// every mode the TUI turned on, so the editor sees a normal terminal.
pub fn suspend_terminal(stdout: &mut Stdout) -> io::Result<()> {
    execute!(
        stdout,
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste,
        EnableLineWrap,
        cursor::Show
    )?;
    disable_raw_mode()?;
    stdout.flush()
}

/// Take the terminal back after the child exits, restoring what
/// [`suspend_terminal`] undid.
///
/// The alternate screen is deliberately *not* re-entered here: the caller drops
/// its `Terminal`, and the next paint re-enters and clears, so the workbench
/// comes back drawn rather than as a stale frame.
pub fn resume_terminal(stdout: &mut Stdout) -> io::Result<()> {
    enable_raw_mode()?;
    execute!(
        stdout,
        EnableBracketedPaste,
        EnableMouseCapture,
        DisableLineWrap
    )?;
    stdout.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visual_wins_over_editor() {
        assert_eq!(editor_command_line(Some("nvim"), Some("vi")), "nvim");
    }

    #[test]
    fn editor_is_used_when_visual_is_unset() {
        assert_eq!(editor_command_line(None, Some("nano")), "nano");
    }

    #[test]
    fn blank_values_count_as_unset() {
        assert_eq!(
            editor_command_line(Some("   "), Some("")),
            DEFAULT_EDITOR,
            "an empty VISUAL must not shadow the default"
        );
    }

    #[test]
    fn arguments_are_kept_so_wait_flags_survive() {
        // `code -w` without the flag returns instantly and the prompt comes
        // back empty — the flag is the whole point of configuring it.
        assert_eq!(
            editor_command_line(Some("code -w"), None),
            "code -w",
            "arguments must reach the shell verbatim"
        );
    }

    #[test]
    fn nothing_configured_falls_back_to_a_guaranteed_editor() {
        assert_eq!(editor_command_line(None, None), DEFAULT_EDITOR);
    }
}
