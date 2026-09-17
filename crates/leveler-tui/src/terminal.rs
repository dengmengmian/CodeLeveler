//! Terminal lifecycle: raw mode + bracketed paste + mouse capture for the
//! alternate-screen workbench (Conversation scroll wheel / drag).

use std::io::{self, Stdout};

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::terminal::{
    DisableLineWrap, EnableLineWrap, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::{cursor, execute};

/// Owns raw/paste/wrap/mouse state and restores it on `Drop`, even on panic.
pub struct TerminalGuard {
    restored: bool,
}

impl TerminalGuard {
    /// Enter raw mode + bracketed paste + mouse capture, disabling line wrap.
    ///
    /// The guard is armed immediately after the first state change so any later
    /// init failure (or panic) still restores cooked mode.
    pub fn enter() -> io::Result<(Self, Stdout)> {
        install_panic_hook();
        enable_raw_mode()?;
        // Armed now: Drop restores raw mode even if the next execute! fails.
        let mut guard = Self { restored: false };
        let mut stdout = io::stdout();
        if let Err(err) = execute!(
            stdout,
            EnableBracketedPaste,
            EnableMouseCapture,
            DisableLineWrap
        ) {
            guard.restore();
            return Err(err);
        }
        Ok((guard, stdout))
    }

    /// Restore the terminal to its cooked state. Idempotent.
    pub fn restore(&mut self) {
        if self.restored {
            return;
        }
        self.restored = true;
        restore_terminal();
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Best-effort restore, used both by the guard and the panic hook.
fn restore_terminal() {
    let _ = disable_raw_mode();
    // Leave the alternate screen in case an overlay was open; harmless if not.
    let _ = execute!(
        io::stdout(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste,
        EnableLineWrap,
        cursor::Show
    );
}

/// Install a panic hook (once) that restores the terminal before the default
/// hook prints the panic, so a crash never leaves a garbled terminal.
fn install_panic_hook() {
    use std::sync::Once;
    static HOOK: Once = Once::new();
    HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal();
            previous(info);
        }));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Document the init contract: raw mode is armed before secondary
    /// setup, so a failed later step still restores via Drop/restore().
    #[test]
    fn guard_restore_is_idempotent() {
        let mut guard = TerminalGuard { restored: false };
        // Without having entered raw mode in tests, restore is best-effort.
        guard.restore();
        assert!(guard.restored);
        guard.restore(); // second call must be a no-op
        assert!(guard.restored);
    }
}
