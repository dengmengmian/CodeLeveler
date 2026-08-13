//! The typed browser domain. These are the values that cross the tool boundary;
//! `serde_json::Value` is confined to the driver transport, never the domain.

use serde::{Deserialize, Serialize};

/// Identifies the isolation scope a page/ref belongs to. Refs are only valid
/// within their owning session (§10 session isolation).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BrowserSessionId(pub String);

/// Identifies one page/tab within the runtime.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BrowserPageId(pub String);

impl BrowserSessionId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl BrowserPageId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for BrowserPageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A safe handle to one element, valid **only** within the (session, page,
/// generation) it was produced in (§13). It is never resolved by guessing a
/// similar element: if `generation` no longer matches the page's current
/// generation, or the driver cannot resolve `token`, the action fails with
/// [`crate::BrowserError::RefStale`].
///
/// `token` is the driver's accessibility-ref (e.g. Playwright `aria-ref`),
/// which is bound to a specific accessibility snapshot — resolving a token from
/// a superseded snapshot fails at the driver rather than matching a lookalike.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserRef {
    pub session: BrowserSessionId,
    pub page: BrowserPageId,
    /// The snapshot generation this ref was minted in.
    pub generation: u64,
    /// Driver-side accessibility ref token (opaque to the agent).
    pub token: String,
}

impl BrowserRef {
    /// True when this ref belongs to `page` and is still on its current
    /// `generation` — the cheap proactive staleness guard before the driver is
    /// even asked.
    pub fn is_current(&self, page: &BrowserPageId, current_generation: u64) -> bool {
        &self.page == page && self.generation == current_generation
    }
}

/// State flags carried by a snapshot node. All optional — absent means the
/// attribute does not apply to that role.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserNodeState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expanded: Option<bool>,
    /// Heading/treeitem level, when applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<u32>,
}

impl BrowserNodeState {
    pub fn is_empty(&self) -> bool {
        self.checked.is_none()
            && self.selected.is_none()
            && self.disabled.is_none()
            && self.expanded.is_none()
            && self.level.is_none()
    }
}

/// One node of a semantic snapshot: an accessibility-oriented view of an
/// element, never raw DOM/HTML/CSS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserNode {
    /// ARIA role (`button`, `textbox`, `heading`, `row`, `dialog`, …).
    pub role: String,
    /// Accessible name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Current value for form controls (redacted for password fields — never
    /// the real secret).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// The interaction token, present only on actionable elements.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default, skip_serializing_if = "BrowserNodeState::is_empty")]
    pub state: BrowserNodeState,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<BrowserNode>,
}

/// A bounded, semantic view of a page (§21/§23). `truncated` is never silent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserSnapshot {
    pub page: BrowserPageId,
    pub url: String,
    pub title: String,
    /// Monotonic per-page generation; bumped on structural change/navigation.
    /// Refs carry the generation they were minted in.
    pub generation: u64,
    pub root: Vec<BrowserNode>,
    /// True when the node/char budget clipped the tree.
    pub truncated: bool,
    /// Nodes actually included after budgeting.
    pub nodes_returned: usize,
    /// Best-effort total node estimate when truncated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approximate_total: Option<usize>,
}

/// A native JS dialog surfaced to the agent (§35). CodeLeveler never lets a
/// dialog hang the driver — it is reported and the agent decides accept/dismiss.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DialogInfo {
    /// `alert` | `confirm` | `prompt` | `beforeunload`.
    pub kind: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_value: Option<String>,
}

/// The structured outcome of an interaction (click/type/select/press).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserActionResult {
    pub page: BrowserPageId,
    pub url: String,
    pub title: String,
    /// The page navigated as a result of the action.
    pub navigated: bool,
    /// A new page/tab was opened (target=_blank / window.open).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_page: Option<BrowserPageId>,
    /// A dialog opened and awaits the agent's decision.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dialog: Option<DialogInfo>,
    /// The page generation after the action (a bump signals prior refs stale).
    pub generation: u64,
}

/// One entry of the tab/page list (§34).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabInfo {
    pub page: BrowserPageId,
    pub url: String,
    pub title: String,
    pub active: bool,
}

/// Which browser engine backs the runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserEngine {
    /// System-installed Google Chrome (`channel: chrome`).
    SystemChrome,
    /// System-installed Chromium.
    SystemChromium,
    /// CodeLeveler-managed Chromium under `runtimes/browser/`.
    ManagedChromium,
}

/// Lifecycle state of the runtime (§17). `Installing` carries progress so the
/// TUI can show a managed download without a fake spinner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum BrowserRuntimeStatus {
    /// Never started; no process yet (the lazy default).
    NotReady,
    /// A managed runtime is being fetched/prepared.
    Installing {
        received_bytes: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        total_bytes: Option<u64>,
    },
    /// A driver + browser are live and usable.
    Ready,
    /// The driver/browser crashed; the next call may restart it.
    Crashed { reason: String },
}

/// Non-sensitive description of the live runtime (§44 — no executable path,
/// no profile path; those never reach the model/logs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserRuntimeInfo {
    pub engine: BrowserEngine,
    /// Browser version string (e.g. Chrome `127.0.x`).
    pub browser_version: String,
    /// Managed driver version, when a managed runtime is in use.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub driver_version: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ref_currency_requires_same_page_and_generation() {
        let r = BrowserRef {
            session: BrowserSessionId::new("s1"),
            page: BrowserPageId::new("p1"),
            generation: 5,
            token: "e12".into(),
        };
        assert!(r.is_current(&BrowserPageId::new("p1"), 5));
        assert!(
            !r.is_current(&BrowserPageId::new("p1"), 6),
            "generation bump ⇒ stale"
        );
        assert!(
            !r.is_current(&BrowserPageId::new("p2"), 5),
            "wrong page ⇒ stale"
        );
    }

    #[test]
    fn snapshot_round_trips_and_omits_empty() {
        let snap = BrowserSnapshot {
            page: BrowserPageId::new("p1"),
            url: "http://localhost:3000/users".into(),
            title: "Users".into(),
            generation: 18,
            root: vec![BrowserNode {
                role: "button".into(),
                name: Some("Create user".into()),
                value: None,
                token: Some("e15".into()),
                state: BrowserNodeState::default(),
                children: vec![],
            }],
            truncated: false,
            nodes_returned: 1,
            approximate_total: None,
        };
        let json = serde_json::to_string(&snap).unwrap();
        // Empty state/value/children/approximate_total are omitted for density.
        assert!(!json.contains("\"state\""));
        assert!(!json.contains("\"value\""));
        assert!(!json.contains("\"children\""));
        assert!(!json.contains("approximate_total"));
        let back: BrowserSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back, snap);
    }

    #[test]
    fn runtime_status_is_tagged() {
        let s = BrowserRuntimeStatus::Installing {
            received_bytes: 10,
            total_bytes: Some(100),
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"state\":\"installing\""));
        assert_eq!(
            serde_json::from_str::<BrowserRuntimeStatus>(&json).unwrap(),
            s
        );
    }
}
