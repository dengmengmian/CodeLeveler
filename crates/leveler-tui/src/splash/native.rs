//! Native PNG presentation state. Capability selection never reads terminal input.

use image::{DynamicImage, ImageFormat};
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::{StatefulProtocol, StatefulProtocolType};

const PNG: &[u8] = include_bytes!("../../../../assets/brand/codeleveler-mascot.png");

pub(crate) struct NativeMascot {
    picker: Picker,
    source: DynamicImage,
    pub(crate) protocol: StatefulProtocol,
    /// Kitty stores transmitted image data by id, outside the character grid.
    /// The id must be deleted explicitly when the slot is replaced or retired.
    uploaded: bool,
    failed: bool,
    pending_cleanup: Vec<String>,
}

impl std::fmt::Debug for NativeMascot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeMascot")
            .field("protocol", &self.picker.protocol_type())
            .finish_non_exhaustive()
    }
}

impl NativeMascot {
    pub(crate) fn detect() -> Result<Option<Self>, image::ImageError> {
        let env = leveler_core::environment();
        let multiplexer = env.var_os("TMUX").is_some() || env.var_os("STY").is_some();
        let wt_session = env.var_os("WT_SESSION").is_some();
        // Windows Terminal does not set TERM or TERM_PROGRAM. Its DA1 reply
        // advertises Sixel with attribute 4, so ask the terminal rather than
        // guessing from the OS name. The probe is bounded and runs before the
        // first frame; a failed probe simply keeps the image hidden.
        let sixel = wt_session && !multiplexer && probe_sixel();
        let protocol = protocol_for(
            env.var("TERM_PROGRAM").as_deref(),
            env.var("TERM").as_deref(),
            env.var("LC_TERMINAL").as_deref(),
            multiplexer,
            wt_session,
            sixel,
        );
        let Some(protocol) = protocol else {
            return Ok(None);
        };
        let font = crossterm::terminal::window_size()
            .ok()
            .and_then(|size| {
                let w = size.width.checked_div(size.columns)?;
                let h = size.height.checked_div(size.rows)?;
                (w > 0 && h > 0).then_some((w, h))
            })
            .unwrap_or((10, 20));
        Self::new(protocol, font).map(Some)
    }

    pub(crate) fn new(protocol: ProtocolType, font: (u16, u16)) -> Result<Self, image::ImageError> {
        Self::from_png(PNG, protocol, font)
    }

    fn from_png(
        bytes: &[u8],
        protocol: ProtocolType,
        font: (u16, u16),
    ) -> Result<Self, image::ImageError> {
        let source = image::load_from_memory_with_format(bytes, ImageFormat::Png)?;
        // The painted slot is at most ~22x10 terminal cells. Encoding the
        // 1312px master on startup adds seconds of CPU for pixels no terminal
        // can show; keep a high-resolution working copy that still covers a
        // large window and high-DPI cell metrics.
        let source = if source.width() > 512 || source.height() > 512 {
            source.resize(512, 512, image::imageops::FilterType::Lanczos3)
        } else {
            source
        };
        let mut picker = Picker::from_fontsize(font);
        picker.set_protocol_type(protocol);
        picker.set_background_color([0, 0, 0, 0]);
        let protocol = picker.new_resize_protocol(source.clone());
        Ok(Self {
            picker,
            source,
            protocol,
            uploaded: false,
            failed: false,
            pending_cleanup: Vec::new(),
        })
    }

    /// Source pixel dimensions, so layout can match the illustration's aspect.
    pub(crate) fn aspect(&self) -> (u32, u32) {
        (self.source.width(), self.source.height())
    }

    /// A protocol failure keeps the allocation alive so its cleanup still owns
    /// the transmitted image id; layout must stop reserving a slot for it.
    pub(crate) fn usable(&self) -> bool {
        !self.failed
    }

    pub(crate) fn mark_failed(&mut self) {
        self.failed = true;
        self.queue_kitty_delete();
    }

    /// The frame rendered the image successfully, so the terminal now holds it.
    pub(crate) fn mark_uploaded(&mut self) {
        self.uploaded = true;
    }

    /// An alternate screen reset invalidates Kitty's transmitted-image cache.
    /// The old id is deleted before the replacement is transmitted.
    pub(crate) fn reset(&mut self) {
        self.queue_kitty_delete();
        self.protocol = self.picker.new_resize_protocol(self.source.clone());
    }

    /// Bake the card colour under the PNG's transparent pixels. iTerm2
    /// composites inline images over the window backdrop, not the cell
    /// background, so a transparent margin reveals a terminal background
    /// image as a stray rectangle. Kitty already blends over the card; the
    /// same opaque colour is indistinguishable there.
    pub(crate) fn set_background(&mut self, background: [u8; 4]) {
        if self.protocol.background_color().0 == background {
            return;
        }
        self.queue_kitty_delete();
        self.picker.set_background_color(background);
        self.protocol = self.picker.new_resize_protocol(self.source.clone());
    }

    /// Drop the terminal-side resource once the welcome card is gone for good
    /// (first real message, or process exit). Idempotent.
    pub(crate) fn retire(&mut self) {
        self.queue_kitty_delete();
    }

    /// Protocol-specific delete sequences accumulated since the last flush.
    pub(crate) fn take_cleanup(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pending_cleanup)
    }

    fn queue_kitty_delete(&mut self) {
        if !self.uploaded {
            return;
        }
        self.uploaded = false;
        if let StatefulProtocolType::Kitty(kitty) = self.protocol.protocol_type() {
            // d=I removes this image's data and every placement; q=2 silences
            // an already-gone image without affecting other applications.
            self.pending_cleanup
                .push(format!("\x1b_Ga=d,d=I,i={},q=2\x1b\\", kitty.unique_id));
        }
    }
}

fn protocol_for(
    program: Option<&str>,
    term: Option<&str>,
    lc_terminal: Option<&str>,
    multiplexer: bool,
    wt_session: bool,
    sixel: bool,
) -> Option<ProtocolType> {
    if multiplexer {
        return None;
    }
    let term = term.map(str::trim).unwrap_or_default();
    if term.eq_ignore_ascii_case("dumb") {
        return None;
    }
    // DA1 attribute 4 is the terminal's own claim that it can decode Sixel;
    // WT_SESSION only says which host asked. Both are required.
    if wt_session && sixel {
        return Some(ProtocolType::Sixel);
    }
    // An unset TERM cannot tell us what the terminal is; dumb and the
    // multiplexer TERM families are known to be unsafe for inline graphics.
    if term.is_empty() {
        return None;
    }
    let term = term.to_ascii_lowercase();
    if term.starts_with("screen") || term.starts_with("tmux") {
        return None;
    }
    let program = program.map(str::trim).filter(|program| !program.is_empty());
    match program {
        // Explicitly text-only programs outrank a misleading TERM.
        Some("Apple_Terminal" | "vscode" | "Windows_Terminal" | "tmux" | "screen") => {
            return None;
        }
        Some("kitty" | "ghostty") => return Some(ProtocolType::Kitty),
        Some("iTerm.app" | "WezTerm") => return Some(ProtocolType::Iterm2),
        _ => {}
    }
    if term == "xterm-kitty" || term == "xterm-ghostty" {
        return Some(ProtocolType::Kitty);
    }
    // iTerm2 keeps LC_TERMINAL across SSH sessions whose TERM is generic.
    let lc_terminal = lc_terminal.map(str::trim).unwrap_or_default();
    if lc_terminal.to_ascii_lowercase().contains("iterm") {
        return Some(ProtocolType::Iterm2);
    }
    None
}

/// Ask the terminal for its primary device attributes (DA1).
///
/// Only Windows Terminal needs this path: it sets neither TERM nor
/// TERM_PROGRAM, and its DA1 reply (`ESC[?61;4;...c`) advertises Sixel with
/// attribute 4. The probe is bounded to 150 ms and runs after raw mode is
/// enabled but before the first frame. A key already waiting means the user is
/// typing, so the probe backs off instead of consuming it.
#[cfg(windows)]
fn probe_sixel() -> bool {
    use std::io::Write;
    use std::time::{Duration, Instant};

    use crossterm::event::{Event, KeyCode, poll, read};

    // A just-created terminal window often has a Resize or mouse event queued.
    // Those are terminal bookkeeping, not user input; discard them after a
    // short settle so the DA1 reply can be read. A pending key does back off.
    std::thread::sleep(Duration::from_millis(30));
    let mut key_pending = false;
    for _ in 0..64 {
        match poll(Duration::ZERO) {
            Ok(true) => match read() {
                Ok(Event::Key(_)) => {
                    key_pending = true;
                    break;
                }
                Ok(_) => {}
                Err(_) => break,
            },
            _ => break,
        }
    }
    if key_pending {
        return false;
    }
    let mut stdout = std::io::stdout();
    if stdout
        .write_all(b"\x1b[c")
        .and_then(|_| stdout.flush())
        .is_err()
    {
        return false;
    }
    let deadline = Instant::now() + Duration::from_millis(150);
    let mut response = String::new();
    while Instant::now() < deadline {
        let left = deadline.saturating_duration_since(Instant::now());
        match poll(left) {
            Ok(true) => match read() {
                Ok(Event::Key(key)) => {
                    match key.code {
                        KeyCode::Esc => response.push('\x1b'),
                        KeyCode::Char(c) => response.push(c),
                        _ => {}
                    }
                    if response.ends_with('c') {
                        break;
                    }
                }
                Ok(_) => {}
                Err(_) => return false,
            },
            _ => break,
        }
    }
    da1_advertises_sixel(&response)
}

#[cfg(not(windows))]
fn probe_sixel() -> bool {
    false
}

#[cfg(any(windows, test))]
fn da1_advertises_sixel(response: &str) -> bool {
    // Crossterm delivers the reply as key events and may consume the leading
    // ESC as the start of the sequence. Accept both the raw and the
    // ESC-stripped form; the `[?` prefix and trailing `c` are the identity.
    let start = response
        .find("\x1b[?")
        .map(|index| index + 3)
        .or_else(|| response.find("[?").map(|index| index + 2));
    let Some(start) = start else {
        return false;
    };
    let rest = &response[start..];
    let Some(end) = rest.find('c') else {
        return false;
    };
    rest[..end].split(';').any(|param| param.trim() == "4")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Eq, Clone, Copy)]
    enum Seen {
        Kitty,
        Iterm2,
        Sixel,
        Hidden,
    }

    fn seen_with(
        program: Option<&str>,
        term: Option<&str>,
        lc_terminal: Option<&str>,
        multiplexer: bool,
        wt_session: bool,
        sixel: bool,
    ) -> Seen {
        match protocol_for(program, term, lc_terminal, multiplexer, wt_session, sixel) {
            Some(ProtocolType::Kitty) => Seen::Kitty,
            Some(ProtocolType::Iterm2) => Seen::Iterm2,
            Some(ProtocolType::Sixel) => Seen::Sixel,
            None => Seen::Hidden,
            other => panic!("unexpected protocol {other:?}"),
        }
    }

    fn seen(
        program: Option<&str>,
        term: Option<&str>,
        lc_terminal: Option<&str>,
        multiplexer: bool,
    ) -> Seen {
        seen_with(program, term, lc_terminal, multiplexer, false, false)
    }

    #[test]
    fn supported_terminals_select_their_native_protocol() {
        for (program, term) in [
            (Some("kitty"), Some("xterm-kitty")),
            (Some("ghostty"), Some("xterm-ghostty")),
            (None, Some("xterm-kitty")),
            (None, Some("xterm-ghostty")),
        ] {
            assert_eq!(
                seen(program, term, None, false),
                Seen::Kitty,
                "{program:?} {term:?}"
            );
        }
        for (program, term) in [
            (Some("iTerm.app"), Some("xterm-256color")),
            (Some("WezTerm"), Some("xterm-256color")),
        ] {
            assert_eq!(
                seen(program, term, None, false),
                Seen::Iterm2,
                "{program:?} {term:?}"
            );
        }
        // iTerm2 over SSH commonly inherits a generic TERM and reports LC_TERMINAL.
        assert_eq!(
            seen(None, Some("xterm-256color"), Some("iTerm2"), false),
            Seen::Iterm2
        );
    }

    #[test]
    fn windows_terminal_sixel_probe_enables_native_images() {
        assert_eq!(seen_with(None, None, None, false, true, true), Seen::Sixel);
        assert_eq!(
            seen_with(
                Some("Windows_Terminal"),
                Some("xterm-256color"),
                None,
                false,
                true,
                true
            ),
            Seen::Sixel
        );
        // The host identity alone is not capability: without the DA1 claim
        // the image stays hidden.
        assert_eq!(
            seen_with(None, None, None, false, true, false),
            Seen::Hidden
        );
        // A positive probe is not enough without the host that answers DA1.
        assert_eq!(
            seen_with(None, Some("xterm-256color"), None, false, false, true),
            Seen::Hidden
        );
        // Multiplexers and dumb terminals stay text-only.
        assert_eq!(seen_with(None, None, None, true, true, true), Seen::Hidden);
        assert_eq!(
            seen_with(None, Some("dumb"), None, false, true, true),
            Seen::Hidden
        );
    }

    #[test]
    fn da1_reply_with_sixel_attribute_is_recognized() {
        assert!(da1_advertises_sixel(
            "\x1b[?61;4;6;7;14;21;22;23;24;28;32;42;52c"
        ));
        // Crossterm can strip the leading ESC while parsing the reply.
        assert!(da1_advertises_sixel(
            "[?61;4;6;7;14;21;22;23;24;28;32;42;52c"
        ));
        assert!(da1_advertises_sixel("user text\x1b[?61;4;6;7c trailing"));
        assert!(!da1_advertises_sixel("\x1b[?1;2c"));
        assert!(!da1_advertises_sixel("\x1b[?61;42;52c"));
        assert!(!da1_advertises_sixel(""));
    }

    #[test]
    fn unknown_or_text_only_terminals_stay_hidden() {
        for (program, term) in [
            (Some("Apple_Terminal"), Some("xterm-256color")),
            (Some("vscode"), Some("xterm-256color")),
            (Some("Windows_Terminal"), Some("xterm-256color")),
            (None, Some("xterm-256color")),
            (None, Some("vt100")),
            (None, None),
        ] {
            assert_eq!(
                seen(program, term, None, false),
                Seen::Hidden,
                "{program:?} {term:?}"
            );
        }
        // A known text-only program outranks a misleading kitty TERM.
        assert_eq!(
            seen(Some("Apple_Terminal"), Some("xterm-kitty"), None, false),
            Seen::Hidden
        );
        assert_eq!(
            seen(Some("vscode"), Some("xterm-ghostty"), None, false),
            Seen::Hidden
        );
        // A known image program wins over a misleading kitty TERM.
        assert_eq!(
            seen(Some("iTerm.app"), Some("xterm-kitty"), None, false),
            Seen::Iterm2
        );
    }

    #[test]
    fn dumb_and_unset_terms_never_render_images() {
        for term in [Some("dumb"), Some("DUMB"), Some(""), None] {
            assert_eq!(
                seen(Some("iTerm.app"), term, Some("iTerm2"), false),
                Seen::Hidden,
                "{term:?}"
            );
            assert_eq!(
                seen(Some("kitty"), term, None, false),
                Seen::Hidden,
                "{term:?}"
            );
        }
    }

    #[test]
    fn multiplexers_are_hidden_even_when_the_outer_terminal_supports_images() {
        for term in [
            Some("screen-256color"),
            Some("screen.xterm-256color"),
            Some("tmux-256color"),
        ] {
            assert_eq!(
                seen(Some("iTerm.app"), term, None, false),
                Seen::Hidden,
                "{term:?}"
            );
        }
        for program in [Some("tmux"), Some("screen")] {
            assert_eq!(
                seen(program, Some("xterm-256color"), None, false),
                Seen::Hidden,
                "{program:?}"
            );
        }
        for (program, term) in [
            (Some("iTerm.app"), Some("xterm-256color")),
            (Some("kitty"), Some("xterm-kitty")),
        ] {
            assert_eq!(seen(program, term, None, true), Seen::Hidden);
        }
    }

    fn kitty_id(protocol: &StatefulProtocol) -> u32 {
        match protocol.protocol_type() {
            StatefulProtocolType::Kitty(kitty) => kitty.unique_id,
            _ => panic!("expected kitty protocol"),
        }
    }

    fn delete(id: u32) -> String {
        format!("\x1b_Ga=d,d=I,i={id},q=2\x1b\\")
    }

    #[test]
    fn kitty_ids_are_deleted_before_replacement_and_on_retirement() {
        let mut image = NativeMascot::new(ProtocolType::Kitty, (10, 20)).unwrap();
        image.reset();
        image.retire();
        assert!(
            image.take_cleanup().is_empty(),
            "nothing was transmitted yet"
        );
        let first = kitty_id(&image.protocol);

        image.mark_uploaded();
        image.reset();
        assert_eq!(image.take_cleanup(), vec![delete(first)]);
        let second = kitty_id(&image.protocol);
        assert_ne!(first, second);

        image.mark_uploaded();
        image.retire();
        assert_eq!(image.take_cleanup(), vec![delete(second)]);
        image.retire();
        assert!(image.take_cleanup().is_empty(), "retirement is idempotent");
    }

    #[test]
    fn a_card_background_replaces_the_transparent_margin_and_its_upload() {
        let mut image = NativeMascot::new(ProtocolType::Kitty, (10, 20)).unwrap();
        image.set_background([17, 19, 21, 255]);
        assert_eq!(image.protocol.background_color().0, [17, 19, 21, 255]);

        let first = kitty_id(&image.protocol);
        image.mark_uploaded();
        image.set_background([250, 250, 250, 255]);
        assert_eq!(image.take_cleanup(), vec![delete(first)]);
        assert_eq!(image.protocol.background_color().0, [250, 250, 250, 255]);

        // A theme repaint with the same canvas must not churn the upload.
        let second = kitty_id(&image.protocol);
        image.mark_uploaded();
        image.set_background([250, 250, 250, 255]);
        assert!(image.take_cleanup().is_empty());
        assert_eq!(kitty_id(&image.protocol), second);
    }

    #[test]
    fn a_failed_encoding_keeps_ownership_of_what_was_transmitted() {
        let mut image = NativeMascot::new(ProtocolType::Kitty, (10, 20)).unwrap();
        let id = kitty_id(&image.protocol);
        image.mark_uploaded();
        image.mark_failed();
        assert!(!image.usable());
        assert_eq!(image.take_cleanup(), vec![delete(id)]);
    }

    #[test]
    fn iterm2_has_no_out_of_band_image_state_to_delete() {
        let mut image = NativeMascot::new(ProtocolType::Iterm2, (10, 20)).unwrap();
        image.mark_uploaded();
        image.reset();
        image.retire();
        assert!(image.take_cleanup().is_empty());
    }

    #[test]
    fn a_corrupt_asset_is_an_error_not_a_panic() {
        assert!(NativeMascot::from_png(b"not a png", ProtocolType::Kitty, (10, 20)).is_err());
        assert!(NativeMascot::from_png(PNG, ProtocolType::Kitty, (10, 20)).is_ok());
    }

    #[test]
    fn source_png_is_bounded_and_keeps_transparency() {
        let image = NativeMascot::new(ProtocolType::Iterm2, (10, 20)).unwrap();
        let (width, height) = image.aspect();
        assert!(width <= 512 && height <= 512);
        let original = 1312.0 / 1199.0;
        let working = f64::from(width) / f64::from(height);
        assert!((original - working).abs() < 0.01);
        assert_eq!(image.source.to_rgba8().get_pixel(0, 0)[3], 0);
        assert_eq!(image.protocol.background_color().0, [0, 0, 0, 0]);
    }

    #[test]
    fn sixel_has_no_out_of_band_cleanup() {
        let mut image = NativeMascot::new(ProtocolType::Sixel, (10, 20)).unwrap();
        image.mark_uploaded();
        image.reset();
        image.retire();
        assert!(image.take_cleanup().is_empty());
    }
}
