//! Clipboard routing distinguishes local readback from unacknowledged terminal forwarding.
use crate::terminal_diagnostics::{Capability, TerminalDiagnostics};
use base64::{Engine, engine::general_purpose::STANDARD};
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const MAX_OSC52_BYTES: usize = 64 * 1024;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardRoute {
    Native,
    Tmux,
    Osc52,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    VerifiedNative,
    SentUnverified,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyReceipt {
    pub delivery: Delivery,
    pub routes: Vec<ClipboardRoute>,
    pub failures: Vec<String>,
}
trait Backend {
    fn native(&mut self, text: &str) -> Result<bool, String>;
    fn tmux(&mut self, text: &str) -> Result<(), String>;
    fn osc52(&mut self, bytes: &[u8]) -> Result<(), String>;
}
// Conservative raw-byte limit, not a guarantee that every emulator accepts this size.
fn sequence(text: &str, tmux: bool) -> Result<Vec<u8>, String> {
    if text.len() > MAX_OSC52_BYTES {
        return Err(format!(
            "OSC 52 payload exceeds {MAX_OSC52_BYTES} bytes; not sent"
        ));
    }
    // XTerm OSC 52 selects CLIPBOARD ('c'); the payload is RFC 4648 base64,
    // so selected text cannot inject terminal commands.
    let osc = format!("\x1b]52;c;{}\x07", STANDARD.encode(text.as_bytes()));
    if tmux {
        // tmux DCS passthrough requires each embedded ESC doubled. Whether
        // allow-passthrough is enabled is deliberately not assumed here.
        Ok(format!("\x1bPtmux;{}\x1b\\", osc.replace('\x1b', "\x1b\x1b")).into_bytes())
    } else {
        Ok(osc.into_bytes())
    }
}
fn copy_with(
    text: &str,
    env: &TerminalDiagnostics,
    backend: &mut impl Backend,
) -> Result<CopyReceipt, String> {
    let mut receipt = CopyReceipt {
        delivery: Delivery::SentUnverified,
        routes: Vec::new(),
        failures: Vec::new(),
    };
    if text.is_empty() {
        return Ok(receipt);
    }
    // SSH native readback belongs to the remote host. A tmux server's inherited
    // environment also cannot identify its current attached client: it may have
    // attached later over SSH. Neither boundary proves a native clipboard is
    // the user's clipboard, so do not write it or claim verified delivery.
    if !env.remote && !env.tmux {
        match backend.native(text) {
            Ok(verified) => {
                receipt.routes.push(ClipboardRoute::Native);
                if verified {
                    receipt.delivery = Delivery::VerifiedNative;
                } else {
                    receipt.failures.push(
                        "native clipboard write succeeded; readback unavailable or mismatched"
                            .into(),
                    );
                }
            }
            Err(error) => receipt.failures.push(error),
        }
    }
    if env.tmux {
        match backend.tmux(text) {
            Ok(()) => receipt.routes.push(ClipboardRoute::Tmux),
            Err(error) => receipt.failures.push(error),
        }
    }
    if receipt.delivery != Delivery::VerifiedNative {
        if env.osc52 == Capability::Unavailable {
            receipt
                .failures
                .push("OSC 52 unavailable for this terminal/output".into());
        } else {
            match sequence(text, env.tmux).and_then(|bytes| backend.osc52(&bytes)) {
                Ok(()) => receipt.routes.push(ClipboardRoute::Osc52),
                Err(error) => receipt.failures.push(error),
            }
        }
    }
    if receipt.routes.is_empty() {
        Err(receipt.failures.join("; "))
    } else {
        Ok(receipt)
    }
}

struct SystemBackend;
impl Backend for SystemBackend {
    fn native(&mut self, text: &str) -> Result<bool, String> {
        // Keep ownership alive on platforms where the clipboard is serviced by
        // the source application. Dropping the source can erase a Linux copy.
        static CLIPBOARD: OnceLock<Mutex<Option<arboard::Clipboard>>> = OnceLock::new();
        let mut slot = CLIPBOARD
            .get_or_init(|| Mutex::new(None))
            .lock()
            .map_err(|_| "native clipboard lock poisoned".to_string())?;
        if slot.is_none() {
            *slot = Some(
                arboard::Clipboard::new()
                    .map_err(|error| format!("native clipboard unavailable: {error}"))?,
            );
        }
        let clipboard = slot.as_mut().expect("initialized clipboard");
        clipboard
            .set_text(text.to_owned())
            .map_err(|error| format!("native clipboard write failed: {error}"))?;
        Ok(clipboard.get_text().is_ok_and(|readback| readback == text))
    }
    fn tmux(&mut self, text: &str) -> Result<(), String> {
        // Pass the payload through a private, auto-cleaned file, never command
        // arguments or a blocking stdin pipe. This writes the tmux paste buffer;
        // current-client OSC 52 handles outer clipboard forwarding separately.
        let mut file = tempfile::NamedTempFile::new()
            .map_err(|error| format!("tmux copy temporary file failed: {error}"))?;
        file.write_all(text.as_bytes())
            .map_err(|error| format!("tmux copy temporary write failed: {error}"))?;
        let mut child = Command::new("tmux")
            .arg("load-buffer")
            .arg(file.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("tmux clipboard unavailable: {error}"))?;
        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    return if status.success() {
                        Ok(())
                    } else {
                        Err(format!("tmux buffer write failed ({status})"))
                    };
                }
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                result => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(match result {
                        Err(error) => format!("tmux buffer wait failed: {error}"),
                        _ => "tmux buffer write timed out".into(),
                    });
                }
            }
        }
    }
    fn osc52(&mut self, bytes: &[u8]) -> Result<(), String> {
        let mut stdout = std::io::stdout().lock();
        stdout
            .write_all(bytes)
            .and_then(|()| stdout.flush())
            .map_err(|error| format!("OSC 52 write failed (delivery unknown): {error}"))
    }
}

pub fn copy(text: &str) -> Result<CopyReceipt, String> {
    copy_with(text, &TerminalDiagnostics::detect(), &mut SystemBackend)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Fake {
        native_calls: usize,
        tmux_calls: usize,
        output: Vec<u8>,
        fail: bool,
        readback: bool,
    }
    impl Backend for Fake {
        fn native(&mut self, _: &str) -> Result<bool, String> {
            self.native_calls += 1;
            if self.fail {
                Err("native failed".into())
            } else {
                Ok(self.readback)
            }
        }
        fn tmux(&mut self, _: &str) -> Result<(), String> {
            self.tmux_calls += 1;
            if self.fail {
                Err("tmux failed".into())
            } else {
                Ok(())
            }
        }
        fn osc52(&mut self, bytes: &[u8]) -> Result<(), String> {
            if self.fail {
                Err("write failed".into())
            } else {
                self.output.extend_from_slice(bytes);
                Ok(())
            }
        }
    }
    fn env(remote: bool, tmux: bool) -> TerminalDiagnostics {
        TerminalDiagnostics {
            remote,
            tmux,
            stdout_terminal: true,
            osc52: Capability::Unknown,
            term: None,
            colorterm: None,
        }
    }
    #[test]
    fn remote_copy_never_writes_remote_native_clipboard() {
        let mut backend = Fake::default();
        let receipt = copy_with("secret", &env(true, false), &mut backend).unwrap();
        assert_eq!(backend.native_calls, 0);
        assert_eq!(receipt.delivery, Delivery::SentUnverified);
        assert_eq!(receipt.routes, [ClipboardRoute::Osc52]);
    }
    #[test]
    fn tmux_without_ssh_environment_never_claims_native_client_delivery() {
        let mut backend = Fake {
            readback: true,
            ..Default::default()
        };
        let receipt = copy_with("text", &env(false, true), &mut backend).unwrap();
        assert_eq!(backend.native_calls, 0);
        assert_eq!(receipt.delivery, Delivery::SentUnverified);
        assert_eq!(
            receipt.routes,
            [ClipboardRoute::Tmux, ClipboardRoute::Osc52]
        );
    }
    #[test]
    fn native_write_without_readback_is_not_verified() {
        let mut backend = Fake::default();
        let receipt = copy_with("text", &env(false, false), &mut backend).unwrap();
        assert_eq!(receipt.delivery, Delivery::SentUnverified);
    }
    #[test]
    fn native_matching_readback_is_verified() {
        let mut backend = Fake {
            readback: true,
            ..Default::default()
        };
        assert_eq!(
            copy_with("text", &env(false, false), &mut backend)
                .unwrap()
                .delivery,
            Delivery::VerifiedNative
        );
    }
    #[test]
    fn osc52_encodes_controls_and_tmux_doubles_escape() {
        assert_eq!(sequence("a\x1b\x07", false).unwrap(), b"\x1b]52;c;YRsH\x07");
        assert_eq!(
            sequence("a", true).unwrap(),
            b"\x1bPtmux;\x1b\x1b]52;c;YQ==\x07\x1b\\"
        );
    }
    #[test]
    fn oversized_payload_is_rejected_before_any_terminal_write() {
        let mut backend = Fake::default();
        assert!(
            copy_with(
                &"x".repeat(MAX_OSC52_BYTES + 1),
                &env(true, false),
                &mut backend
            )
            .is_err()
        );
        assert!(backend.output.is_empty());
        assert_eq!(backend.native_calls, 0);
        assert!(sequence(&"x".repeat(MAX_OSC52_BYTES), false).is_ok());
    }
    #[test]
    fn empty_copy_has_no_side_effect_and_does_not_clear_clipboard() {
        let mut backend = Fake::default();
        assert!(
            copy_with("", &env(false, true), &mut backend)
                .unwrap()
                .routes
                .is_empty()
        );
        assert_eq!(backend.native_calls + backend.tmux_calls, 0);
        assert!(backend.output.is_empty());
    }
    #[test]
    fn unavailable_remote_terminal_returns_error_without_native_side_effect() {
        let mut backend = Fake::default();
        let mut terminal = env(true, false);
        terminal.osc52 = Capability::Unavailable;
        assert!(copy_with("text", &terminal, &mut backend).is_err());
        assert_eq!(backend.native_calls, 0);
        assert!(backend.output.is_empty());
    }
    #[test]
    fn route_failure_is_not_success() {
        let mut backend = Fake {
            fail: true,
            ..Default::default()
        };
        let error = copy_with("text", &env(true, true), &mut backend).unwrap_err();
        assert!(error.contains("tmux failed") && error.contains("write failed"));
        assert_eq!(backend.native_calls, 0);
    }
    #[test]
    fn tmux_success_reports_sent_and_also_forwards_terminal() {
        let mut backend = Fake::default();
        let receipt = copy_with("text", &env(true, true), &mut backend).unwrap();
        assert_eq!(
            receipt.routes,
            [ClipboardRoute::Tmux, ClipboardRoute::Osc52]
        );
        assert_eq!(receipt.delivery, Delivery::SentUnverified);
        assert_eq!(backend.tmux_calls, 1);
    }
}
