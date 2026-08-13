//! `BrowserRuntime` — the daemon-owned, lazily-started browser.
//!
//! Owns one persistent browser context (the project profile) behind a managed
//! driver subprocess, and all the safety bookkeeping the driver does *not* do:
//! per-page generations, the valid-ref set, session isolation, and snapshot
//! budgeting. Everything returned crosses the tool boundary as a typed domain
//! value; the driver's raw JSON never escapes this module.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use leveler_core::{EnvSnapshot, LevelerHome};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::driver::BrowserDriver;
use crate::install::ensure_installed;
use crate::{
    BrowserActionResult, BrowserError, BrowserPageId, BrowserRuntimeInfo, BrowserRuntimeStatus,
    BrowserSessionId, BrowserSnapshot, ConsoleEntry, TabInfo,
};

/// Default per-operation deadlines.
const NAV_TIMEOUT: Duration = Duration::from_secs(30);
const ACTION_TIMEOUT: Duration = Duration::from_secs(15);
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(60);

/// Snapshot output budget (§23). Interactive-first ordering comes from the
/// driver's tree; V1 bounds by lines and chars and never truncates silently.
const MAX_SNAPSHOT_LINES: usize = 500;
const MAX_SNAPSHOT_CHARS: usize = 24_000;

/// Per-page safety state: which generation is current and which driver tokens
/// are valid in it.
struct PageState {
    session: BrowserSessionId,
    generation: u64,
    aria_hash: u64,
    tokens: HashSet<String>,
}

/// The live half of a started runtime.
struct Active {
    driver: Arc<BrowserDriver>,
    info: BrowserRuntimeInfo,
    pages: HashMap<BrowserPageId, PageState>,
    crashed: Option<String>,
}

/// The daemon-owned browser runtime. Cheap to hold; nothing starts until the
/// first operation calls [`Self::ensure_ready`].
pub struct BrowserRuntime {
    home: LevelerHome,
    env: EnvSnapshot,
    profile_dir: PathBuf,
    /// `None` until started; the `Mutex` is also the in-process singleflight.
    inner: Mutex<Option<Active>>,
}

impl BrowserRuntime {
    /// Construct a runtime bound to one project profile directory. Lazy: no
    /// process, no discovery, no install happens here.
    pub fn new(home: LevelerHome, env: EnvSnapshot, profile_dir: PathBuf) -> Self {
        Self {
            home,
            env,
            profile_dir,
            inner: Mutex::new(None),
        }
    }

    /// Current lifecycle status without starting anything.
    pub async fn status(&self) -> BrowserRuntimeStatus {
        match &*self.inner.lock().await {
            None => BrowserRuntimeStatus::NotReady,
            Some(a) => match &a.crashed {
                Some(reason) => BrowserRuntimeStatus::Crashed {
                    reason: reason.clone(),
                },
                None => BrowserRuntimeStatus::Ready,
            },
        }
    }

    /// Ensure a driver + browser are live, installing the managed runtime on
    /// first use. Idempotent and singleflighted (the `inner` mutex in-process,
    /// an `fs2` install lock cross-process). Returns non-sensitive runtime info.
    pub async fn ensure_ready(&self) -> Result<BrowserRuntimeInfo, BrowserError> {
        let mut guard = self.inner.lock().await;
        if let Some(a) = guard.as_ref()
            && a.crashed.is_none()
        {
            return Ok(a.info.clone());
        }
        // (Re)start: install if needed, spawn the driver, launch the context.
        let layout = ensure_installed(&self.home, &self.env).await?;
        let driver = BrowserDriver::spawn(
            &layout.node,
            &layout.driver_script,
            &layout.root,
            &layout.driver_env,
        )
        .await?;

        std::fs::create_dir_all(&self.profile_dir)
            .map_err(|e| BrowserError::ProfileUnavailable(format!("create profile dir: {e}")))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ =
                std::fs::set_permissions(&self.profile_dir, std::fs::Permissions::from_mode(0o700));
        }

        let mut launch = json!({
            "userDataDir": self.profile_dir.to_string_lossy(),
            "headless": true,
        });
        if let Some(ch) = &layout.channel {
            launch["channel"] = json!(ch);
        }
        let launched = driver
            .request("launch", launch, LAUNCH_TIMEOUT)
            .await
            .map_err(|e| BrowserError::LaunchFailed(e.to_string()))?;

        let info = BrowserRuntimeInfo {
            engine: layout.engine,
            browser_version: launched
                .get("browserVersion")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string(),
            driver_version: Some(crate::install::PINNED_PLAYWRIGHT.to_string()),
        };

        *guard = Some(Active {
            driver: Arc::new(driver),
            info: info.clone(),
            pages: HashMap::new(),
            crashed: None,
        });
        Ok(info)
    }

    /// Subscribe to driver events (console errors, page errors, dialogs) for
    /// forwarding to the TUI. `None` until the runtime has started.
    pub async fn subscribe_events(
        &self,
    ) -> Option<tokio::sync::broadcast::Receiver<crate::DriverEvent>> {
        self.inner
            .lock()
            .await
            .as_ref()
            .map(|a| a.driver.subscribe())
    }

    /// Open a fresh page owned by `session`.
    pub async fn new_page(
        &self,
        session: &BrowserSessionId,
    ) -> Result<BrowserPageId, BrowserError> {
        self.ensure_ready().await?;
        let mut guard = self.inner.lock().await;
        let active = active_mut(&mut guard)?;
        let res = active
            .driver
            .request("newPage", json!({}), ACTION_TIMEOUT)
            .await?;
        let page = BrowserPageId::new(
            res.get("pageId")
                .and_then(Value::as_str)
                .ok_or_else(|| BrowserError::ActionFailed("driver returned no pageId".into()))?,
        );
        active.pages.insert(
            page.clone(),
            PageState {
                session: session.clone(),
                generation: 0,
                aria_hash: 0,
                tokens: HashSet::new(),
            },
        );
        Ok(page)
    }

    /// Navigate a page to `url`. The caller (tool) is responsible for the
    /// network/SSRF gate before calling this.
    pub async fn navigate(
        &self,
        session: &BrowserSessionId,
        page: &BrowserPageId,
        url: &str,
    ) -> Result<BrowserActionResult, BrowserError> {
        let mut guard = self.inner.lock().await;
        let active = active_mut(&mut guard)?;
        own_page(active, session, page)?;
        let res = active
            .driver
            .request(
                "navigate",
                json!({"pageId": page.as_str(), "url": url, "timeoutMs": NAV_TIMEOUT.as_millis()}),
                NAV_TIMEOUT + Duration::from_secs(5),
            )
            .await?;
        // Navigation always advances the generation (page content replaced).
        let st = active.pages.get_mut(page).expect("owned");
        st.generation += 1;
        st.aria_hash = 0;
        st.tokens.clear();
        Ok(action_result(page, &res, st.generation))
    }

    /// A bounded, ref-annotated semantic snapshot. Bumps the page generation
    /// only when the accessibility tree actually changed.
    pub async fn snapshot(
        &self,
        session: &BrowserSessionId,
        page: &BrowserPageId,
    ) -> Result<BrowserSnapshot, BrowserError> {
        let mut guard = self.inner.lock().await;
        let active = active_mut(&mut guard)?;
        own_page(active, session, page)?;
        let res = active
            .driver
            .request("snapshot", json!({"pageId": page.as_str()}), ACTION_TIMEOUT)
            .await?;
        let aria = res.get("aria").and_then(Value::as_str).unwrap_or("");
        let url = res
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let title = res
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        let hash = fnv1a(aria);
        let st = active.pages.get_mut(page).expect("owned");
        if hash != st.aria_hash {
            st.generation += 1;
            st.aria_hash = hash;
            st.tokens = extract_tokens(aria);
        }
        let generation = st.generation;
        let (text, truncated, nodes_returned, approximate_total) =
            render_snapshot(aria, generation);
        Ok(BrowserSnapshot {
            page: page.clone(),
            url,
            title,
            generation,
            text,
            truncated,
            nodes_returned,
            approximate_total,
        })
    }

    /// Perform an interaction on the element identified by `ref_str` (§13 safe
    /// refs). A ref whose generation is not current, or whose token is not in
    /// the current valid set, fails as [`BrowserError::RefStale`] — never
    /// retargets.
    pub async fn interact(
        &self,
        session: &BrowserSessionId,
        page: &BrowserPageId,
        ref_str: &str,
        action: Interaction,
    ) -> Result<BrowserActionResult, BrowserError> {
        let mut guard = self.inner.lock().await;
        let active = active_mut(&mut guard)?;
        own_page(active, session, page)?;
        let (ref_gen, token) = parse_ref(ref_str)
            .ok_or_else(|| BrowserError::RefStale(format!("unparseable ref {ref_str}")))?;
        let st = active.pages.get(page).expect("owned");
        if ref_gen != st.generation || !st.tokens.contains(&token) {
            return Err(BrowserError::RefStale(format!(
                "ref {ref_str} is not current (page generation {})",
                st.generation
            )));
        }
        let (method, mut params) = action.to_request();
        params["pageId"] = json!(page.as_str());
        params["ref"] = json!(token);
        params["timeoutMs"] = json!(ACTION_TIMEOUT.as_millis());
        let res = active
            .driver
            .request(&method, params, ACTION_TIMEOUT + Duration::from_secs(5))
            .await?;
        // The action may have mutated the page; the next snapshot detects it and
        // bumps the generation. Report the current generation.
        let st = active.pages.get(page).expect("owned");
        Ok(action_result(page, &res, st.generation))
    }

    /// List the open pages/tabs (§34).
    pub async fn tabs(&self, _session: &BrowserSessionId) -> Result<Vec<TabInfo>, BrowserError> {
        let mut guard = self.inner.lock().await;
        let active = active_mut(&mut guard)?;
        let res = active
            .driver
            .request("listPages", json!({}), ACTION_TIMEOUT)
            .await?;
        let mut tabs = Vec::new();
        if let Some(arr) = res.get("pages").and_then(Value::as_array) {
            for p in arr {
                tabs.push(TabInfo {
                    page: BrowserPageId::new(p.get("pageId").and_then(Value::as_str).unwrap_or("")),
                    url: p
                        .get("url")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    title: p
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    active: p.get("active").and_then(Value::as_bool).unwrap_or(false),
                });
            }
        }
        Ok(tabs)
    }

    /// Set how the next dialog on a page is answered (§35), before the action
    /// that triggers it.
    pub async fn set_dialog_policy(
        &self,
        session: &BrowserSessionId,
        page: &BrowserPageId,
        accept: bool,
        prompt_text: Option<String>,
    ) -> Result<(), BrowserError> {
        let mut guard = self.inner.lock().await;
        let active = active_mut(&mut guard)?;
        own_page(active, session, page)?;
        active
            .driver
            .request(
                "setDialogPolicy",
                json!({"pageId": page.as_str(), "action": if accept {"accept"} else {"dismiss"}, "promptText": prompt_text}),
                ACTION_TIMEOUT,
            )
            .await?;
        Ok(())
    }

    /// Recent console errors/warnings and page errors for a page (§36).
    pub async fn console_tail(
        &self,
        session: &BrowserSessionId,
        page: &BrowserPageId,
    ) -> Result<Vec<ConsoleEntry>, BrowserError> {
        let mut guard = self.inner.lock().await;
        let active = active_mut(&mut guard)?;
        own_page(active, session, page)?;
        let res = active
            .driver
            .request(
                "consoleTail",
                json!({"pageId": page.as_str()}),
                ACTION_TIMEOUT,
            )
            .await?;
        let mut entries = Vec::new();
        if let Some(arr) = res.get("entries").and_then(Value::as_array) {
            for e in arr {
                entries.push(ConsoleEntry {
                    level: e
                        .get("level")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    text: e
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                });
            }
        }
        Ok(entries)
    }

    /// A PNG screenshot as base64 (for the tool's `metadata.image` channel).
    /// Returns `(base64, media_type)`.
    pub async fn screenshot(
        &self,
        session: &BrowserSessionId,
        page: &BrowserPageId,
        full_page: bool,
    ) -> Result<(String, String), BrowserError> {
        let mut guard = self.inner.lock().await;
        let active = active_mut(&mut guard)?;
        own_page(active, session, page)?;
        let res = active
            .driver
            .request(
                "screenshot",
                json!({"pageId": page.as_str(), "fullPage": full_page}),
                ACTION_TIMEOUT + Duration::from_secs(5),
            )
            .await?;
        let b64 = res
            .get("base64")
            .and_then(Value::as_str)
            .ok_or_else(|| BrowserError::ActionFailed("driver returned no screenshot".into()))?
            .to_string();
        let mt = res
            .get("mediaType")
            .and_then(Value::as_str)
            .unwrap_or("image/png")
            .to_string();
        Ok((b64, mt))
    }
}

/// The interaction verbs the runtime forwards to the driver.
pub enum Interaction {
    Click,
    /// Replace the field value (default) or append.
    Type {
        text: String,
        append: bool,
    },
    /// Select options by label or value.
    Select {
        values: Vec<String>,
        by_value: bool,
    },
    /// Press a key while the element is focused.
    Press {
        key: String,
    },
}

impl Interaction {
    fn to_request(&self) -> (String, Value) {
        match self {
            Self::Click => ("click".into(), json!({})),
            Self::Type { text, append } => ("type".into(), json!({"text": text, "append": append})),
            Self::Select { values, by_value } => (
                "select".into(),
                json!({"values": values, "by": if *by_value {"value"} else {"label"}}),
            ),
            Self::Press { key } => ("press".into(), json!({"key": key})),
        }
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn active_mut<'a>(
    guard: &'a mut tokio::sync::MutexGuard<'_, Option<Active>>,
) -> Result<&'a mut Active, BrowserError> {
    match guard.as_mut() {
        Some(a) if a.crashed.is_none() => Ok(a),
        Some(a) => Err(BrowserError::RuntimeCrashed(
            a.crashed.clone().unwrap_or_default(),
        )),
        None => Err(BrowserError::Unavailable("runtime not started".into())),
    }
}

/// Enforce session ownership of a page (§10/§18) — a session may never act on
/// another session's page.
fn own_page(
    active: &Active,
    session: &BrowserSessionId,
    page: &BrowserPageId,
) -> Result<(), BrowserError> {
    match active.pages.get(page) {
        None => Err(BrowserError::PageClosed(format!("unknown page {page}"))),
        Some(st) if &st.session != session => Err(BrowserError::ActionFailed(
            "page belongs to another session".into(),
        )),
        Some(_) => Ok(()),
    }
}

fn action_result(page: &BrowserPageId, res: &Value, generation: u64) -> BrowserActionResult {
    BrowserActionResult {
        page: page.clone(),
        url: res
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        title: res
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        navigated: res
            .get("navigated")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        new_page: res
            .get("newPage")
            .and_then(Value::as_str)
            .map(BrowserPageId::new),
        dialog: None,
        generation,
    }
}

/// Agent-facing ref = `<generation><token>` (e.g. `18e5`), so a ref is
/// self-describing and a stale generation is detectable without server memory.
fn parse_ref(s: &str) -> Option<(u64, String)> {
    let epos = s.find('e')?;
    let ref_gen: u64 = s.get(..epos)?.parse().ok()?;
    let token = s.get(epos..)?;
    if token.len() < 2 || !token[1..].chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some((ref_gen, token.to_string()))
}

/// Collect the driver's valid `eN` tokens from a raw aria snapshot.
fn extract_tokens(aria: &str) -> HashSet<String> {
    let mut tokens = HashSet::new();
    let mut rest = aria;
    while let Some(pos) = rest.find("ref=e") {
        let start = pos + 4; // at 'e'
        let after = &rest[start..];
        let end = after[1..]
            .char_indices()
            .find(|(_, c)| !c.is_ascii_digit())
            .map(|(i, _)| i + 1)
            .unwrap_or(after.len());
        tokens.insert(after[..end].to_string());
        rest = &rest[start + end..];
    }
    tokens
}

/// Rewrite `[ref=eN]` → `[ref=<gen>eN]` and enforce the output budget.
/// Returns (text, truncated, nodes_returned, approximate_total).
fn render_snapshot(aria: &str, generation: u64) -> (String, bool, usize, Option<usize>) {
    let rewritten = aria.replace("ref=e", &format!("ref={generation}e"));
    let total_lines = rewritten.lines().count();
    let interactive = rewritten.matches("[ref=").count();

    if rewritten.len() <= MAX_SNAPSHOT_CHARS && total_lines <= MAX_SNAPSHOT_LINES {
        return (rewritten, false, interactive, None);
    }
    // Budget by lines first, then hard char cap.
    let mut out = String::new();
    for (kept, line) in rewritten.lines().enumerate() {
        if kept >= MAX_SNAPSHOT_LINES || out.len() + line.len() + 1 > MAX_SNAPSHOT_CHARS {
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    let kept_interactive = out.matches("[ref=").count();
    (out, true, kept_interactive, Some(total_lines))
}

/// Small non-cryptographic hash for detecting snapshot change.
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ref_requires_generation_and_token() {
        assert_eq!(parse_ref("18e5"), Some((18, "e5".into())));
        assert_eq!(parse_ref("0e12"), Some((0, "e12".into())));
        assert_eq!(parse_ref("e5"), None, "no generation");
        assert_eq!(parse_ref("18x5"), None, "no token");
        assert_eq!(parse_ref("18e"), None, "empty token");
    }

    #[test]
    fn extract_tokens_reads_all_refs() {
        let aria = "- button \"A\" [ref=e5]\n- textbox \"B\" [ref=e12]";
        let t = extract_tokens(aria);
        assert!(t.contains("e5") && t.contains("e12") && t.len() == 2);
    }

    #[test]
    fn render_embeds_generation_and_counts_interactive() {
        let aria = "- heading \"Users\" [level=1]\n- button \"Create\" [ref=e5]";
        let (text, trunc, nodes, total) = render_snapshot(aria, 7);
        assert!(text.contains("[ref=7e5]"));
        assert!(!trunc && nodes == 1 && total.is_none());
    }

    #[test]
    fn render_truncates_over_budget_and_reports() {
        let mut aria = String::new();
        for i in 0..(MAX_SNAPSHOT_LINES + 50) {
            aria.push_str(&format!("- button \"b{i}\" [ref=e{i}]\n"));
        }
        let (_text, trunc, _nodes, total) = render_snapshot(&aria, 1);
        assert!(trunc);
        assert_eq!(total, Some(MAX_SNAPSHOT_LINES + 50));
    }
}
