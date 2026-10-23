//! Memory storage backend selection.

use serde::{Deserialize, Serialize};

/// Which memory implementation a project reads and writes.
///
/// `Legacy` is the shipped JSON store and stays the default until the Markdown
/// pipeline has completed migration. `Markdown` is opt-in for tests and
/// dogfood only; it never becomes the implicit choice.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryBackend {
    /// Shipped JSON store (`active/` + `archive/`).
    #[default]
    Legacy,
    /// Memory 2.0 Markdown store.
    Markdown,
}

impl MemoryBackend {
    /// The name used in configuration and diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Legacy => "legacy",
            Self::Markdown => "markdown",
        }
    }
}
