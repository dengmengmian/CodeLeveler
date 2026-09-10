//! The typed browser domain. These are the values that cross the tool
//! boundary; `serde_json::Value` is confined to the two protocol adapters.

use serde::{Deserialize, Serialize};

/// A browser PRODUCT — what the user actually runs.
///
/// Deliberately separate from the protocol that drives it: "Chromium backend"
/// is a protocol, not a browser. A user whose default browser is Edge must be
/// driven through Edge, never through Chrome merely because both speak CDP.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserProduct {
    Safari,
    Chrome,
    Edge,
    Chromium,
}

impl BrowserProduct {
    /// The stable id used in config, tool input and diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Safari => "safari",
            Self::Chrome => "chrome",
            Self::Edge => "edge",
            Self::Chromium => "chromium",
        }
    }

    /// Parse a config/tool value. Unknown values are `None` — never coerced to
    /// a default, because a typo must not silently pick a different browser.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "safari" => Some(Self::Safari),
            "chrome" | "google chrome" | "google-chrome" => Some(Self::Chrome),
            "edge" | "msedge" | "microsoft edge" => Some(Self::Edge),
            "chromium" => Some(Self::Chromium),
            _ => None,
        }
    }

    /// Every product, for exhaustive discovery/diagnostics.
    pub const ALL: [Self; 4] = [Self::Safari, Self::Chrome, Self::Edge, Self::Chromium];
}

impl std::fmt::Display for BrowserProduct {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where a resolved product came from. Carried into diagnostics so an
/// unavailable browser can say WHY that browser was the one chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProductSource {
    /// The tool call named it.
    Explicit,
    /// `[browser].default` in the global config.
    Configured,
    /// The operating system's default web browser.
    SystemDefault,
}

impl ProductSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Explicit => "explicitly requested",
            Self::Configured => "configured as [browser].default",
            Self::SystemDefault => "the system default browser",
        }
    }
}

/// A product plus how it was chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectedProduct {
    pub product: BrowserProduct,
    pub source: ProductSource,
}

/// Identifies the isolation scope a tab/ref belongs to. Refs and tabs are only
/// valid within their owning session (§19).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BrowserSessionId(pub String);

/// Identifies one tab within the live browser.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TabId(pub String);

impl BrowserSessionId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TabId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for TabId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A bounded, semantic view of a page (§17).
///
/// `text` is role + accessible name + `[ref]` tokens, never raw DOM/HTML. The
/// same renderer serves both backends, so a model reads one format whichever
/// browser is driving. `truncated` is never silent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserSnapshot {
    pub tab: TabId,
    pub url: String,
    pub title: String,
    /// Monotonic per-tab generation. Model-facing refs embed it, so a ref from
    /// a superseded snapshot is mechanically stale rather than retargeted.
    pub generation: u64,
    pub text: String,
    pub truncated: bool,
    pub nodes_returned: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approximate_total: Option<usize>,
}

/// The structured outcome of an act/navigate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionOutcome {
    pub tab: TabId,
    pub url: String,
    pub title: String,
    /// The page navigated as a result of the action.
    pub navigated: bool,
    /// A new tab was opened (target=_blank / window.open) and adopted by this
    /// session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_tab: Option<TabId>,
}

/// One entry of the tab list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabInfo {
    pub tab: TabId,
    pub url: String,
    pub title: String,
    pub active: bool,
}

/// What `browser_inspect` is asking the backend for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InspectKind {
    Console,
    PageErrors,
    Network,
}

impl InspectKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Console => "console",
            Self::PageErrors => "page_errors",
            Self::Network => "network",
        }
    }
}

/// One console message / page error surfaced to the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsoleEntry {
    /// `error` | `warning` | `pageerror`.
    pub level: String,
    pub text: String,
}

/// One network request/response pair the backend observed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkEntry {
    pub method: String,
    pub url: String,
    /// `None` when the request failed before a response.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u32>,
    /// Set when the request failed; carries the browser's own error text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
}

/// What an inspect returned.
///
/// A backend that cannot observe the asked-for channel returns
/// [`crate::BrowserError::Unsupported`] instead of an empty variant, so
/// "nothing happened" and "this browser cannot see it" are never the same
/// answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InspectReport {
    Console(Vec<ConsoleEntry>),
    Network(Vec<NetworkEntry>),
}

/// Lifecycle state of the browser capability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum BrowserStatus {
    /// Nothing started; no process (the lazy default).
    NotStarted,
    /// A browser is live and usable.
    Live { product: BrowserProduct },
    /// The protocol connection dropped. The next navigate starts a fresh
    /// session; every other operation reports this state (§24).
    Disconnected {
        product: BrowserProduct,
        reason: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Three products share CDP, and sharing a protocol does not make them the
    /// same browser. `discover` proves the install paths stay separate; this
    /// pins the values themselves.
    #[test]
    fn a_product_is_not_its_protocol() {
        assert_ne!(BrowserProduct::Edge, BrowserProduct::Chrome);
        assert_ne!(BrowserProduct::Chromium, BrowserProduct::Chrome);
        let ids: Vec<&str> = BrowserProduct::ALL.iter().map(|p| p.as_str()).collect();
        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(
            ids.len(),
            unique.len(),
            "each product has its own id: {ids:?}"
        );
    }

    #[test]
    fn parse_round_trips_and_refuses_unknown() {
        for p in BrowserProduct::ALL {
            assert_eq!(BrowserProduct::parse(p.as_str()), Some(p));
        }
        assert_eq!(
            BrowserProduct::parse("Microsoft Edge"),
            Some(BrowserProduct::Edge)
        );
        assert_eq!(
            BrowserProduct::parse("Google Chrome"),
            Some(BrowserProduct::Chrome)
        );
        // A typo must not silently become a different browser.
        assert_eq!(BrowserProduct::parse("chrom"), None);
        assert_eq!(BrowserProduct::parse("firefox"), None);
        assert_eq!(BrowserProduct::parse(""), None);
    }

    #[test]
    fn snapshot_round_trips_and_omits_empty() {
        let snap = BrowserSnapshot {
            tab: TabId::new("tab-1"),
            url: "http://localhost:3000/users".into(),
            title: "Users".into(),
            generation: 18,
            text: "[18e15] button \"Create user\"".into(),
            truncated: false,
            nodes_returned: 1,
            approximate_total: None,
        };
        let json = serde_json::to_string(&snap).unwrap();
        assert!(!json.contains("approximate_total"));
        assert_eq!(
            serde_json::from_str::<BrowserSnapshot>(&json).unwrap(),
            snap
        );
    }
}
