//! Recognising the "I pasted a picture" gesture when the terminal spells it as
//! a path.
//!
//! A terminal has no way to put image bytes into a bracketed paste, so the ones
//! that support Cmd+V on an image write a scratch file and paste its path
//! instead — Ghostty leaves `$TMPDIR/clipboard-2026-09-16-215212-CE7C9522.png`.
//! That path is the terminal's bookkeeping. The user pasted a picture, so the
//! composer must stage an attachment, not type a filesystem path at them.
//!
//! The judgement is made on what is on disk, never on how the text is spelled:
//! prose that merely mentions `/tmp/a.png` stays prose. The runtime's
//! `leveler-media` remains the authority on what an image really is and refuses
//! anything this pre-filter lets through by mistake; being strict here is what
//! keeps pasted *text* from being swallowed.

use std::path::Path;

/// A paste the terminal made on the user's behalf.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PastedImage {
    /// The file to import.
    pub(crate) path: String,
    /// The name to show, when the file name is the terminal's own and means
    /// nothing to the user. `None` keeps the file's own name.
    pub(crate) name: Option<String>,
}

/// The image signatures `leveler-media` accepts, matched on content.
const SIGNATURES: &[&[u8]] = &[
    b"\x89PNG\r\n\x1a\n", // PNG
    b"\xff\xd8\xff",      // JPEG
    b"GIF87a",
    b"GIF89a",
    // WebP is `RIFF....WEBP`; the four size bytes are checked past the prefix.
    b"RIFF",
];

/// The first bytes needed to tell an image from anything else.
const SNIFF_BYTES: usize = 12;

/// Decide whether a bracketed-paste payload is a picture the terminal handed us
/// as a path.
pub(crate) fn pasted_image(text: &str) -> Option<PastedImage> {
    let candidate = unquote(text.trim());
    if candidate.is_empty() || candidate.contains('\n') {
        return None;
    }
    let path = Path::new(candidate);
    if !path.is_file() || !is_image_file(path) {
        return None;
    }
    Some(PastedImage {
        path: candidate.to_string(),
        name: scratch_file_name(path),
    })
}

/// Strip one layer of matching quotes: shells quote a pasted path, the path
/// itself does not carry the quotes.
fn unquote(text: &str) -> &str {
    for quote in ['\'', '"'] {
        if let Some(inner) = text
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            return inner;
        }
    }
    text
}

/// Read only the first bytes and match them against the image signatures.
fn is_image_file(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut head = [0u8; SNIFF_BYTES];
    let Ok(read) = file.read(&mut head) else {
        return false;
    };
    let head = &head[..read];
    SIGNATURES.iter().any(|sig| head.starts_with(sig))
        && (!head.starts_with(b"RIFF") || head.len() == SNIFF_BYTES && &head[8..12] == b"WEBP")
}

/// The display name for a file the terminal wrote itself. Only a
/// `clipboard-*` file directly in the OS temp directory qualifies — anywhere
/// else, the name is the user's and is kept.
fn scratch_file_name(path: &Path) -> Option<String> {
    let in_temp = path.parent() == Some(std::env::temp_dir().as_path());
    let scratch = path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("clipboard-"));
    (in_temp && scratch).then(|| "clipboard.png".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR";

    #[test]
    fn prose_that_mentions_a_path_is_not_a_paste_gesture() {
        assert_eq!(pasted_image("看看 /tmp/a.png 这张图"), None);
        assert_eq!(pasted_image(""), None);
        assert_eq!(pasted_image("   "), None);
    }

    #[test]
    fn a_path_to_something_that_is_not_an_image_is_not_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.png");
        std::fs::write(&path, b"this is not a png").unwrap();
        assert_eq!(pasted_image(path.to_str().unwrap()), None);
        // A directory is not a file.
        assert_eq!(pasted_image(dir.path().to_str().unwrap()), None);
    }

    #[test]
    fn riff_alone_is_not_a_webp() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("sound.wav");
        std::fs::write(&wav, b"RIFF$\x00\x00\x00WAVEfmt ").unwrap();
        assert_eq!(pasted_image(wav.to_str().unwrap()), None);

        let webp = dir.path().join("shot.webp");
        std::fs::write(&webp, b"RIFF$\x00\x00\x00WEBPVP8 ").unwrap();
        assert!(pasted_image(webp.to_str().unwrap()).is_some());
    }

    /// The extension is not the evidence: a screenshot saved without one is
    /// still an image, and that is what the bytes say.
    #[test]
    fn content_decides_not_the_extension() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("screenshot");
        std::fs::write(&path, PNG).unwrap();
        let found = pasted_image(path.to_str().unwrap()).expect("an image by its bytes");
        assert_eq!(found.name, None, "a name the user chose is kept");
    }

    #[test]
    fn a_terminals_clipboard_scratch_file_loses_its_generated_name() {
        let temp = std::env::temp_dir();
        let path = temp.join("clipboard-2026-09-16-215212-CE7C9522.png");
        std::fs::write(&path, PNG).unwrap();
        let found = pasted_image(path.to_str().unwrap()).expect("an image");
        assert_eq!(found.name.as_deref(), Some("clipboard.png"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn quoting_is_the_shells_punctuation_not_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shot.png");
        std::fs::write(&path, PNG).unwrap();
        let raw = path.to_str().unwrap();
        for quoted in [
            format!("'{raw}'"),
            format!("\"{raw}\""),
            format!(" {raw}\n"),
        ] {
            assert_eq!(
                pasted_image(&quoted).map(|p| p.path),
                Some(raw.to_string()),
                "{quoted}"
            );
        }
    }
}
