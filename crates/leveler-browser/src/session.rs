//! The CodeLeveler half of the browser: everything the two protocols do NOT
//! own.
//!
//! Refs and generations, tab ownership between agent sessions, lazy start,
//! disconnect lifecycle, and the product decision. A backend below this line
//! knows nothing about any of it; this module never speaks CDP or WebDriver.
//!
//! Ref ownership is single (§18): the generation is decided HERE, the DOM
//! carries the label the snapshot script stamped, and there is no third copy.
//! Every snapshot mints a new generation, so exactly one generation's labels
//! exist in a page at a time and a superseded ref matches nothing.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use leveler_core::EnvSnapshot;
use tokio::sync::Mutex;

use crate::backend::{Act, BrowserBackend};
use crate::cdp::CdpBackend;
use crate::discover::{Launcher, availability, select_product, system_default_product};
use crate::webdriver::WebDriverBackend;
use crate::{
    ActionOutcome, BrowserError, BrowserProduct, BrowserResult, BrowserSessionId, BrowserSnapshot,
    BrowserStatus, InspectKind, InspectReport, TabId, TabInfo,
};

/// Snapshot output budget. Never truncates silently.
const MAX_SNAPSHOT_LINES: usize = 500;
const MAX_SNAPSHOT_CHARS: usize = 24_000;

/// What one tab's refs are currently keyed to.
struct TabState {
    session: BrowserSessionId,
    generation: u64,
}

/// The live half: one browser, its tabs, and who owns them.
struct Live {
    backend: Arc<dyn BrowserBackend>,
    product: BrowserProduct,
    tabs: HashMap<TabId, TabState>,
    active: HashMap<BrowserSessionId, TabId>,
    /// Set the moment the protocol connection drops. A live-looking status
    /// over a dead connection is the defect this field exists to prevent.
    disconnected: Option<String>,
}

/// The browser capability handle. Cheap to hold: nothing starts until the
/// first navigate.
pub struct Browser {
    env: EnvSnapshot,
    profile_dir: PathBuf,
    /// `[browser].default`, when the user set one.
    configured: Option<BrowserProduct>,
    live: Mutex<Option<Live>>,
}

impl Browser {
    pub fn new(env: EnvSnapshot, profile_dir: PathBuf, configured: Option<BrowserProduct>) -> Self {
        Self {
            env,
            profile_dir,
            configured,
            live: Mutex::new(None),
        }
    }

    /// Which product this host would drive, and whether it can. The answer the
    /// composition root turns into capability availability.
    ///
    /// It never answers with a product other than the selected one: if the
    /// user's default browser cannot be driven, that is the answer (§34).
    pub fn resolve(&self, explicit: Option<BrowserProduct>) -> BrowserResult<BrowserProduct> {
        let selected = select_product(explicit, self.configured, system_default_product())?;
        availability(&self.env, selected.product).map_err(|e| match e {
            BrowserError::Unavailable(why) => BrowserError::Unavailable(format!(
                "{} is {}, and {why}",
                selected.product,
                selected.source.as_str()
            )),
            other => other,
        })?;
        Ok(selected.product)
    }

    /// Current lifecycle state, without starting anything.
    pub async fn status(&self) -> BrowserStatus {
        match &*self.live.lock().await {
            None => BrowserStatus::NotStarted,
            Some(l) => match &l.disconnected {
                Some(reason) => BrowserStatus::Disconnected {
                    product: l.product,
                    reason: reason.clone(),
                },
                None => BrowserStatus::Live { product: l.product },
            },
        }
    }

    /// The product currently driving, if one is live.
    pub async fn live_product(&self) -> Option<BrowserProduct> {
        self.live.lock().await.as_ref().map(|l| l.product)
    }

    /// Close the browser and reap its process tree. For daemon shutdown.
    pub async fn shutdown(&self) {
        let live = self.live.lock().await.take();
        if let Some(live) = live {
            live.backend.shutdown().await;
        }
    }

    /// Start the selected browser, or reuse the live one.
    ///
    /// A disconnected browser is REPLACED here and only here: every other
    /// operation reports the disconnect instead of quietly restarting, so the
    /// model is never handed a fresh, empty page that looks like its old one.
    async fn ensure_live(&self, explicit: Option<BrowserProduct>) -> BrowserResult<()> {
        let mut guard = self.live.lock().await;
        if let Some(live) = guard.as_ref() {
            // Two reasons to replace what is running: the caller named a
            // different browser, or this one is no longer answering.
            let another_browser = explicit.is_some_and(|p| p != live.product);
            let gone = live.disconnected.is_some() || !live.backend.is_live().await;
            if !another_browser && !gone {
                return Ok(());
            }
            if let Some(old) = guard.take() {
                old.backend.shutdown().await;
            }
        }

        let product = self.resolve(explicit)?;
        let backend: Arc<dyn BrowserBackend> = match availability(&self.env, product)? {
            Launcher::Cdp { executable } => {
                Arc::new(CdpBackend::launch(&executable, &self.profile_dir).await?)
            }
            Launcher::WebDriver { driver } => Arc::new(WebDriverBackend::launch(&driver).await?),
        };
        *guard = Some(Live {
            backend,
            product,
            tabs: HashMap::new(),
            active: HashMap::new(),
            disconnected: None,
        });
        Ok(())
    }

    /// The live backend, or the mechanical reason there is none.
    async fn backend(&self) -> BrowserResult<Arc<dyn BrowserBackend>> {
        let guard = self.live.lock().await;
        match guard.as_ref() {
            Some(l) if l.disconnected.is_none() => Ok(l.backend.clone()),
            Some(l) => Err(BrowserError::Disconnected(format!(
                "{}: {}",
                l.product,
                l.disconnected.clone().unwrap_or_default()
            ))),
            None => Err(BrowserError::Unavailable(
                "no browser has been started in this session".into(),
            )),
        }
    }

    /// Record a disconnect the moment a protocol call reports one, so status
    /// can never say Live over a dead connection (§24).
    async fn note(&self, e: BrowserError) -> BrowserError {
        if let BrowserError::Disconnected(reason) = &e
            && let Some(l) = self.live.lock().await.as_mut()
            && l.disconnected.is_none()
        {
            l.disconnected = Some(reason.clone());
        }
        e
    }

    /// Run a protocol result through the disconnect recorder, so a dropped
    /// connection is remembered at the moment it is observed.
    async fn guarded<T>(&self, result: BrowserResult<T>) -> BrowserResult<T> {
        match result {
            Err(e) => Err(self.note(e).await),
            ok => ok,
        }
    }

    /// The tab this agent session is currently on.
    pub async fn active_tab(&self, session: &BrowserSessionId) -> BrowserResult<TabId> {
        let guard = self.live.lock().await;
        guard
            .as_ref()
            .and_then(|l| l.active.get(session))
            .cloned()
            .ok_or_else(|| BrowserError::TabClosed("this session has no open tab".into()))
    }

    async fn own(&self, session: &BrowserSessionId, tab: &TabId) -> BrowserResult<()> {
        let guard = self.live.lock().await;
        let tabs = &guard
            .as_ref()
            .ok_or_else(|| BrowserError::Unavailable("no browser started".into()))?
            .tabs;
        own_tab(tabs, session, tab)
    }

    async fn generation(&self, tab: &TabId) -> BrowserResult<u64> {
        self.live
            .lock()
            .await
            .as_ref()
            .and_then(|l| l.tabs.get(tab))
            .map(|t| t.generation)
            .ok_or_else(|| BrowserError::TabClosed(format!("unknown tab {tab}")))
    }

    async fn bump(&self, tab: &TabId) -> BrowserResult<u64> {
        let mut guard = self.live.lock().await;
        let state = guard
            .as_mut()
            .and_then(|l| l.tabs.get_mut(tab))
            .ok_or_else(|| BrowserError::TabClosed(format!("unknown tab {tab}")))?;
        state.generation += 1;
        Ok(state.generation)
    }

    // ── the operations the three tools are built from ───────────────────────

    /// Open `url`, reusing this session's tab or creating one. The only entry
    /// point that starts (or restarts) a browser.
    pub async fn navigate(
        &self,
        session: &BrowserSessionId,
        explicit: Option<BrowserProduct>,
        url: &str,
    ) -> BrowserResult<ActionOutcome> {
        refuse_metadata_target(url)?;
        self.ensure_live(explicit).await?;
        let backend = self.backend().await?;
        let tab = match self.active_tab(session).await {
            Ok(t) => t,
            Err(_) => self.open_tab(session, &backend).await?,
        };
        self.guarded(backend.navigate(&tab, url).await).await?;
        // Navigation replaces the document, so every prior ref is dead before
        // the model can even ask.
        self.bump(&tab).await?;
        self.outcome(&backend, &tab, true, None).await
    }

    pub async fn reload(&self, session: &BrowserSessionId) -> BrowserResult<ActionOutcome> {
        let backend = self.backend().await?;
        let tab = self.active_tab(session).await?;
        self.own(session, &tab).await?;
        self.guarded(backend.reload(&tab).await).await?;
        self.bump(&tab).await?;
        self.outcome(&backend, &tab, true, None).await
    }

    /// A new, empty tab owned by this session.
    pub async fn new_tab(&self, session: &BrowserSessionId) -> BrowserResult<TabId> {
        let backend = self.backend().await?;
        self.open_tab(session, &backend).await
    }

    async fn open_tab(
        &self,
        session: &BrowserSessionId,
        backend: &Arc<dyn BrowserBackend>,
    ) -> BrowserResult<TabId> {
        let tab = self.guarded(backend.new_tab().await).await?;
        let mut guard = self.live.lock().await;
        if let Some(l) = guard.as_mut() {
            l.tabs.insert(
                tab.clone(),
                TabState {
                    session: session.clone(),
                    generation: 0,
                },
            );
            l.active.insert(session.clone(), tab.clone());
        }
        Ok(tab)
    }

    pub async fn close_tab(&self, session: &BrowserSessionId, tab: &TabId) -> BrowserResult<()> {
        let backend = self.backend().await?;
        self.own(session, tab).await?;
        self.guarded(backend.close_tab(tab).await).await?;
        let mut guard = self.live.lock().await;
        if let Some(l) = guard.as_mut() {
            l.tabs.remove(tab);
            l.active.retain(|_, t| t != tab);
        }
        Ok(())
    }

    pub async fn select_tab(
        &self,
        session: &BrowserSessionId,
        tab: &TabId,
    ) -> BrowserResult<ActionOutcome> {
        let backend = self.backend().await?;
        self.own(session, tab).await?;
        {
            let mut guard = self.live.lock().await;
            if let Some(l) = guard.as_mut() {
                l.active.insert(session.clone(), tab.clone());
            }
        }
        self.outcome(&backend, tab, false, None).await
    }

    /// The tabs this session owns, reconciled against what the browser still
    /// has. A session never sees another session's tabs (§19).
    pub async fn tabs(&self, session: &BrowserSessionId) -> BrowserResult<Vec<TabInfo>> {
        let backend = self.backend().await?;
        let live = self.guarded(backend.list_tabs().await).await?;
        let mut guard = self.live.lock().await;
        let l = guard
            .as_mut()
            .ok_or_else(|| BrowserError::Unavailable("no browser started".into()))?;
        let ids: Vec<TabId> = live.iter().map(|t| t.tab.clone()).collect();
        l.tabs.retain(|id, _| ids.contains(id));
        l.active.retain(|_, t| ids.contains(t));
        let current = l.active.get(session).cloned();
        Ok(owned_tabs(&l.tabs, &live, session, current.as_ref()))
    }

    /// A bounded semantic snapshot. Mints a new generation, so the refs it
    /// returns are the only live ones on that tab.
    pub async fn snapshot(
        &self,
        session: &BrowserSessionId,
        tab: Option<TabId>,
    ) -> BrowserResult<BrowserSnapshot> {
        let backend = self.backend().await?;
        let tab = match tab {
            Some(t) => t,
            None => self.active_tab(session).await?,
        };
        self.own(session, &tab).await?;
        {
            let mut guard = self.live.lock().await;
            if let Some(l) = guard.as_mut() {
                l.active.insert(session.clone(), tab.clone());
            }
        }
        let generation = self.bump(&tab).await?;
        let raw = self
            .guarded(backend.snapshot(&tab, generation).await)
            .await?;
        let (text, truncated, nodes_returned, approximate_total) =
            budget(&raw.text, raw.nodes, raw.total);
        Ok(BrowserSnapshot {
            tab,
            url: raw.url,
            title: raw.title,
            generation,
            text,
            truncated,
            nodes_returned,
            approximate_total,
        })
    }

    /// Perform one interaction. The ref's generation is checked HERE, before
    /// any protocol call: a ref from a superseded snapshot is stale, never a
    /// lookalike element (§18).
    pub async fn act(
        &self,
        session: &BrowserSessionId,
        tab: Option<TabId>,
        act: Act<'_>,
    ) -> BrowserResult<ActionOutcome> {
        let backend = self.backend().await?;
        let tab = match tab {
            Some(t) => t,
            None => self.active_tab(session).await?,
        };
        self.own(session, &tab).await?;
        let generation = self.generation(&tab).await?;
        if let Some(r) = act_ref(&act) {
            check_ref(r, generation)?;
        }
        let before = backend
            .locate(&tab)
            .await
            .map(|(u, _)| u)
            .unwrap_or_default();
        self.guarded(backend.act(&tab, &act).await).await?;
        // A click can open a tab. Adopt it into THIS session so the model can
        // reach it, and no other session can (§19).
        let new_tab = if matches!(act, Act::Click { .. }) {
            self.adopt_new_tabs(session, &backend).await
        } else {
            None
        };
        let after = backend
            .locate(&tab)
            .await
            .map(|(u, _)| u)
            .unwrap_or_default();
        let navigated = !before.is_empty() && before != after;
        if navigated {
            self.bump(&tab).await?;
        }
        self.outcome(&backend, &tab, navigated, new_tab).await
    }

    pub async fn screenshot(
        &self,
        session: &BrowserSessionId,
        tab: Option<TabId>,
    ) -> BrowserResult<String> {
        let backend = self.backend().await?;
        let tab = match tab {
            Some(t) => t,
            None => self.active_tab(session).await?,
        };
        self.own(session, &tab).await?;
        self.guarded(backend.screenshot(&tab).await).await
    }

    /// Read one observation channel. A backend that has no such channel says
    /// so; it is never served by the other backend (§34).
    pub async fn inspect(
        &self,
        session: &BrowserSessionId,
        tab: Option<TabId>,
        kind: InspectKind,
    ) -> BrowserResult<InspectReport> {
        let backend = self.backend().await?;
        let tab = match tab {
            Some(t) => t,
            None => self.active_tab(session).await?,
        };
        self.own(session, &tab).await?;
        self.guarded(backend.inspect(&tab, kind).await).await
    }

    /// Adopt page targets the browser has that this session does not know of.
    async fn adopt_new_tabs(
        &self,
        session: &BrowserSessionId,
        backend: &Arc<dyn BrowserBackend>,
    ) -> Option<TabId> {
        let live = backend.list_tabs().await.ok()?;
        let mut guard = self.live.lock().await;
        let l = guard.as_mut()?;
        let mut adopted = None;
        for t in live {
            if l.tabs.contains_key(&t.tab) {
                continue;
            }
            l.tabs.insert(
                t.tab.clone(),
                TabState {
                    session: session.clone(),
                    generation: 0,
                },
            );
            adopted = Some(t.tab);
        }
        adopted
    }

    async fn outcome(
        &self,
        backend: &Arc<dyn BrowserBackend>,
        tab: &TabId,
        navigated: bool,
        new_tab: Option<TabId>,
    ) -> BrowserResult<ActionOutcome> {
        let (url, title) = backend.locate(tab).await.unwrap_or_default();
        Ok(ActionOutcome {
            tab: tab.clone(),
            url,
            title,
            navigated,
            new_tab,
        })
    }
}

fn act_ref<'a>(act: &Act<'a>) -> Option<&'a str> {
    match act {
        Act::Click { r#ref }
        | Act::Fill { r#ref, .. }
        | Act::Type { r#ref, .. }
        | Act::Select { r#ref, .. } => Some(r#ref),
        Act::Press { r#ref, .. } | Act::Scroll { r#ref, .. } => *r#ref,
    }
}

/// A model-facing ref is `<generation>e<n>`. Self-describing, so a superseded
/// one is refused before any protocol call and without server-side memory.
fn check_ref(r#ref: &str, current: u64) -> BrowserResult<()> {
    let Some((generation, token)) = parse_ref(r#ref) else {
        return Err(BrowserError::RefStale(format!(
            "{ref} is not a ref from a snapshot"
        )));
    };
    let _ = token;
    if generation != current {
        return Err(BrowserError::RefStale(format!(
            "{ref} is from snapshot generation {generation}; this tab is on {current}"
        )));
    }
    Ok(())
}

fn parse_ref(s: &str) -> Option<(u64, u64)> {
    let (generation, rest) = s.split_once('e')?;
    Some((generation.parse().ok()?, rest.parse().ok()?))
}

/// A session may only act on tabs it owns.
fn own_tab(
    tabs: &HashMap<TabId, TabState>,
    session: &BrowserSessionId,
    tab: &TabId,
) -> BrowserResult<()> {
    match tabs.get(tab) {
        None => Err(BrowserError::TabClosed(format!("unknown tab {tab}"))),
        Some(s) if &s.session != session => Err(BrowserError::TabClosed(format!(
            "tab {tab} belongs to another session"
        ))),
        Some(_) => Ok(()),
    }
}

/// The tabs one session may see. Pure, so isolation is tested without a
/// browser.
fn owned_tabs(
    tabs: &HashMap<TabId, TabState>,
    live: &[crate::backend::RawTab],
    session: &BrowserSessionId,
    current: Option<&TabId>,
) -> Vec<TabInfo> {
    let mut out = Vec::new();
    for raw in live {
        match tabs.get(&raw.tab) {
            Some(state) if &state.session == session => out.push(TabInfo {
                tab: raw.tab.clone(),
                url: raw.url.clone(),
                title: raw.title.clone(),
                active: current == Some(&raw.tab),
            }),
            _ => continue,
        }
    }
    out.sort_by(|a, b| a.tab.cmp(&b.tab));
    out
}

/// Enforce the snapshot output budget. Truncation is always reported.
fn budget(text: &str, nodes: usize, total: usize) -> (String, bool, usize, Option<usize>) {
    if text.len() <= MAX_SNAPSHOT_CHARS && text.lines().count() <= MAX_SNAPSHOT_LINES {
        return (text.to_string(), false, nodes, None);
    }
    let mut out = String::new();
    for (kept, line) in text.lines().enumerate() {
        if kept >= MAX_SNAPSHOT_LINES || out.len() + line.len() + 1 > MAX_SNAPSHOT_CHARS {
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    let kept = out.matches("] ").count();
    (out, true, kept, Some(total))
}

/// The one navigation-target rule that survives.
///
/// It is NOT a network boundary and does not pretend to be one: the browser is
/// an explicitly network-authorised capability, so localhost, LAN dev servers
/// and the public internet are all reachable, and nothing here can stop a page
/// the model already opened from fetching whatever it likes. What it does stop
/// is the model being steered — by a page, by a search result — into pointing
/// the browser at a cloud instance-metadata endpoint, which is a credential
/// read dressed as a navigation.
fn refuse_metadata_target(url: &str) -> BrowserResult<()> {
    let Some((scheme, rest)) = url.split_once("://") else {
        return Err(BrowserError::Denied(format!(
            "{url} is not an http/https URL"
        )));
    };
    if !matches!(scheme, "http" | "https") {
        return Err(BrowserError::Denied(format!(
            "{scheme}: is not a scheme the browser navigates to (http/https only)"
        )));
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(authority);
    let host = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(v6),
        None => host.split(':').next().unwrap_or(host),
    };
    if host.is_empty() {
        return Err(BrowserError::Denied(format!("{url} has no host")));
    }
    if is_metadata_host(host) {
        return Err(BrowserError::Denied(format!(
            "{host} is a cloud instance-metadata address"
        )));
    }
    Ok(())
}

/// Link-local, which is where every cloud provider parks its metadata service.
fn is_metadata_host(host: &str) -> bool {
    if let Ok(v4) = host.parse::<std::net::Ipv4Addr>() {
        return v4.is_link_local();
    }
    if let Ok(v6) = host.parse::<std::net::Ipv6Addr>() {
        let seg = v6.segments();
        // fe80::/10 link-local, plus the fd00:ec2::254 EC2 IPv6 endpoint.
        return (seg[0] & 0xffc0) == 0xfe80
            || v6 == "fd00:ec2::254".parse::<std::net::Ipv6Addr>().unwrap();
    }
    matches!(host, "metadata.google.internal" | "metadata.goog")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::RawTab;

    fn state(session: &str) -> TabState {
        TabState {
            session: BrowserSessionId::new(session),
            generation: 0,
        }
    }

    fn raw(id: &str, url: &str, title: &str) -> RawTab {
        RawTab {
            tab: TabId::new(id),
            url: url.into(),
            title: title.into(),
        }
    }

    #[test]
    fn a_session_sees_only_its_own_tabs() {
        let mut tabs = HashMap::new();
        tabs.insert(TabId::new("t1"), state("A"));
        tabs.insert(TabId::new("t2"), state("B"));
        let live = [raw("t1", "http://a", "A"), raw("t2", "http://b", "B")];

        let a = owned_tabs(&tabs, &live, &BrowserSessionId::new("A"), None);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].tab.as_str(), "t1");
        assert!(a.iter().all(|t| t.url != "http://b" && t.title != "B"));

        let b = owned_tabs(&tabs, &live, &BrowserSessionId::new("B"), None);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].tab.as_str(), "t2");
    }

    #[test]
    fn a_tab_the_browser_no_longer_has_disappears() {
        let mut tabs = HashMap::new();
        tabs.insert(TabId::new("t1"), state("A"));
        tabs.insert(TabId::new("t2"), state("A"));
        let live = [raw("t1", "http://a", "A")];
        let a = owned_tabs(&tabs, &live, &BrowserSessionId::new("A"), None);
        assert_eq!(a.len(), 1);
    }

    #[test]
    fn only_the_focused_tab_is_active() {
        let mut tabs = HashMap::new();
        tabs.insert(TabId::new("t1"), state("A"));
        tabs.insert(TabId::new("t3"), state("A"));
        let live = [raw("t1", "u1", "x"), raw("t3", "u3", "y")];
        let cur = TabId::new("t3");
        let a = owned_tabs(&tabs, &live, &BrowserSessionId::new("A"), Some(&cur));
        let active: Vec<_> = a
            .iter()
            .filter(|t| t.active)
            .map(|t| t.tab.as_str())
            .collect();
        assert_eq!(active, vec!["t3"]);
    }

    #[test]
    fn another_sessions_tab_is_refused() {
        let mut tabs = HashMap::new();
        tabs.insert(TabId::new("t1"), state("A"));
        assert!(own_tab(&tabs, &BrowserSessionId::new("A"), &TabId::new("t1")).is_ok());
        assert!(matches!(
            own_tab(&tabs, &BrowserSessionId::new("B"), &TabId::new("t1")),
            Err(BrowserError::TabClosed(_))
        ));
        assert!(matches!(
            own_tab(&tabs, &BrowserSessionId::new("A"), &TabId::new("t9")),
            Err(BrowserError::TabClosed(_))
        ));
    }

    #[test]
    fn a_ref_from_a_superseded_snapshot_is_stale_before_any_protocol_call() {
        assert!(check_ref("7e12", 7).is_ok());
        let e = check_ref("6e12", 7).unwrap_err();
        assert!(matches!(e, BrowserError::RefStale(_)), "{e:?}");
        assert!(e.to_string().contains("generation 6"), "{e}");
        // Nothing that is not a snapshot ref resolves to an element.
        assert!(check_ref("e12", 7).is_err());
        assert!(check_ref("button", 7).is_err());
        assert!(check_ref("", 7).is_err());
    }

    #[test]
    fn truncation_is_reported_never_silent() {
        let short = "[1e1] button \"Go\"";
        let (text, trunc, nodes, total) = budget(short, 1, 1);
        assert_eq!(text, short);
        assert!(!trunc && nodes == 1 && total.is_none());

        let long: String = (0..MAX_SNAPSHOT_LINES + 50)
            .map(|i| format!("[1e{i}] button \"b{i}\"\n"))
            .collect();
        let (_t, trunc, _n, total) = budget(&long, 550, 550);
        assert!(trunc);
        assert_eq!(total, Some(550));
    }

    /// The browser is a network-authorised capability, so the ordinary web —
    /// localhost, a LAN dev box, the public internet — is reachable.
    #[test]
    fn ordinary_navigation_targets_are_allowed() {
        for url in [
            "http://localhost:3000/users",
            "http://127.0.0.1:5173/",
            "https://[::1]:8080/",
            "http://192.168.1.50:3000/",
            "http://10.0.0.7/app",
            "https://example.com/docs",
            "https://api.github.com/repos",
        ] {
            assert!(
                refuse_metadata_target(url).is_ok(),
                "{url} must be reachable"
            );
        }
    }

    /// The one exception, and the reason for it: reading instance metadata is
    /// a credential read, not a page visit.
    #[test]
    fn instance_metadata_endpoints_are_refused() {
        for url in [
            "http://169.254.169.254/latest/meta-data/",
            "http://[fe80::1]/",
            "http://metadata.google.internal/computeMetadata/v1/",
        ] {
            let e = refuse_metadata_target(url).unwrap_err();
            assert!(matches!(e, BrowserError::Denied(_)), "{url}: {e:?}");
        }
    }

    #[test]
    fn only_http_urls_are_navigated_to() {
        for url in [
            "file:///etc/passwd",
            "data:text/html,x",
            "ftp://h/",
            "not-a-url",
        ] {
            assert!(refuse_metadata_target(url).is_err(), "{url}");
        }
    }
}
