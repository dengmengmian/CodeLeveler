//! Compact presentation for local files pasted into the composer.
//!
//! Unlike images, these files are not uploaded. The composer only replaces an
//! existing absolute path with a short token and restores the exact pasted
//! text when the message is submitted.

use std::ops::Range;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PastedFile {
    pub(crate) range: Range<usize>,
    pub(crate) name: String,
}

pub(crate) fn pasted_files(text: &str) -> Vec<PastedFile> {
    token_ranges(text)
        .into_iter()
        .filter_map(|range| {
            let candidate = decode_shell_token(&text[range.clone()])?;
            let path = Path::new(&candidate);
            if !path.is_absolute() || !path.is_file() {
                return None;
            }
            if path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| {
                    matches!(
                        extension.to_ascii_lowercase().as_str(),
                        "png" | "jpg" | "jpeg" | "gif" | "webp"
                    )
                })
            {
                return None;
            }
            // A standalone pasted image is handled by the real attachment
            // path before this presentation-only pass. Do not downgrade an
            // image embedded in prose into a file reference either.
            if super::paste_image::pasted_image(&candidate).is_some() {
                return None;
            }
            let name = path.file_name()?.to_string_lossy().into_owned();
            Some(PastedFile { range, name })
        })
        .collect()
}

fn token_ranges(text: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut start = None;
    let mut quote = None;
    let mut escaped = false;
    for (index, ch) in text.char_indices() {
        if start.is_none() {
            if ch.is_whitespace() {
                continue;
            }
            start = Some(index);
        }
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' && quote != Some('\'') {
            escaped = true;
            continue;
        }
        match quote {
            Some(mark) if ch == mark => quote = None,
            Some(_) => {}
            None if matches!(ch, '\'' | '"') => quote = Some(ch),
            None if ch.is_whitespace() => {
                ranges.push(start.take().unwrap()..index);
            }
            None => {}
        }
    }
    if let Some(start) = start {
        ranges.push(start..text.len());
    }
    ranges
}

fn decode_shell_token(raw: &str) -> Option<String> {
    let raw = match (raw.chars().next(), raw.chars().last()) {
        (Some(first @ ('\'' | '"')), Some(last)) if first == last && raw.len() >= 2 => {
            &raw[first.len_utf8()..raw.len() - last.len_utf8()]
        }
        _ => raw,
    };
    let mut decoded = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            decoded.push(chars.next()?);
        } else {
            decoded.push(ch);
        }
    }
    Some(decoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoted_and_escaped_paths_are_recognised() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a file.xlsx");
        std::fs::write(&path, b"sheet").unwrap();
        let raw = path.to_string_lossy();
        let quoted = format!("'{raw}' read it");
        let found = pasted_files(&quoted);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "a file.xlsx");
    }
}
