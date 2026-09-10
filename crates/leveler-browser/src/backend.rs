//! The one boundary between CodeLeveler and a browser protocol.
//!
//! It exists because there are two real protocols behind it — CDP and W3C
//! WebDriver — not because a browser platform was wanted. Everything a browser
//! protocol does NOT own stays on the CodeLeveler side of this line: refs,
//! generations, tab ownership, session lifecycle, cancellation, product
//! selection. What is below the line is navigation, the DOM, rendering and
//! input, which is exactly what the two protocols provide.
//!
//! Operations a protocol cannot provide return [`BrowserError::Unsupported`].
//! They are never emulated, and never quietly served by the other backend.

use async_trait::async_trait;

use crate::{BrowserResult, InspectKind, InspectReport, TabId};

/// The snapshot as the backend produced it, before CodeLeveler budgets it.
#[derive(Debug, Clone)]
pub struct RawSnapshot {
    pub url: String,
    pub title: String,
    pub text: String,
    /// Interactive/named nodes the script assigned a ref to.
    pub nodes: usize,
    /// Every reported node, ref'd or not.
    pub total: usize,
}

/// One tab as the backend sees it.
#[derive(Debug, Clone)]
pub struct RawTab {
    pub tab: TabId,
    pub url: String,
    pub title: String,
}

/// A user interaction. `r#ref` is the DOM-carried `data-leveler-ref` label,
/// already validated against the current generation by the session — a backend
/// never decides whether a ref is current.
#[derive(Debug, Clone)]
pub enum Act<'a> {
    Click {
        r#ref: &'a str,
    },
    /// Replace the field's value.
    Fill {
        r#ref: &'a str,
        text: &'a str,
    },
    /// Append to the field's value.
    Type {
        r#ref: &'a str,
        text: &'a str,
    },
    Press {
        r#ref: Option<&'a str>,
        key: &'a str,
    },
    Select {
        r#ref: &'a str,
        values: &'a [String],
    },
    Scroll {
        r#ref: Option<&'a str>,
        dx: f64,
        dy: f64,
    },
}

impl Act<'_> {
    /// The verb, for error text.
    pub fn verb(&self) -> &'static str {
        match self {
            Self::Click { .. } => "click",
            Self::Fill { .. } => "fill",
            Self::Type { .. } => "type",
            Self::Press { .. } => "press",
            Self::Select { .. } => "select",
            Self::Scroll { .. } => "scroll",
        }
    }
}

/// One live browser, driven over one protocol.
#[async_trait]
pub trait BrowserBackend: Send + Sync {
    async fn new_tab(&self) -> BrowserResult<TabId>;
    async fn close_tab(&self, tab: &TabId) -> BrowserResult<()>;
    async fn list_tabs(&self) -> BrowserResult<Vec<RawTab>>;

    async fn navigate(&self, tab: &TabId, url: &str) -> BrowserResult<()>;
    async fn reload(&self, tab: &TabId) -> BrowserResult<()>;
    async fn locate(&self, tab: &TabId) -> BrowserResult<(String, String)>;

    /// Run the shared snapshot script, stamping `generation` into every ref.
    async fn snapshot(&self, tab: &TabId, generation: u64) -> BrowserResult<RawSnapshot>;

    async fn act(&self, tab: &TabId, act: &Act<'_>) -> BrowserResult<()>;
    async fn screenshot(&self, tab: &TabId) -> BrowserResult<String>;
    async fn inspect(&self, tab: &TabId, kind: InspectKind) -> BrowserResult<InspectReport>;

    /// True while the protocol connection is still answering.
    async fn is_live(&self) -> bool;

    /// Close the browser and reap its process tree. Safe to call twice.
    async fn shutdown(&self);
}

/// Choosing an option in a `<select>`, as one body both backends wrap.
///
/// Neither protocol has a "choose this option" command, so this IS the
/// operation rather than a stand-in for one: it sets the selection and fires
/// the events a real choice fires. `el` and `values` are bound by the wrapper.
pub const SELECT_BODY: &str = "\
  if (el.tagName !== 'SELECT') return {ok:false, why:'not a <select>'};\
  var wanted = values.map(function(v){ return String(v); });\
  var hit = 0;\
  for (var i = 0; i < el.options.length; i++) {\
    var o = el.options[i];\
    var m = wanted.indexOf(o.label) >= 0 || wanted.indexOf(o.text.trim()) >= 0 || wanted.indexOf(o.value) >= 0;\
    o.selected = m; if (m) hit++;\
  }\
  if (!hit) return {ok:false, why:'no option matched'};\
  el.dispatchEvent(new Event('input', {bubbles:true}));\
  el.dispatchEvent(new Event('change', {bubbles:true}));\
  return {ok:true};";

/// A CSS string literal, so a ref can never break out of the attribute
/// selector it is embedded in. The session already refuses any ref that is not
/// `<generation>e<n>`; this is the second, independent layer.
pub fn css_string_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// The shared snapshot script, with `GENERATION` bound.
pub fn snapshot_script(generation: u64) -> String {
    include_str!("js/snapshot.js").replace("GENERATION", &generation.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_snapshot_script_is_the_same_source_for_both_backends() {
        let s = snapshot_script(7);
        assert!(s.contains("data-leveler-ref"));
        assert!(
            s.trim_end().ends_with("})(7)"),
            "generation must be bound: {}",
            &s[s.len() - 40..]
        );
        assert!(!s.contains("GENERATION"), "no placeholder may survive");
    }
}
