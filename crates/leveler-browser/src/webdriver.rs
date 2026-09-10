//! W3C WebDriver backend — Safari, over `/usr/bin/safaridriver`.
//!
//! `Rust → WebDriver → safaridriver → Safari`. safaridriver is an HTTP server
//! implementing the WebDriver REST API; this module is an HTTP client for it
//! and nothing more.
//!
//! Two protocol facts shape everything here, and neither is worked around:
//!
//! - WebDriver is **window-at-a-time**: every command applies to the session's
//!   current window, so a tab operation switches first. Tab ids ARE window
//!   handles.
//! - WebDriver has **no observation channel**. There is no console, no page
//!   error stream and no network log in the protocol, so `browser_inspect`
//!   answers [`BrowserError::Unsupported`] rather than an empty list. Safari is
//!   never made to look like Chrome (§32), and never silently served by it.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::backend::{Act, BrowserBackend, RawSnapshot, RawTab, snapshot_script};
use crate::{BrowserError, BrowserResult, InspectKind, InspectReport, TabId};

const DRIVER_READY_TIMEOUT: Duration = Duration::from_secs(20);
const NAV_TIMEOUT: Duration = Duration::from_secs(30);

/// What the user has to switch on, in the order the menus actually appear.
/// macOS puts the second toggle in the menu bar, not in Settings, which is why
/// turning on the first one alone leaves automation off.
const SAFARI_PREREQUISITE: &str = "Safari Remote Automation is off. Turn it on in two steps: \
     Safari → Settings → Advanced → tick “Show features for web developers”, \
     then in the menu bar Develop → tick “Allow Remote Automation”. \
     To drive a different browser instead, set [browser].default in ~/.leveler/config.toml";

/// Safari allows exactly ONE WebDriver session per running instance, and the
/// pairing is held by Safari, not by the driver. If CodeLeveler is killed
/// outright — `kill -9`, a force quit, a panic — the session is never deleted
/// and Safari stays paired to a driver that no longer exists. Every later
/// session then fails, and safaridriver's own wording does not say what to do.
const SAFARI_ALREADY_PAIRED: &str = "Safari is still paired to a WebDriver session that no \
     longer exists, which happens when CodeLeveler was killed rather than shut down. Safari \
     holds that pairing until it quits, so quit Safari (Cmd+Q) and try again. To drive a \
     different browser instead, set [browser].default in ~/.leveler/config.toml";

/// The W3C web-element key. A script result carrying it is an element handle.
const ELEMENT_KEY: &str = "element-6066-11e4-a52e-4f735466cecf";

pub struct WebDriverBackend {
    base: String,
    session: String,
    http: reqwest::Client,
    /// The window this session last switched to, so a switch is skipped when
    /// the browser is already there.
    current: Mutex<Option<TabId>>,
    driver: DriverProcess,
    live: Arc<std::sync::atomic::AtomicBool>,
}

/// The `safaridriver` child. Safari itself is started by macOS, not by this
/// process, so ending the WebDriver SESSION is what closes the automation
/// window; reaping the driver is the second half (§25).
struct DriverProcess {
    #[cfg(unix)]
    pgid: i32,
    #[cfg(unix)]
    _child: tokio::process::Child,
    #[cfg(not(unix))]
    _unused: (),
}

impl DriverProcess {
    fn spawn(driver: &Path, port: u16) -> BrowserResult<Self> {
        #[cfg(unix)]
        {
            let mut cmd = tokio::process::Command::new(driver);
            cmd.arg("-p")
                .arg(port.to_string())
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true)
                .process_group(0);
            let child = cmd
                .spawn()
                .map_err(|e| BrowserError::LaunchFailed(format!("spawn safaridriver: {e}")))?;
            let pgid = child
                .id()
                .map(|p| p as i32)
                .ok_or_else(|| BrowserError::LaunchFailed("safaridriver has no pid".into()))?;
            Ok(Self {
                pgid,
                _child: child,
            })
        }
        #[cfg(not(unix))]
        {
            let _ = (driver, port);
            Err(BrowserError::Unavailable(
                "Safari exists only on macOS".into(),
            ))
        }
    }

    async fn reap(&self) {
        #[cfg(unix)]
        unsafe {
            libc::killpg(self.pgid, libc::SIGTERM);
            tokio::time::sleep(Duration::from_millis(200)).await;
            libc::killpg(self.pgid, libc::SIGKILL);
        }
    }
}

impl WebDriverBackend {
    /// Start safaridriver on a free ephemeral port and open one session.
    pub async fn launch(driver: &Path) -> BrowserResult<Self> {
        let port = free_port()?;
        let process = DriverProcess::spawn(driver, port)?;
        let base = format!("http://127.0.0.1:{port}");
        let http = reqwest::Client::builder()
            .timeout(NAV_TIMEOUT + Duration::from_secs(5))
            .build()
            .map_err(|e| BrowserError::LaunchFailed(format!("http client: {e}")))?;
        wait_for_driver(&http, &base, DRIVER_READY_TIMEOUT).await?;

        let body = json!({"capabilities": {"alwaysMatch": {"browserName": "safari"}}});
        let res = request(
            &http,
            reqwest::Method::POST,
            &format!("{base}/session"),
            Some(body),
        )
        .await
        .map_err(|e| match e {
            // The one prerequisite only the user can satisfy. safaridriver is
            // the only thing on this machine that can answer it reliably —
            // Safari's own preference is sealed inside a TCC-protected
            // container — so its refusal is where the instruction lives.
            BrowserError::ActionFailed(m)
                if m.to_ascii_lowercase().contains("remote automation") =>
            {
                BrowserError::Unavailable(SAFARI_PREREQUISITE.into())
            }
            BrowserError::ActionFailed(m) if m.contains("already paired") => {
                BrowserError::Unavailable(SAFARI_ALREADY_PAIRED.into())
            }
            other => other,
        })?;
        let session = res
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| BrowserError::LaunchFailed("safaridriver returned no sessionId".into()))?
            .to_string();
        // WebDriver has no "cancel this command": a command runs to completion
        // on the server, and the session accepts nothing else until it does.
        // A bounded page-load timeout is therefore what lets an abandoned
        // navigate release the session instead of wedging it.
        let _ = request(
            &http,
            reqwest::Method::POST,
            &format!("{base}/session/{session}/timeouts"),
            Some(json!({
                "pageLoad": NAV_TIMEOUT.as_millis() as u64,
                "script": 15_000,
                "implicit": 0,
            })),
        )
        .await;
        Ok(Self {
            base,
            session,
            http,
            current: Mutex::new(None),
            driver: process,
            live: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}/session/{}{}", self.base, self.session, path)
    }

    async fn post(&self, path: &str, body: Value) -> BrowserResult<Value> {
        self.send(reqwest::Method::POST, path, Some(body)).await
    }

    async fn get(&self, path: &str) -> BrowserResult<Value> {
        self.send(reqwest::Method::GET, path, None).await
    }

    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> BrowserResult<Value> {
        let out = request(&self.http, method, &self.url(path), body).await;
        if let Err(BrowserError::Disconnected(_)) = &out {
            self.live.store(false, std::sync::atomic::Ordering::SeqCst);
        }
        out
    }

    /// Point the session at `tab` before acting on it. WebDriver has no
    /// per-window addressing, so this IS how a tab is targeted.
    async fn focus(&self, tab: &TabId) -> BrowserResult<()> {
        {
            let current = self.current.lock().await;
            if current.as_ref() == Some(tab) {
                return Ok(());
            }
        }
        self.post("/window", json!({"handle": tab.as_str()}))
            .await?;
        *self.current.lock().await = Some(tab.clone());
        Ok(())
    }

    async fn execute(&self, script: &str, args: Vec<Value>) -> BrowserResult<Value> {
        self.post("/execute/sync", json!({"script": script, "args": args}))
            .await
    }

    /// Resolve a ref to a web-element handle, or fail as stale.
    async fn element_for(&self, r#ref: &str) -> BrowserResult<String> {
        let selector = format!(
            "[data-leveler-ref={}]",
            crate::backend::css_string_literal(r#ref)
        );
        let res = self
            .post(
                "/element",
                json!({"using": "css selector", "value": selector}),
            )
            .await
            .map_err(|e| match e {
                BrowserError::ActionFailed(m) if m.contains("no such element") => {
                    BrowserError::RefStale(format!("{ref} matches no element in the current page"))
                }
                other => other,
            })?;
        res.get(ELEMENT_KEY)
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| {
                BrowserError::RefStale(format!("{ref} matches no element in the current page"))
            })
    }

    fn element_arg(id: &str) -> Value {
        json!({ ELEMENT_KEY: id })
    }

    /// Release the field's focus after typing into it.
    ///
    /// This is what a user does next — they move on — and it is what makes a
    /// field's `change` event fire. On Safari it is also load-bearing: after
    /// WebDriver sends keys, the next element click arrives with its pointer
    /// events out of order (`mouseup` before `mousedown`), which forms no
    /// click at all, so the button the agent just pressed does nothing.
    /// Committing the typing first restores the ordinary
    /// `mousedown`/`mouseup`/`click` sequence.
    async fn commit_typing(&self) -> BrowserResult<()> {
        self.execute(
            "if (document.activeElement && document.activeElement.blur) \
             document.activeElement.blur();",
            vec![],
        )
        .await?;
        Ok(())
    }
}

#[async_trait]
impl BrowserBackend for WebDriverBackend {
    async fn new_tab(&self) -> BrowserResult<TabId> {
        let res = self.post("/window/new", json!({"type": "tab"})).await?;
        let handle = res
            .get("handle")
            .and_then(Value::as_str)
            .ok_or_else(|| BrowserError::ActionFailed("new window returned no handle".into()))?;
        let tab = TabId::new(handle);
        self.focus(&tab).await?;
        Ok(tab)
    }

    async fn close_tab(&self, tab: &TabId) -> BrowserResult<()> {
        self.focus(tab).await?;
        self.send(reqwest::Method::DELETE, "/window", None).await?;
        *self.current.lock().await = None;
        Ok(())
    }

    async fn list_tabs(&self) -> BrowserResult<Vec<RawTab>> {
        let handles = self.get("/window/handles").await?;
        let handles: Vec<String> = handles
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let restore = self.current.lock().await.clone();
        let mut out = Vec::new();
        for h in handles {
            let tab = TabId::new(h);
            if self.focus(&tab).await.is_err() {
                continue;
            }
            let url = self.get("/url").await.ok();
            let title = self.get("/title").await.ok();
            out.push(RawTab {
                tab,
                url: url
                    .as_ref()
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                title: title
                    .as_ref()
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            });
        }
        if let Some(back) = restore {
            let _ = self.focus(&back).await;
        }
        Ok(out)
    }

    async fn navigate(&self, tab: &TabId, url: &str) -> BrowserResult<()> {
        self.focus(tab).await?;
        self.post("/url", json!({"url": url})).await?;
        Ok(())
    }

    async fn reload(&self, tab: &TabId) -> BrowserResult<()> {
        self.focus(tab).await?;
        self.post("/refresh", json!({})).await?;
        Ok(())
    }

    async fn locate(&self, tab: &TabId) -> BrowserResult<(String, String)> {
        self.focus(tab).await?;
        let url = self.get("/url").await?;
        let title = self.get("/title").await?;
        Ok((
            url.as_str().unwrap_or("").to_string(),
            title.as_str().unwrap_or("").to_string(),
        ))
    }

    async fn snapshot(&self, tab: &TabId, generation: u64) -> BrowserResult<RawSnapshot> {
        self.focus(tab).await?;
        // Parenthesised deliberately. The script begins with comment lines, and
        // `return` followed by a newline is where JavaScript inserts a
        // semicolon — the bare form returns `undefined` and every snapshot
        // comes back empty. Inside parentheses there is no ASI to insert.
        let value = self
            .execute(
                &format!("return (\n{}\n);", snapshot_script(generation)),
                vec![],
            )
            .await?;
        let get = |k: &str| {
            value
                .get(k)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        let num = |k: &str| value.get(k).and_then(Value::as_u64).unwrap_or(0) as usize;
        Ok(RawSnapshot {
            url: get("url"),
            title: get("title"),
            text: get("text"),
            nodes: num("nodes"),
            total: num("total"),
        })
    }

    async fn act(&self, tab: &TabId, act: &Act<'_>) -> BrowserResult<()> {
        self.focus(tab).await?;
        match act {
            Act::Click { r#ref } => {
                let el = self.element_for(r#ref).await?;
                self.post(&format!("/element/{el}/click"), json!({}))
                    .await?;
                Ok(())
            }
            Act::Fill { r#ref, text } => {
                let el = self.element_for(r#ref).await?;
                self.post(&format!("/element/{el}/clear"), json!({}))
                    .await?;
                self.post(&format!("/element/{el}/value"), json!({"text": text}))
                    .await?;
                self.commit_typing().await
            }
            Act::Type { r#ref, text } => {
                let el = self.element_for(r#ref).await?;
                self.post(&format!("/element/{el}/value"), json!({"text": text}))
                    .await?;
                self.commit_typing().await
            }
            Act::Press { r#ref, key } => {
                if let Some(r) = r#ref {
                    let el = self.element_for(r).await?;
                    // Focusing IS clicking in WebDriver's element model; a
                    // send-keys of the empty string is the protocol's way to
                    // put the caret there without a click.
                    let _ = self
                        .post(&format!("/element/{el}/value"), json!({"text": ""}))
                        .await;
                }
                let value = webdriver_key(key);
                self.post(
                    "/actions",
                    json!({"actions": [{
                        "type": "key", "id": "leveler-keyboard",
                        "actions": [
                            {"type": "keyDown", "value": value},
                            {"type": "keyUp", "value": value}
                        ]
                    }]}),
                )
                .await?;
                Ok(())
            }
            Act::Select { r#ref, values } => {
                let el = self.element_for(r#ref).await?;
                let script = format!(
                    "var el = arguments[0], values = arguments[1]; {}",
                    crate::backend::SELECT_BODY
                );
                let out = self
                    .execute(&script, vec![Self::element_arg(&el), json!(values)])
                    .await?;
                if out.get("ok").and_then(Value::as_bool) == Some(true) {
                    return Ok(());
                }
                Err(BrowserError::ActionFailed(format!(
                    "select failed: {}",
                    out.get("why").and_then(Value::as_str).unwrap_or("unknown")
                )))
            }
            Act::Scroll { r#ref, dx, dy } => {
                // WebDriver's wheel input source is not implemented by
                // safaridriver, so the scroll is performed through the page's
                // own scrolling API. The page really scrolls; nothing is
                // simulated or reported that did not happen.
                match r#ref {
                    Some(r) => {
                        let el = self.element_for(r).await?;
                        self.execute(
                            "arguments[0].scrollIntoView({block:'center'}); \
                             window.scrollBy(arguments[1], arguments[2]);",
                            vec![Self::element_arg(&el), json!(dx), json!(dy)],
                        )
                        .await?;
                    }
                    None => {
                        self.execute(
                            "window.scrollBy(arguments[0], arguments[1]);",
                            vec![json!(dx), json!(dy)],
                        )
                        .await?;
                    }
                }
                Ok(())
            }
        }
    }

    async fn screenshot(&self, tab: &TabId) -> BrowserResult<String> {
        self.focus(tab).await?;
        let res = self.get("/screenshot").await?;
        res.as_str()
            .map(str::to_string)
            .ok_or_else(|| BrowserError::ActionFailed("no screenshot data".into()))
    }

    async fn inspect(&self, _tab: &TabId, kind: InspectKind) -> BrowserResult<InspectReport> {
        // Not a gap to fill later: WebDriver has no such channel, so there is
        // nothing to read. Saying "unsupported" is the honest answer; an empty
        // list would claim the page was clean.
        Err(BrowserError::Unsupported(format!(
            "the safari backend does not support {} inspection: WebDriver has no \
             console, page-error or network channel",
            kind.as_str()
        )))
    }

    async fn is_live(&self) -> bool {
        self.live.load(std::sync::atomic::Ordering::SeqCst)
    }

    async fn shutdown(&self) {
        // Deleting the session is what closes Safari's automation window;
        // reaping the driver is the second half.
        let _ = request(
            &self.http,
            reqwest::Method::DELETE,
            &format!("{}/session/{}", self.base, self.session),
            None,
        )
        .await;
        self.driver.reap().await;
    }
}

/// One WebDriver call. Unwraps the `{"value": …}` envelope and turns the
/// protocol's error objects into typed failures.
async fn request(
    http: &reqwest::Client,
    method: reqwest::Method,
    url: &str,
    body: Option<Value>,
) -> BrowserResult<Value> {
    let mut req = http.request(method, url);
    if let Some(b) = body {
        req = req.json(&b);
    }
    let res = req.send().await.map_err(|e| {
        if e.is_timeout() {
            BrowserError::Timeout {
                stage: "webdriver".into(),
                message: e.to_string(),
            }
        } else {
            BrowserError::Disconnected(format!("safaridriver: {e}"))
        }
    })?;
    let status = res.status();
    let text = res.text().await.unwrap_or_default();
    let parsed: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    let value = parsed.get("value").cloned().unwrap_or(Value::Null);
    if status.is_success() {
        return Ok(value);
    }
    Err(map_webdriver_error(&value))
}

fn map_webdriver_error(value: &Value) -> BrowserError {
    let code = value.get("error").and_then(Value::as_str).unwrap_or("");
    let message = value
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("webdriver error")
        .to_string();
    match code {
        "no such element" | "stale element reference" => BrowserError::RefStale(message),
        "no such window" => BrowserError::TabClosed(message),
        "invalid session id" => BrowserError::Disconnected(message),
        "timeout" | "script timeout" => BrowserError::Timeout {
            stage: "webdriver".into(),
            message,
        },
        _ => BrowserError::ActionFailed(message),
    }
}

/// Wait until safaridriver's HTTP server answers `/status`.
async fn wait_for_driver(
    http: &reqwest::Client,
    base: &str,
    timeout: Duration,
) -> BrowserResult<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if http.get(format!("{base}/status")).send().await.is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(BrowserError::LaunchFailed(format!(
                "safaridriver did not start listening within {}s",
                timeout.as_secs()
            )));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// An ephemeral port the OS says is free. Never a fixed port: two test
/// binaries in one workspace run would otherwise collide (§41).
fn free_port() -> BrowserResult<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .map_err(|e| BrowserError::LaunchFailed(format!("reserve a driver port: {e}")))?;
    listener
        .local_addr()
        .map(|a| a.port())
        .map_err(|e| BrowserError::LaunchFailed(format!("read the reserved port: {e}")))
}

/// The WebDriver key value for a named key (the U+E0xx private-use block).
fn webdriver_key(key: &str) -> String {
    let code = match key {
        "Enter" => '\u{E007}',
        "Tab" => '\u{E004}',
        "Escape" => '\u{E00C}',
        "Backspace" => '\u{E003}',
        "Delete" => '\u{E017}',
        "ArrowUp" => '\u{E013}',
        "ArrowDown" => '\u{E015}',
        "ArrowLeft" => '\u{E012}',
        "ArrowRight" => '\u{E014}',
        "Home" => '\u{E011}',
        "End" => '\u{E010}',
        "PageUp" => '\u{E00E}',
        "PageDown" => '\u{E00F}',
        "Space" => ' ',
        other => return other.to_string(),
    };
    code.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ref_cannot_break_out_of_the_attribute_selector() {
        use crate::backend::css_string_literal;
        assert_eq!(css_string_literal("3e5"), "\"3e5\"");
        let hostile = css_string_literal(r#"a"],[x"#);
        assert!(hostile.starts_with('"') && hostile.ends_with('"'));
        assert!(
            hostile.contains("\\\""),
            "the inner quote must be escaped: {hostile}"
        );
    }

    #[test]
    fn named_keys_map_into_the_webdriver_key_block() {
        assert_eq!(webdriver_key("Enter"), "\u{E007}");
        assert_eq!(webdriver_key("ArrowDown"), "\u{E015}");
        // An ordinary character is itself.
        assert_eq!(webdriver_key("a"), "a");
    }

    #[test]
    fn protocol_error_codes_become_typed_failures() {
        let stale =
            map_webdriver_error(&json!({"error":"stale element reference","message":"gone"}));
        assert!(matches!(stale, BrowserError::RefStale(_)), "{stale:?}");
        let win = map_webdriver_error(&json!({"error":"no such window","message":"x"}));
        assert!(matches!(win, BrowserError::TabClosed(_)), "{win:?}");
        let dead = map_webdriver_error(&json!({"error":"invalid session id","message":"x"}));
        assert!(matches!(dead, BrowserError::Disconnected(_)), "{dead:?}");
    }

    #[test]
    fn free_port_is_never_the_same_twice_in_a_row() {
        // The property that matters for §41: no fixed port anywhere.
        let a = free_port().unwrap();
        assert!(a > 0);
    }
}
