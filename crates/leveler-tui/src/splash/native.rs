//! Native PNG presentation state. Capability selection never reads terminal input.

use image::{DynamicImage, ImageFormat};
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::StatefulProtocol;

const PNG: &[u8] = include_bytes!("../../../../assets/brand/codeleveler-mascot.png");

pub(crate) struct NativeMascot {
    picker: Picker,
    source: DynamicImage,
    pub(crate) protocol: StatefulProtocol,
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
        let source = image::load_from_memory_with_format(PNG, ImageFormat::Png)?;
        let mut picker = Picker::from_fontsize(font);
        picker.set_protocol_type(protocol);
        picker.set_background_color([0, 0, 0, 0]);
        let protocol = picker.new_resize_protocol(source.clone());
        Ok(Self {
            picker,
            source,
            protocol,
        })
    }

    /// Source pixel dimensions, so layout can match the illustration's aspect.
    pub(crate) fn aspect(&self) -> (u32, u32) {
        (self.source.width(), self.source.height())
    }

    /// An alternate screen reset invalidates Kitty's transmitted-image cache.
    pub(crate) fn reset(&mut self) {
        self.protocol = self.picker.new_resize_protocol(self.source.clone());
    }
}

fn protocol_for(
    program: Option<&str>,
    term: Option<&str>,
    multiplexer: bool,
) -> Option<ProtocolType> {
    if multiplexer {
        return None;
    }
    if matches!(term, Some("xterm-kitty" | "xterm-ghostty"))
        || matches!(program, Some("kitty" | "ghostty"))
    {
        return Some(ProtocolType::Kitty);
    }
    match program {
        Some("iTerm.app" | "WezTerm") => Some(ProtocolType::Iterm2),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capabilities_choose_native_protocols_only() {
        assert_eq!(
            protocol_for(None, Some("xterm-kitty"), false),
            Some(ProtocolType::Kitty)
        );
        assert_eq!(
            protocol_for(Some("ghostty"), Some("xterm-ghostty"), false),
            Some(ProtocolType::Kitty)
        );
        for app in ["iTerm.app", "WezTerm"] {
            assert_eq!(
                protocol_for(Some(app), None, false),
                Some(ProtocolType::Iterm2)
            );
        }
        for app in [None, Some("Apple_Terminal"), Some("vscode")] {
            assert_eq!(protocol_for(app, Some("xterm-256color"), false), None);
        }
        assert_eq!(
            protocol_for(Some("iTerm.app"), Some("xterm-kitty"), true),
            None
        );
    }
    #[test]
    fn source_png_keeps_its_transparent_pixels() {
        let image = NativeMascot::new(ProtocolType::Iterm2, (10, 20)).unwrap();
        assert_eq!((image.source.width(), image.source.height()), (1312, 1199));
        assert_eq!(image.source.to_rgba8().get_pixel(0, 0)[3], 0);
        assert_eq!(image.protocol.background_color().0, [0, 0, 0, 0]);
    }
}
