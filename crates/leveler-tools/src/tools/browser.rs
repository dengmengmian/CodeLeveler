//! The three browser tools.
//!
//! One tool per job, and the jobs are genuinely different: `browser_tab` moves
//! between pages and reads them, `browser_act` behaves like a user inside one,
//! `browser_inspect` reads what the browser observed. Twelve narrow tools were
//! collapsed into these three; none of the three is an `action=`-shaped
//! catch-all for the other two.
//!
//! The tools are thin. Refs, generations, tab ownership, product selection,
//! process lifetime and the disconnect lifecycle all live in
//! [`leveler_browser::Browser`]; what is added here is the tool boundary —
//! input parsing, cancellation, and a structured `ToolOutput`.
//!
//! **The browser is a network-authorised capability.** Exposing it IS the
//! authorisation, so there is no per-navigation network gate here: localhost,
//! LAN dev servers and the public internet are all reachable, and a click that
//! navigates, a page's `fetch`, a WebSocket and a subresource all behave the
//! way they do in the user's own browser.

use std::sync::Arc;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_browser::{
    Act, Browser, BrowserError, BrowserProduct, BrowserSessionId, InspectKind, InspectReport, TabId,
};
use leveler_execution::RiskLevel;

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

/// Each tool holds the browser handle it was constructed with, and nothing
/// else. Written out three times rather than generated: at this size a macro
/// hides more than it saves.
pub struct BrowserTabTool {
    browser: Arc<Browser>,
}

pub struct BrowserActTool {
    browser: Arc<Browser>,
}

pub struct BrowserInspectTool {
    browser: Arc<Browser>,
}

impl BrowserTabTool {
    pub fn new(browser: Arc<Browser>) -> Self {
        Self { browser }
    }
}

impl BrowserActTool {
    pub fn new(browser: Arc<Browser>) -> Self {
        Self { browser }
    }
}

impl BrowserInspectTool {
    pub fn new(browser: Arc<Browser>) -> Self {
        Self { browser }
    }
}

fn scope(context: &ToolContext) -> BrowserSessionId {
    BrowserSessionId::new(context.session_scope())
}

fn target(tab: Option<String>) -> Option<TabId> {
    tab.filter(|t| !t.is_empty()).map(TabId::new)
}

fn failed(e: BrowserError) -> ToolOutput {
    ToolOutput::error(e.to_string())
}

/// Run one browser operation, ending it the moment the turn is cancelled.
///
/// Dropping the operation's future abandons the in-flight protocol call; the
/// browser session itself is untouched, so a cancelled click does not cost the
/// model its page, its cookies or its tabs.
macro_rules! cancellable {
    ($cancel:expr, $op:expr) => {
        tokio::select! {
            biased;
            _ = $cancel.cancelled() => return Ok(ToolOutput::error("browser operation cancelled")),
            r = $op => r,
        }
    };
}

/// How an action result is reported. Deliberately flat facts: what the tab is,
/// where it is, whether it moved. No suggestion about what to do next (§33).
fn describe(outcome: &leveler_browser::ActionOutcome) -> String {
    let mut s = format!(
        "tab: {}\nurl: {}\ntitle: {}",
        outcome.tab, outcome.url, outcome.title
    );
    if outcome.navigated {
        s.push_str("\nnavigated: true");
    }
    if let Some(new) = &outcome.new_tab {
        s.push_str(&format!("\nnew_tab: {new}"));
    }
    s
}

// ── browser_tab ─────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum TabAction {
    Navigate,
    Snapshot,
    Screenshot,
    Reload,
    ListTabs,
    NewTab,
    SelectTab,
    CloseTab,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct TabInput {
    /// What to do with the page or tab.
    action: TabAction,
    /// Absolute http/https URL. Required by `navigate`.
    #[serde(default)]
    url: Option<String>,
    /// Tab id from a previous result (defaults to the current tab).
    #[serde(default)]
    tab: Option<String>,
    /// Which browser to drive: `safari`, `chrome`, `edge` or `chromium`.
    /// Omit to use the configured or system default browser. Naming a
    /// different browser than the one already running closes that one and
    /// starts this one, so its tabs and login state do not carry over.
    #[serde(default)]
    browser: Option<String>,
}

#[async_trait]
impl Tool for BrowserTabTool {
    fn name(&self) -> &'static str {
        "browser_tab"
    }

    fn description(&self) -> &'static str {
        "Drive pages and tabs in a real browser: navigate to a URL, read a \
         semantic snapshot of the current page, capture a screenshot, reload, \
         and list/open/select/close tabs. The snapshot's [ref] tokens are what \
         browser_act operates on. Uses your default browser unless `browser` \
         names one."
    }

    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<TabInput>()
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Network
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: TabInput = super::parse_input(self.name(), input)?;
        let session = scope(&context);
        let b = &self.browser;
        let tab = target(input.tab);

        let product = match input
            .browser
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            None => None,
            Some(name) => match BrowserProduct::parse(name) {
                Some(p) => Some(p),
                None => {
                    return Ok(ToolOutput::error(format!(
                        "`{name}` is not a browser CodeLeveler drives (safari, chrome, edge, chromium)"
                    )));
                }
            },
        };

        Ok(match input.action {
            TabAction::Navigate => {
                let Some(url) = input.url.as_deref().filter(|u| !u.trim().is_empty()) else {
                    return Ok(ToolOutput::error("browser_tab navigate needs a `url`"));
                };
                match cancellable!(cancellation, b.navigate(&session, product, url.trim())) {
                    Ok(o) => ToolOutput::ok(describe(&o)),
                    Err(e) => failed(e),
                }
            }
            TabAction::Snapshot => match cancellable!(cancellation, b.snapshot(&session, tab)) {
                Ok(s) => {
                    let mut header = format!(
                        "tab: {}\nurl: {}\ntitle: {}\ngeneration: {}",
                        s.tab, s.url, s.title, s.generation
                    );
                    if s.truncated {
                        header.push_str(&format!(
                            "\ntruncated: true (showing {} of ~{} nodes)",
                            s.nodes_returned,
                            s.approximate_total.unwrap_or(s.nodes_returned)
                        ));
                    }
                    ToolOutput::ok(format!("{header}\n\n{}", s.text))
                }
                Err(e) => failed(e),
            },
            TabAction::Screenshot => {
                match cancellable!(cancellation, b.screenshot(&session, tab)) {
                    Ok(data) => ToolOutput::ok("captured page screenshot").with_metadata(
                        serde_json::json!({"image": {"media_type": "image/png", "data": data}}),
                    ),
                    Err(e) => failed(e),
                }
            }
            TabAction::Reload => match cancellable!(cancellation, b.reload(&session)) {
                Ok(o) => ToolOutput::ok(describe(&o)),
                Err(e) => failed(e),
            },
            TabAction::ListTabs => match cancellable!(cancellation, b.tabs(&session)) {
                Ok(tabs) if tabs.is_empty() => ToolOutput::ok("no open tabs"),
                Ok(tabs) => ToolOutput::ok(
                    tabs.iter()
                        .map(|t| {
                            format!(
                                "{}{}  {}  {}",
                                if t.active { "* " } else { "  " },
                                t.tab,
                                t.title,
                                t.url
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                Err(e) => failed(e),
            },
            TabAction::NewTab => match cancellable!(cancellation, b.new_tab(&session)) {
                Ok(t) => ToolOutput::ok(format!("tab: {t}\nurl: about:blank")),
                Err(e) => failed(e),
            },
            TabAction::SelectTab => {
                let Some(t) = tab else {
                    return Ok(ToolOutput::error("browser_tab select_tab needs a `tab`"));
                };
                match cancellable!(cancellation, b.select_tab(&session, &t)) {
                    Ok(o) => ToolOutput::ok(describe(&o)),
                    Err(e) => failed(e),
                }
            }
            TabAction::CloseTab => {
                let Some(t) = tab else {
                    return Ok(ToolOutput::error("browser_tab close_tab needs a `tab`"));
                };
                match cancellable!(cancellation, b.close_tab(&session, &t)) {
                    Ok(()) => ToolOutput::ok(format!("closed {t}")),
                    Err(e) => failed(e),
                }
            }
        })
    }
}

// ── browser_act ─────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum ActAction {
    Click,
    Fill,
    Type,
    Press,
    Select,
    Scroll,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ActInput {
    /// The interaction to perform.
    action: ActAction,
    /// A [ref] token from the latest browser_tab snapshot. Required by
    /// click/fill/type/select; optional for press and scroll.
    #[serde(default, rename = "ref")]
    element: Option<String>,
    /// Text to enter. `fill` replaces the field's value; `type` appends.
    #[serde(default)]
    text: Option<String>,
    /// Key to press, e.g. "Enter", "Escape", "Tab", "ArrowDown".
    #[serde(default)]
    key: Option<String>,
    /// Option label(s) or value(s) to choose in a dropdown.
    #[serde(default)]
    values: Option<Vec<String>>,
    /// Horizontal scroll distance in pixels.
    #[serde(default)]
    dx: Option<f64>,
    /// Vertical scroll distance in pixels.
    #[serde(default)]
    dy: Option<f64>,
    /// Tab id (defaults to the current tab).
    #[serde(default)]
    tab: Option<String>,
}

#[async_trait]
impl Tool for BrowserActTool {
    fn name(&self) -> &'static str {
        "browser_act"
    }

    fn description(&self) -> &'static str {
        "Act on the current page as a user would: click, fill or type into a \
         field, press a key, choose from a dropdown, or scroll. Elements are \
         addressed by the [ref] tokens from the latest browser_tab snapshot; a \
         ref from an older snapshot is refused as stale."
    }

    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<ActInput>()
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Network
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: ActInput = super::parse_input(self.name(), input)?;
        let session = scope(&context);
        let tab = target(input.tab);
        let element = input.element.as_deref().filter(|r| !r.is_empty());
        let values = input.values.unwrap_or_default();

        let needs_ref = |what: &str| {
            ToolOutput::error(format!("browser_act {what} needs a `ref` from a snapshot"))
        };
        let act = match input.action {
            ActAction::Click => match element {
                Some(r) => Act::Click { r#ref: r },
                None => return Ok(needs_ref("click")),
            },
            ActAction::Fill | ActAction::Type => {
                let Some(r) = element else {
                    return Ok(needs_ref("fill/type"));
                };
                let Some(text) = input.text.as_deref() else {
                    return Ok(ToolOutput::error("browser_act fill/type needs `text`"));
                };
                if matches!(input.action, ActAction::Fill) {
                    Act::Fill { r#ref: r, text }
                } else {
                    Act::Type { r#ref: r, text }
                }
            }
            ActAction::Press => {
                let Some(key) = input.key.as_deref().filter(|k| !k.is_empty()) else {
                    return Ok(ToolOutput::error("browser_act press needs a `key`"));
                };
                Act::Press {
                    r#ref: element,
                    key,
                }
            }
            ActAction::Select => {
                let Some(r) = element else {
                    return Ok(needs_ref("select"));
                };
                if values.is_empty() {
                    return Ok(ToolOutput::error("browser_act select needs `values`"));
                }
                Act::Select {
                    r#ref: r,
                    values: &values,
                }
            }
            ActAction::Scroll => Act::Scroll {
                r#ref: element,
                dx: input.dx.unwrap_or(0.0),
                dy: input.dy.unwrap_or(0.0),
            },
        };

        Ok(
            match cancellable!(cancellation, self.browser.act(&session, tab, act)) {
                Ok(o) => ToolOutput::ok(describe(&o)),
                Err(e) => failed(e),
            },
        )
    }
}

// ── browser_inspect ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum InspectWhat {
    Console,
    PageErrors,
    Network,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct InspectInput {
    /// Which channel to read.
    what: InspectWhat,
    /// Tab id (defaults to the current tab).
    #[serde(default)]
    tab: Option<String>,
}

#[async_trait]
impl Tool for BrowserInspectTool {
    fn name(&self) -> &'static str {
        "browser_inspect"
    }

    fn description(&self) -> &'static str {
        "Read what the browser observed on the current page: console \
         errors/warnings, uncaught page errors, or the network requests it \
         made and how they finished. Availability depends on the browser: \
         Safari's automation protocol has no such channels and says so."
    }

    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<InspectInput>()
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Safe
    }

    fn supports_parallel(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: InspectInput = super::parse_input(self.name(), input)?;
        let session = scope(&context);
        let tab = target(input.tab);
        let kind = match input.what {
            InspectWhat::Console => InspectKind::Console,
            InspectWhat::PageErrors => InspectKind::PageErrors,
            InspectWhat::Network => InspectKind::Network,
        };

        Ok(
            match cancellable!(cancellation, self.browser.inspect(&session, tab, kind)) {
                Ok(InspectReport::Console(entries)) if entries.is_empty() => {
                    ToolOutput::ok(format!("no {} entries", kind.as_str()))
                }
                Ok(InspectReport::Console(entries)) => ToolOutput::ok(
                    entries
                        .iter()
                        .map(|e| format!("[{}] {}", e.level, e.text))
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                Ok(InspectReport::Network(rows)) if rows.is_empty() => {
                    ToolOutput::ok("no network requests recorded")
                }
                Ok(InspectReport::Network(rows)) => ToolOutput::ok(
                    rows.iter()
                        .map(|r| {
                            let outcome = match (&r.status, &r.failure) {
                                (_, Some(f)) => format!("FAILED {f}"),
                                (Some(s), None) => s.to_string(),
                                (None, None) => "pending".into(),
                            };
                            format!("{:>6}  {}  {}", outcome, r.method, r.url)
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                Err(e) => failed(e),
            },
        )
    }
}
