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
        let protocol = protocol_for(
            env.var("TERM_PROGRAM").as_deref(),
            env.var("TERM").as_deref(),
            env.var("LC_TERMINAL").as_deref(),
            env.var_os("TMUX").is_some() || env.var_os("STY").is_some(),
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
) -> Option<ProtocolType> {
    if multiplexer {
        return None;
    }
    // An unset TERM cannot tell us what the terminal is; dumb and the
    // multiplexer TERM families are known to be unsafe for inline graphics.
    let term = term.map(str::trim).filter(|term| !term.is_empty())?;
    let term = term.to_ascii_lowercase();
    if term == "dumb" || term.starts_with("screen") || term.starts_with("tmux") {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Eq, Clone, Copy)]
    enum Seen {
        Kitty,
        Iterm2,
        Hidden,
    }

    fn seen(
        program: Option<&str>,
        term: Option<&str>,
        lc_terminal: Option<&str>,
        multiplexer: bool,
    ) -> Seen {
        match protocol_for(program, term, lc_terminal, multiplexer) {
            Some(ProtocolType::Kitty) => Seen::Kitty,
            Some(ProtocolType::Iterm2) => Seen::Iterm2,
            None => Seen::Hidden,
            other => panic!("unexpected protocol {other:?}"),
        }
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
    fn source_png_keeps_its_transparent_pixels() {
        let image = NativeMascot::new(ProtocolType::Iterm2, (10, 20)).unwrap();
        assert_eq!((image.source.width(), image.source.height()), (1312, 1199));
        assert_eq!(image.source.to_rgba8().get_pixel(0, 0)[3], 0);
        assert_eq!(image.protocol.background_color().0, [0, 0, 0, 0]);
    }
}
