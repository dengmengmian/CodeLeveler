//! Attachment projections for the UI (spec §39).
//!
//! The UI and client exchange lightweight references — never image bytes. The
//! processed image lives in the media store, keyed by `sha256`; the client loads
//! and base64-encodes it only at the request boundary (spec §45).

use serde::{Deserialize, Serialize};

/// Identifies a pending or sent attachment.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AttachmentId(String);

impl AttachmentId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AttachmentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The kind of attachment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum AttachmentKind {
    Image,
    TextFile,
    Document,
    Unknown,
}

/// A reference to a processed, stored attachment (spec §39). Carries only
/// metadata; the bytes are addressed by `sha256` in the media store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AttachmentRef {
    pub id: AttachmentId,
    pub kind: AttachmentKind,
    pub name: String,
    pub mime_type: String,
    pub size_bytes: u64,
    /// Content-address of the processed bytes in the media store.
    pub sha256: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Original file the user attached. Clipboard and pasted bytes have none.
    /// The composer preview is the only surface that shows it; the sentence
    /// the model reads stays `[图片 #N]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
    /// Local processed PNG (`{sha256}.png` under the media store) a terminal
    /// may read for a pixel preview. Never shown and never sent as text.
    /// Absent for clients that do not have the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_path: Option<String>,
}

impl AttachmentRef {
    /// A compact one-line label, e.g. `error.png · PNG · 1280×720 · 182 KB`.
    pub fn summary(&self) -> String {
        let dims = match (self.width, self.height) {
            (Some(w), Some(h)) => format!(" · {w}×{h}"),
            _ => String::new(),
        };
        format!(
            "{} · {}{dims} · {}",
            self.name,
            self.mime_type
                .rsplit('/')
                .next()
                .unwrap_or(&self.mime_type)
                .to_uppercase(),
            human_size(self.size_bytes),
        )
    }
}

fn human_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{} KB", bytes / 1024)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attachment(size: u64, width: Option<u32>, height: Option<u32>) -> AttachmentRef {
        AttachmentRef {
            id: AttachmentId::new("a1"),
            kind: AttachmentKind::Image,
            name: "preview.png".to_string(),
            mime_type: "image/png".to_string(),
            size_bytes: size,
            sha256: "abc".to_string(),
            width,
            height,
            source_path: None,
            preview_path: None,
        }
    }

    #[test]
    fn paths_are_optional_and_stay_out_of_the_summary() {
        let mut with_paths = attachment(186_368, Some(1280), Some(720));
        with_paths.source_path = Some("/Users/secret/shot.png".into());
        with_paths.preview_path = Some("/tmp/state/media/abc.png".into());
        let summary = with_paths.summary();
        assert_eq!(summary, "preview.png · PNG · 1280×720 · 182 KB");
        assert!(!summary.contains("secret"));
        assert!(!summary.contains("media"));

        let old: AttachmentRef = serde_json::from_str(
            r#"{"id":"a1","kind":"image","name":"preview.png","mime_type":"image/png","size_bytes":512,"sha256":"abc"}"#,
        )
        .unwrap();
        assert_eq!(old.source_path, None);
        assert_eq!(old.preview_path, None);
        assert_eq!(old.summary(), "preview.png · PNG · 512 B");
    }

    #[test]
    fn summary_includes_name_kind_size_and_dimensions() {
        let a = attachment(186_368, Some(1280), Some(720));
        assert_eq!(a.summary(), "preview.png · PNG · 1280×720 · 182 KB");
    }

    #[test]
    fn summary_omits_dimensions_when_missing() {
        let a = attachment(512, None, None);
        assert_eq!(a.summary(), "preview.png · PNG · 512 B");
    }

    #[test]
    fn human_size_converts_bytes_to_mb() {
        assert_eq!(human_size(2 * 1024 * 1024), "2.0 MB");
    }

    #[test]
    fn human_size_converts_bytes_to_kb() {
        assert_eq!(human_size(1536), "1 KB");
    }

    #[test]
    fn human_size_leaves_small_values_as_bytes() {
        assert_eq!(human_size(512), "512 B");
    }
}
