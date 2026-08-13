//! Structured browser tools (§19). Thin, stateless wrappers over the
//! daemon-owned [`leveler_browser::BrowserRuntime`] held in
//! `ToolContext.services.browser`. All page/ref/generation safety lives in the
//! runtime; these tools add the tool-boundary concerns: the network/SSRF gate
//! on navigation, session scoping, and structured `ToolOutput`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_browser::{
    BrowserError, BrowserPageId, BrowserRuntime, BrowserSessionId, Interaction, WaitCondition,
};
use leveler_execution::RiskLevel;

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

// ── shared helpers ───────────────────────────────────────────────────────────

/// The runtime and this context's session scope, or a clear model-visible error
/// when the browser capability is disabled.
fn runtime(context: &ToolContext) -> Result<(&Arc<BrowserRuntime>, BrowserSessionId), ToolOutput> {
    match &context.services.browser {
        Some(rt) => Ok((rt, BrowserSessionId::new(context.session_scope()))),
        None => Err(ToolOutput::error(
            "the browser capability is not enabled in this runtime",
        )),
    }
}

fn err_out(e: BrowserError) -> ToolOutput {
    ToolOutput::error(e.to_string())
}

/// Resolve the target page: the explicit `page` argument, else the session's
/// active page (set by the last navigate/snapshot).
async fn resolve_page(
    rt: &BrowserRuntime,
    session: &BrowserSessionId,
    page: Option<String>,
) -> Result<BrowserPageId, ToolOutput> {
    match page {
        Some(p) if !p.is_empty() => Ok(BrowserPageId::new(p)),
        _ => rt.active_page(session).await.map_err(err_out),
    }
}

/// Format an action result for the model.
fn fmt_action(r: &leveler_browser::BrowserActionResult) -> String {
    let mut s = format!("page: {}\nurl: {}\ntitle: {}", r.page, r.url, r.title);
    if r.navigated {
        s.push_str("\nnavigated: true");
    }
    if let Some(np) = &r.new_page {
        s.push_str(&format!("\nnew_page: {np}"));
    }
    s.push_str("\n(call browser.snapshot to see the current refs)");
    s
}

// ── the localhost-aware network gate (§15) ──────────────────────────────────

/// Enforce the existing network policy on a navigation target. Deny when the
/// session is network-denied; otherwise allow http/https, permit loopback dev
/// URLs, and reject any other host that resolves into a blocked (private /
/// link-local / metadata / loopback) range — so the browser cannot become an
/// SSRF bypass around the runtime's own gate.
fn check_navigation(context: &ToolContext, url: &str) -> Result<(), String> {
    if context.policy.network_denied() {
        return Err("network access is denied for this run".into());
    }
    navigation_host_allowed(url)
}

/// The URL/host half of the gate (pure, no policy) — split out so the SSRF
/// logic is testable without a full `ToolContext`.
fn navigation_host_allowed(url: &str) -> Result<(), String> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| format!("unsupported URL (need http/https): {url}"))?;
    if !matches!(scheme, "http" | "https") {
        return Err(format!(
            "unsupported URL scheme `{scheme}` (need http/https)"
        ));
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(authority);
    // Strip a :port (but keep IPv6 `[..]`).
    let host = if let Some(stripped) = host.strip_prefix('[') {
        stripped.split(']').next().unwrap_or(stripped)
    } else {
        host.split(':').next().unwrap_or(host)
    };
    if host.is_empty() {
        return Err(format!("URL has no host: {url}"));
    }
    // Loopback dev servers are explicitly allowed (the §15 narrow exception).
    if matches!(host, "localhost" | "127.0.0.1" | "::1") {
        return Ok(());
    }
    // Any other host must resolve entirely to non-blocked addresses.
    use std::net::ToSocketAddrs;
    match (host, 80u16).to_socket_addrs() {
        Ok(addrs) => {
            let mut any = false;
            for addr in addrs {
                any = true;
                if crate::tools::web_fetch::is_blocked_ip(addr.ip()) {
                    return Err(format!(
                        "navigation to a blocked/private address is refused: {host}"
                    ));
                }
            }
            if !any {
                return Err(format!("could not resolve host: {host}"));
            }
            Ok(())
        }
        Err(e) => Err(format!("could not resolve host {host}: {e}")),
    }
}

// ── navigate ─────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
struct NavigateInput {
    /// Absolute http/https URL (localhost dev servers allowed).
    url: String,
}

pub struct BrowserNavigateTool;

#[async_trait]
impl Tool for BrowserNavigateTool {
    fn name(&self) -> &'static str {
        "browser_navigate"
    }
    fn description(&self) -> &'static str {
        "Open a URL in the isolated project browser (creating/reusing the current \
         page). Then use browser_snapshot to read the page."
    }
    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<NavigateInput>()
    }
    fn risk(&self) -> RiskLevel {
        RiskLevel::Network
    }
    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        _cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: NavigateInput = super::parse_input(self.name(), input)?;
        let (rt, session) = match runtime(&context) {
            Ok(v) => v,
            Err(o) => return Ok(o),
        };
        if let Err(reason) = check_navigation(&context, &input.url) {
            return Ok(ToolOutput::error(reason));
        }
        match rt.open(&session, &input.url).await {
            Ok(r) => Ok(ToolOutput::ok(fmt_action(&r))),
            Err(e) => Ok(err_out(e)),
        }
    }
}

// ── snapshot ─────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
struct PageInput {
    /// Page id (defaults to the current page).
    #[serde(default)]
    page: Option<String>,
}

pub struct BrowserSnapshotTool;

#[async_trait]
impl Tool for BrowserSnapshotTool {
    fn name(&self) -> &'static str {
        "browser_snapshot"
    }
    fn description(&self) -> &'static str {
        "Read a semantic, ref-annotated snapshot of the current page. Use the \
         [ref=…] tokens with browser_click/type/select/press."
    }
    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<PageInput>()
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
        _cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: PageInput = super::parse_input(self.name(), input)?;
        let (rt, session) = match runtime(&context) {
            Ok(v) => v,
            Err(o) => return Ok(o),
        };
        let page = match resolve_page(rt, &session, input.page).await {
            Ok(p) => p,
            Err(o) => return Ok(o),
        };
        match rt.snapshot(&session, &page).await {
            Ok(s) => {
                let mut header = format!(
                    "page: {}\nurl: {}\ntitle: {}\ngeneration: {}",
                    s.page, s.url, s.title, s.generation
                );
                if s.truncated {
                    header.push_str(&format!(
                        "\ntruncated: true (showing {} of ~{} nodes)",
                        s.nodes_returned,
                        s.approximate_total.unwrap_or(s.nodes_returned)
                    ));
                }
                Ok(ToolOutput::ok(format!("{header}\n\n{}", s.text)))
            }
            Err(e) => Ok(err_out(e)),
        }
    }
}

// ── interactions (click / type / select / press) ────────────────────────────

macro_rules! ref_input {
    ($name:ident { $($extra:tt)* }) => {
        #[derive(Debug, Deserialize, JsonSchema)]
        struct $name {
            /// A [ref=…] token from the latest browser_snapshot.
            r#ref: String,
            /// Page id (defaults to the current page).
            #[serde(default)]
            page: Option<String>,
            $($extra)*
        }
    };
}

ref_input!(ClickInput {});
ref_input!(TypeInput {
    /// Text to enter. Replaces the field's value unless `append` is true.
    text: String,
    #[serde(default)]
    append: bool,
});
ref_input!(SelectInput {
    /// Option label(s) to choose (or value(s) when `by_value`).
    values: Vec<String>,
    #[serde(default)]
    by_value: bool,
});
ref_input!(PressInput {
    /// Key to press, e.g. "Enter", "Escape", "ArrowDown".
    key: String,
});

async fn run_interaction(
    context: &ToolContext,
    page: Option<String>,
    r#ref: &str,
    action: Interaction,
) -> ToolOutput {
    let (rt, session) = match runtime(context) {
        Ok(v) => v,
        Err(o) => return o,
    };
    let page = match resolve_page(rt, &session, page).await {
        Ok(p) => p,
        Err(o) => return o,
    };
    match rt.interact(&session, &page, r#ref, action).await {
        Ok(r) => ToolOutput::ok(fmt_action(&r)),
        Err(e) => err_out(e),
    }
}

pub struct BrowserClickTool;
#[async_trait]
impl Tool for BrowserClickTool {
    fn name(&self) -> &'static str {
        "browser_click"
    }
    fn description(&self) -> &'static str {
        "Click the element identified by a [ref=…] token from browser_snapshot."
    }
    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<ClickInput>()
    }
    fn risk(&self) -> RiskLevel {
        RiskLevel::Network
    }
    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        _cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: ClickInput = super::parse_input(self.name(), input)?;
        Ok(run_interaction(&context, input.page, &input.r#ref, Interaction::Click).await)
    }
}

pub struct BrowserTypeTool;
#[async_trait]
impl Tool for BrowserTypeTool {
    fn name(&self) -> &'static str {
        "browser_type"
    }
    fn description(&self) -> &'static str {
        "Type text into a [ref=…] field. Replaces the current value unless append=true."
    }
    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<TypeInput>()
    }
    fn risk(&self) -> RiskLevel {
        RiskLevel::Network
    }
    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        _cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: TypeInput = super::parse_input(self.name(), input)?;
        Ok(run_interaction(
            &context,
            input.page,
            &input.r#ref,
            Interaction::Type {
                text: input.text,
                append: input.append,
            },
        )
        .await)
    }
}

pub struct BrowserSelectTool;
#[async_trait]
impl Tool for BrowserSelectTool {
    fn name(&self) -> &'static str {
        "browser_select"
    }
    fn description(&self) -> &'static str {
        "Select option(s) in a [ref=…] dropdown by label (or value when by_value=true)."
    }
    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<SelectInput>()
    }
    fn risk(&self) -> RiskLevel {
        RiskLevel::Network
    }
    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        _cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: SelectInput = super::parse_input(self.name(), input)?;
        Ok(run_interaction(
            &context,
            input.page,
            &input.r#ref,
            Interaction::Select {
                values: input.values,
                by_value: input.by_value,
            },
        )
        .await)
    }
}

pub struct BrowserPressTool;
#[async_trait]
impl Tool for BrowserPressTool {
    fn name(&self) -> &'static str {
        "browser_press"
    }
    fn description(&self) -> &'static str {
        "Press a key (Enter/Escape/Tab/Arrow…) while a [ref=…] element is focused."
    }
    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<PressInput>()
    }
    fn risk(&self) -> RiskLevel {
        RiskLevel::Network
    }
    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        _cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: PressInput = super::parse_input(self.name(), input)?;
        Ok(run_interaction(
            &context,
            input.page,
            &input.r#ref,
            Interaction::Press { key: input.key },
        )
        .await)
    }
}

// ── wait ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
struct WaitInput {
    #[serde(default)]
    page: Option<String>,
    /// Wait for the page load event.
    #[serde(default)]
    load: bool,
    /// Wait until the URL contains this substring.
    #[serde(default)]
    url_contains: Option<String>,
    /// Wait until this text is visible.
    #[serde(default)]
    text_visible: Option<String>,
    /// Wait until this text is gone.
    #[serde(default)]
    text_gone: Option<String>,
    /// Wait until this [ref=…] is visible.
    #[serde(default)]
    ref_visible: Option<String>,
    /// Wait until this [ref=…] is hidden.
    #[serde(default)]
    ref_hidden: Option<String>,
    /// Timeout in seconds (default 15).
    #[serde(default)]
    timeout_seconds: Option<u64>,
}

pub struct BrowserWaitTool;
#[async_trait]
impl Tool for BrowserWaitTool {
    fn name(&self) -> &'static str {
        "browser_wait"
    }
    fn description(&self) -> &'static str {
        "Wait for a structured condition (load / url / text / ref visible|hidden) \
         instead of sleeping."
    }
    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<WaitInput>()
    }
    fn risk(&self) -> RiskLevel {
        RiskLevel::Safe
    }
    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        _cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: WaitInput = super::parse_input(self.name(), input)?;
        let (rt, session) = match runtime(&context) {
            Ok(v) => v,
            Err(o) => return Ok(o),
        };
        let page = match resolve_page(rt, &session, input.page).await {
            Ok(p) => p,
            Err(o) => return Ok(o),
        };
        let condition = if input.load {
            WaitCondition::Load
        } else if let Some(u) = input.url_contains {
            WaitCondition::UrlContains(u)
        } else if let Some(t) = input.text_visible {
            WaitCondition::TextVisible(t)
        } else if let Some(t) = input.text_gone {
            WaitCondition::TextGone(t)
        } else if let Some(r) = input.ref_visible {
            WaitCondition::RefVisible(r)
        } else if let Some(r) = input.ref_hidden {
            WaitCondition::RefHidden(r)
        } else {
            return Ok(ToolOutput::error(
                "specify one wait condition (load / url_contains / text_visible / \
                 text_gone / ref_visible / ref_hidden)",
            ));
        };
        let timeout = Duration::from_secs(input.timeout_seconds.unwrap_or(15).clamp(1, 120));
        match rt.wait(&session, &page, condition, timeout).await {
            Ok(()) => Ok(ToolOutput::ok("condition met")),
            Err(e) => Ok(err_out(e)),
        }
    }
}

// ── tabs / dialog / console / screenshot ─────────────────────────────────────

pub struct BrowserTabsTool;
#[async_trait]
impl Tool for BrowserTabsTool {
    fn name(&self) -> &'static str {
        "browser_tabs"
    }
    fn description(&self) -> &'static str {
        "List the open browser pages/tabs and their urls."
    }
    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<PageInput>()
    }
    fn risk(&self) -> RiskLevel {
        RiskLevel::Safe
    }
    async fn execute(
        &self,
        _input: serde_json::Value,
        context: ToolContext,
        _cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let (rt, session) = match runtime(&context) {
            Ok(v) => v,
            Err(o) => return Ok(o),
        };
        match rt.tabs(&session).await {
            Ok(tabs) => {
                let body = tabs
                    .iter()
                    .map(|t| {
                        format!(
                            "{}{}  {}  {}",
                            if t.active { "* " } else { "  " },
                            t.page,
                            t.title,
                            t.url
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(ToolOutput::ok(if body.is_empty() {
                    "no open pages".into()
                } else {
                    body
                }))
            }
            Err(e) => Ok(err_out(e)),
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DialogInput {
    #[serde(default)]
    page: Option<String>,
    /// Answer the NEXT dialog by accepting it (default dismiss).
    #[serde(default)]
    accept: bool,
    /// Text to enter for a prompt dialog when accepting.
    #[serde(default)]
    prompt_text: Option<String>,
}

pub struct BrowserDialogTool;
#[async_trait]
impl Tool for BrowserDialogTool {
    fn name(&self) -> &'static str {
        "browser_dialog"
    }
    fn description(&self) -> &'static str {
        "Set how the NEXT native dialog (alert/confirm/prompt) is answered, before \
         the action that triggers it. Default is dismiss."
    }
    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<DialogInput>()
    }
    fn risk(&self) -> RiskLevel {
        RiskLevel::Network
    }
    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        _cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: DialogInput = super::parse_input(self.name(), input)?;
        let (rt, session) = match runtime(&context) {
            Ok(v) => v,
            Err(o) => return Ok(o),
        };
        let page = match resolve_page(rt, &session, input.page).await {
            Ok(p) => p,
            Err(o) => return Ok(o),
        };
        match rt
            .set_dialog_policy(&session, &page, input.accept, input.prompt_text)
            .await
        {
            Ok(()) => Ok(ToolOutput::ok(format!(
                "next dialog will be {}",
                if input.accept {
                    "accepted"
                } else {
                    "dismissed"
                }
            ))),
            Err(e) => Ok(err_out(e)),
        }
    }
}

pub struct BrowserConsoleTool;
#[async_trait]
impl Tool for BrowserConsoleTool {
    fn name(&self) -> &'static str {
        "browser_console"
    }
    fn description(&self) -> &'static str {
        "Read recent console errors/warnings and uncaught page errors for the page."
    }
    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<PageInput>()
    }
    fn risk(&self) -> RiskLevel {
        RiskLevel::Safe
    }
    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        _cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: PageInput = super::parse_input(self.name(), input)?;
        let (rt, session) = match runtime(&context) {
            Ok(v) => v,
            Err(o) => return Ok(o),
        };
        let page = match resolve_page(rt, &session, input.page).await {
            Ok(p) => p,
            Err(o) => return Ok(o),
        };
        match rt.console_tail(&session, &page).await {
            Ok(entries) if entries.is_empty() => {
                Ok(ToolOutput::ok("no console errors or warnings"))
            }
            Ok(entries) => Ok(ToolOutput::ok(
                entries
                    .iter()
                    .map(|e| format!("[{}] {}", e.level, e.text))
                    .collect::<Vec<_>>()
                    .join("\n"),
            )),
            Err(e) => Ok(err_out(e)),
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ScreenshotInput {
    #[serde(default)]
    page: Option<String>,
    /// Capture the full scrollable page (default: viewport only).
    #[serde(default)]
    full_page: bool,
}

pub struct BrowserScreenshotTool;
#[async_trait]
impl Tool for BrowserScreenshotTool {
    fn name(&self) -> &'static str {
        "browser_screenshot"
    }
    fn description(&self) -> &'static str {
        "Capture a PNG screenshot of the current page (auxiliary; the snapshot is \
         the primary way to read a page)."
    }
    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<ScreenshotInput>()
    }
    fn risk(&self) -> RiskLevel {
        RiskLevel::Safe
    }
    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        _cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: ScreenshotInput = super::parse_input(self.name(), input)?;
        let (rt, session) = match runtime(&context) {
            Ok(v) => v,
            Err(o) => return Ok(o),
        };
        let page = match resolve_page(rt, &session, input.page).await {
            Ok(p) => p,
            Err(o) => return Ok(o),
        };
        match rt.screenshot(&session, &page, input.full_page).await {
            Ok((b64, media_type)) => Ok(ToolOutput::ok("captured page screenshot").with_metadata(
                serde_json::json!({"image": {"media_type": media_type, "data": b64}}),
            )),
            Err(e) => Ok(err_out(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::navigation_host_allowed;

    #[test]
    fn ssrf_gate_allows_localhost_and_public_forms_but_blocks_private() {
        // Loopback dev servers are the narrow allowed exception.
        assert!(navigation_host_allowed("http://localhost:3000/users").is_ok());
        assert!(navigation_host_allowed("http://127.0.0.1/").is_ok());
        assert!(navigation_host_allowed("https://[::1]/").is_ok());

        // Private / link-local / metadata addresses are refused (IP literals,
        // so this is hermetic — no DNS needed).
        assert!(navigation_host_allowed("http://10.0.0.1/").is_err());
        assert!(navigation_host_allowed("http://192.168.1.1/").is_err());
        assert!(navigation_host_allowed("http://169.254.169.254/latest/meta-data").is_err());

        // Non-http schemes and malformed URLs are refused (no file://, no data:).
        assert!(navigation_host_allowed("file:///etc/passwd").is_err());
        assert!(navigation_host_allowed("ftp://example.com/").is_err());
        assert!(navigation_host_allowed("not-a-url").is_err());
        assert!(navigation_host_allowed("http:///no-host").is_err());
    }
}
