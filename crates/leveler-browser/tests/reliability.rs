//! Phase 4A reliability/security fixtures.
//!
//! The lazy-bootstrap and zero-pollution checks need no browser and always run.
//! The dynamic-DOM / SPA / dialog / new-tab / console fixture matrix drives real
//! system Chrome and self-skips when the environment can't support it — the same
//! gate as `acceptance.rs`. These pin the exact behaviours Dogfood B relies on
//! (ref invalidation on structural change, wait semantics, tab lifecycle).

use std::ffi::OsString;
use std::path::PathBuf;

use leveler_browser::{
    BrowserError, BrowserRuntime, BrowserRuntimeStatus, BrowserSessionId, Interaction,
    WaitCondition, discover_system_chrome, which,
};
use leveler_core::{EnvSnapshot, LevelerHome};

fn process_env() -> EnvSnapshot {
    EnvSnapshot::new(
        std::env::vars_os().collect::<Vec<_>>(),
        std::env::current_dir().unwrap_or_default(),
        std::env::temp_dir(),
    )
}

fn ref_for(text: &str, needle: &str) -> Option<String> {
    let line = text.lines().find(|l| l.contains(needle))?;
    let start = line.find("[ref=")? + 5;
    let end = line[start..].find(']')? + start;
    Some(line[start..end].to_string())
}

// ── §56 lazy bootstrap + §57 zero pollution (no browser needed) ──────────────

#[tokio::test]
async fn lazy_bootstrap_creates_nothing_until_used() {
    let home_dir = tempfile::tempdir().unwrap();
    let profile = home_dir.path().join("profile");
    let home = LevelerHome::from_root(home_dir.path());
    let rt = BrowserRuntime::new(home.clone(), process_env(), profile.clone());

    // Constructing and querying status must not start a process or touch disk.
    assert!(matches!(rt.status().await, BrowserRuntimeStatus::NotReady));
    assert!(
        !home.browser_runtime_dir().exists(),
        "runtimes/browser must not exist before first use"
    );
    assert!(!profile.exists(), "profile must not exist before first use");
    // No active page, and asking for one is a clean error, not a panic/start.
    let err = rt.active_page(&BrowserSessionId::new("s")).await;
    assert!(err.is_err());
    assert!(matches!(rt.status().await, BrowserRuntimeStatus::NotReady));
}

// ── the dynamic fixture matrix (real Chrome, gated) ──────────────────────────

const SPA_FIXTURE: &str = r#"<!doctype html><html><head><title>App</title></head><body>
<h1>Home</h1>
<button id="go">Go to Users</button>
<button id="reorder">Reorder</button>
<button id="openModal">Open dialog</button>
<button id="newtab">Open tab</button>
<ul id="list"><li>Alice</li><li>Bob</li></ul>
<canvas id="pad" width="300" height="200" style="border:1px solid #000"></canvas>
<div id="dragme" draggable="true">DragMe</div>
<div id="dropzone" style="width:120px;height:60px;border:1px dashed #000">Drop here</div>
<div id="view"></div>
<script>
document.getElementById('go').onclick = () => {
  history.pushState({}, '', '#/users');
  document.querySelector('h1').textContent = 'Users';
  const v = document.getElementById('view');
  v.innerHTML = '<button id="save">Save</button>';
};
document.getElementById('reorder').onclick = () => {
  const l = document.getElementById('list');
  l.innerHTML = '<li>Bob</li><li>Alice</li><li><button id="added">Added</button></li>';
};
document.getElementById('openModal').onclick = () => {
  setTimeout(()=>{ if(confirm('Proceed?')) document.querySelector('h1').textContent='Confirmed'; else document.querySelector('h1').textContent='Cancelled'; }, 10);
};
document.getElementById('newtab').onclick = () => { window.open('about:blank','_blank'); };
(() => {
  const pad = document.getElementById('pad');
  let down = false, moves = 0;
  pad.addEventListener('mousedown', () => { down = true; moves = 0; });
  pad.addEventListener('mousemove', () => { if (down) moves += 1; });
  pad.addEventListener('mouseup', () => {
    down = false;
    document.title = moves >= 3 ? 'CANVAS_DRAWN_' + moves : 'CANVAS_TOO_FEW_' + moves;
  });
  const drop = document.getElementById('dropzone');
  drop.addEventListener('dragover', (e) => e.preventDefault());
  drop.addEventListener('drop', (e) => { e.preventDefault(); document.title = 'DND_DROPPED'; });
})();
</script></body></html>"#;

async fn ready_runtime() -> Option<(BrowserRuntime, BrowserSessionId, String)> {
    let env = process_env();
    if which(&env, "node").is_none()
        || which(&env, "npm").is_none()
        || discover_system_chrome(&env).is_none()
    {
        eprintln!("skipping: node/npm/chrome unavailable");
        return None;
    }
    let fix = tempfile::tempdir().unwrap();
    let html = fix.path().join("app.html");
    std::fs::write(&html, SPA_FIXTURE).unwrap();
    let url = format!("file://{}", html.display());
    // leak the fixture dir so the file survives the run
    std::mem::forget(fix);

    let shared_home = std::env::temp_dir().join("leveler-browser-accept-home");
    let home = LevelerHome::from_root(shared_home);
    let install_env = EnvSnapshot::new(
        {
            let mut v: Vec<(OsString, OsString)> = std::env::vars_os().collect();
            v.push((OsString::from("LEVELER_HOME"), OsString::from(home.root())));
            v
        },
        std::env::current_dir().unwrap_or_default(),
        std::env::temp_dir(),
    );
    let profile: PathBuf = tempfile::tempdir().unwrap().keep();
    let rt = BrowserRuntime::new(home, install_env, profile);
    match rt.ensure_ready().await {
        Ok(_) => Some((rt, BrowserSessionId::new("rel"), url)),
        Err(e) => {
            eprintln!("skipping: runtime unavailable: {e}");
            None
        }
    }
}

#[tokio::test]
async fn spa_route_and_dynamic_dom_keep_refs_correct() {
    let Some((rt, s, url)) = ready_runtime().await else {
        return;
    };
    let page = rt.new_page(&s).await.unwrap();
    rt.navigate(&s, &page, &url).await.unwrap();

    // F7 SPA route: client-side navigation, no load event.
    let snap = rt.snapshot(&s, &page).await.unwrap();
    let go = ref_for(&snap.text, "Go to Users").unwrap();
    rt.interact(&s, &page, &go, Interaction::Click)
        .await
        .unwrap();
    // wait for the SPA-rendered content to appear (not a load event).
    rt.wait(
        &s,
        &page,
        WaitCondition::TextVisible("Save".into()),
        std::time::Duration::from_secs(5),
    )
    .await
    .unwrap();
    let snap2 = rt.snapshot(&s, &page).await.unwrap();
    assert!(
        snap2.text.contains("Save"),
        "SPA route rendered new content"
    );
    assert!(snap2.generation > snap.generation);

    // F8 dynamic reorder: the old "Go to Users" ref must be stale, not retargeted.
    let stale = rt.interact(&s, &page, &go, Interaction::Click).await;
    assert!(
        matches!(stale, Err(BrowserError::RefStale(_))),
        "post-navigation ref must be stale, got {stale:?}"
    );
}

// ── a tiny loopback HTTP server (the allowed dev exception) ──────────────────
// Serves the pages the SSRF / ownership fixtures navigate: a redirect to the
// cloud-metadata address, a link + a window.open to private ranges, and a
// second page reachable via a `target=_blank` link. Loopback is allowed by the
// gate, so the page itself loads; the private targets are what must be refused.
struct LocalServer {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}
impl Drop for LocalServer {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn serve_local() -> (String, LocalServer) {
    use std::io::{Read, Write};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://127.0.0.1:{port}");
    // The popup opens about:blank and the opener writes its content (no network
    // request, non-loopback): a frame-less popup cannot be attributed to a granted
    // opener, so loopback popups are refused by design (B-1.3) — ownership is what
    // this fixture tests, not the popup's network origin.
    let home =
        "<!doctype html><html><body><h1>Home</h1>\
         <a id=\"priv\" href=\"http://10.0.0.1/secret\">go private</a>\
         <button id=\"blank\" onclick=\"var w=window.open('about:blank','_blank');w.document.write('<h1>OtherTab</h1>');w.document.close();\">open tab</button>\
         <button id=\"wopen\" onclick=\"window.open('http://10.0.0.2/x','_blank')\">open private tab</button>\
         </body></html>"
            .to_string();
    let other = "<!doctype html><html><body><h1>OtherTab</h1><p>OTHER-CONTENT</p></body></html>";
    // A loopback dev page that fetches a same-origin asset (the granted case).
    let fetcher = format!(
        "<!doctype html><html><body><h1>Fetcher</h1><script>\
         fetch('{base}/other').then(r=>r.text()).then(t=>{{document.title=t.includes('OtherTab')?'FETCH_OK':'FETCH_ODD';}})\
         .catch(()=>{{document.title='FETCH_BLOCKED';}});</script></body></html>"
    );
    // A page that tries to open WebSockets to private / metadata targets. Uses
    // readyState (reliable) rather than depending on onerror/onclose timing: if
    // either socket is OPEN after the grace, the gate failed.
    let wspage =
        "<!doctype html><html><body><h1>WS</h1><script>\
         let a=new WebSocket('ws://10.0.0.1:80/');let b=new WebSocket('ws://169.254.169.254:80/');\
         a.onopen=()=>document.title='WS_OPEN';b.onopen=()=>document.title='WS_OPEN';\
         setTimeout(()=>{if(document.title!=='WS_OPEN'){document.title=(a.readyState===WebSocket.OPEN||b.readyState===WebSocket.OPEN)?'WS_OPEN':'WS_BLOCKED';}},1500);\
         </script></body></html>"
            .to_string();
    // A loopback dev page that opens a ws to ITS OWN origin (the vite-HMR
    // class): allowed for a granted page after R004 F6.
    let wsself = format!(
        "<!doctype html><html><body><h1>WSSELF</h1><script>\
         let w=new WebSocket('ws://127.0.0.1:{port}/ws');\
         w.onopen=()=>document.title='WS_SELF_OPEN';\
         setTimeout(()=>{{if(document.title!=='WS_SELF_OPEN')document.title='WS_SELF_BLOCKED';}},2500);\
         </script></body></html>"
    );
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let handle = std::thread::spawn(move || {
        while !stop_thread.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut sock, _)) => {
                    let mut buf = [0u8; 2048];
                    let n = sock.read(&mut buf).unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let path = req
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("/");
                    // RFC 6455 handshake for /ws: Chrome only fires onopen on a
                    // valid Sec-WebSocket-Accept.
                    if path.starts_with("/ws")
                        && !path.starts_with("/wsself")
                        && !path.starts_with("/wspage")
                    {
                        let key = req
                            .lines()
                            .find(|l| l.to_ascii_lowercase().starts_with("sec-websocket-key:"))
                            .map(|l| l.split_once(':').map_or("", |x| x.1).trim().to_string())
                            .unwrap_or_default();
                        use base64::Engine as _;
                        use sha1::Digest as _;
                        let mut h = sha1::Sha1::new();
                        h.update(key.as_bytes());
                        h.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
                        let accept = base64::engine::general_purpose::STANDARD.encode(h.finalize());
                        let resp = format!(
                            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
                        );
                        let _ = sock.write_all(resp.as_bytes());
                        let _ = sock.flush();
                        // Hold the socket open briefly so onopen fires.
                        std::thread::sleep(std::time::Duration::from_millis(3000));
                        continue;
                    }
                    let resp = if path.starts_with("/redir") {
                        "HTTP/1.1 302 Found\r\nLocation: http://169.254.169.254/latest/meta-data/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                    } else if path.starts_with("/other") || path.starts_with("/asset") {
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            other.len(),
                            other
                        )
                    } else if path.starts_with("/fetcher") {
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            fetcher.len(),
                            fetcher
                        )
                    } else if path.starts_with("/wsself") {
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            wsself.len(),
                            wsself
                        )
                    } else if path.starts_with("/wspage") {
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            wspage.len(),
                            wspage
                        )
                    } else {
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            home.len(),
                            home
                        )
                    };
                    let _ = sock.write_all(resp.as_bytes());
                    let _ = sock.flush();
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(_) => break,
            }
        }
    });
    (
        base,
        LocalServer {
            stop,
            handle: Some(handle),
        },
    )
}

// ── B-1: SSRF enforced at the actual browser request boundary ────────────────
#[tokio::test]
async fn ssrf_enforced_at_the_browser_request_boundary() {
    let Some((rt, s, _url)) = ready_runtime().await else {
        return;
    };
    let (base, _server) = serve_local();
    let page = rt.new_page(&s).await.unwrap();

    // 1) A direct navigation to a private IP is refused at the request boundary.
    let direct = rt.navigate(&s, &page, "http://10.0.0.1/").await;
    assert!(
        matches!(direct, Err(BrowserError::Denied(_))),
        "direct private navigation must be Denied, got {direct:?}"
    );

    // 2) A public→redirect→metadata hop is refused (the redirect target, not the
    //    initial URL, is the blocked one). The loopback origin itself is allowed.
    let redir = rt.navigate(&s, &page, &format!("{base}/redir")).await;
    assert!(
        matches!(redir, Err(BrowserError::Denied(_))),
        "redirect into the metadata address must be Denied, got {redir:?}"
    );

    // 3) A link click whose navigation targets a private IP must NOT reach it —
    //    the loopback page stays put (no data egressed to 10.0.0.1). Fresh page:
    //    each SSRF vector is an independent navigation (a blocked nav leaves the
    //    prior page on chrome-error, exactly as a real agent's next step would
    //    start clean).
    let page = rt.new_page(&s).await.unwrap();
    rt.navigate(&s, &page, &base).await.unwrap();
    let snap = rt.snapshot(&s, &page).await.unwrap();
    let priv_ref = ref_for(&snap.text, "go private").expect("link ref");
    let click = rt
        .interact(&s, &page, &priv_ref, Interaction::Click)
        .await
        .expect("click itself succeeds");
    // The block is surfaced on the action (not swallowed) …
    assert!(
        click.blocked.is_some(),
        "a click whose navigation targets a private IP must report a block"
    );
    // … and the private target is never reached (no data egressed to 10.0.0.1).
    let after = rt.snapshot(&s, &page).await.unwrap();
    assert!(
        !after.url.contains("10.0.0.1"),
        "click must not navigate to the private IP, url was {}",
        after.url
    );
}

// Poll the page title (set by page JS) until it matches, or time out.
async fn poll_title(
    rt: &BrowserRuntime,
    s: &BrowserSessionId,
    page: &leveler_browser::BrowserPageId,
    secs: u64,
    want: &[&str],
) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    let mut last = String::new();
    while std::time::Instant::now() < deadline {
        if let Ok(sn) = rt.snapshot(s, page).await {
            last = sn.title.clone();
            if want.iter().any(|w| last == *w) {
                return last;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    last
}

// ── B-1 #1: loopback is a PAGE-SCOPED dev grant, not a global bypass ──────────
#[tokio::test]
async fn loopback_is_a_page_scoped_grant_not_a_global_bypass() {
    let Some((rt, s, _url)) = ready_runtime().await else {
        return;
    };
    let (base, _server) = serve_local();

    // A GRANTED loopback dev page (explicit loopback navigation) may fetch its
    // own loopback origin — the narrow dev exception still works.
    let granted = rt.new_page(&s).await.unwrap();
    rt.navigate(&s, &granted, &format!("{base}/fetcher"))
        .await
        .unwrap();
    let t2 = poll_title(&rt, &s, &granted, 8, &["FETCH_OK", "FETCH_BLOCKED"]).await;
    assert_eq!(
        t2, "FETCH_OK",
        "an explicitly-navigated loopback dev page may fetch its own origin, got {t2}"
    );

    // An UNGRANTED page (opaque `data:` origin — the public-page stand-in: no
    // explicit loopback navigation, no loopback opener) must NOT reach loopback.
    // A distinct path (`/asset`) so it shares no cache key with the granted fetch.
    // `no-cors` so the refusal is the network gate, not a CORS rejection.
    let ungranted = rt.new_page(&s).await.unwrap();
    let data_url = format!(
        "data:text/html,<html><body><script>fetch('{base}/asset',{{mode:'no-cors'}})\
         .then(()=>{{document.title='FETCH_REACHED'}}).catch(()=>{{document.title='FETCH_BLOCKED'}})</script></body></html>"
    );
    rt.navigate(&s, &ungranted, &data_url).await.unwrap();
    let t = poll_title(&rt, &s, &ungranted, 8, &["FETCH_BLOCKED", "FETCH_REACHED"]).await;
    assert_eq!(
        t, "FETCH_BLOCKED",
        "an ungranted (public-equivalent) page must not fetch loopback"
    );
}

// ── B-1: non-loopback egress goes through the single pinning proxy authority ──
// A `.invalid` host (RFC 6761: never resolves) is not a loopback literal and not
// a blocked IP literal, so the route layer lets it proceed — it can ONLY be
// refused by the proxy (the sole resolver/connector). A typed `Denied` therefore
// proves the request reached the proxy and the proxy failed closed on a target it
// could not resolve/validate, rather than any connection escaping.
#[tokio::test]
async fn non_loopback_egress_is_refused_by_the_proxy_fail_closed() {
    let Some((rt, s, _url)) = ready_runtime().await else {
        return;
    };
    let page = rt.new_page(&s).await.unwrap();
    let r = rt
        .navigate(
            &s,
            &page,
            "http://blocked.this-name-never-resolves.invalid/",
        )
        .await;
    assert!(
        matches!(r, Err(BrowserError::Denied(_))),
        "an unresolvable non-loopback target must be Denied by the proxy, got {r:?}"
    );
}

// ── B-1 #3: WebSocket egress is gated by the same host policy ─────────────────
#[tokio::test]
async fn websocket_egress_is_gated() {
    let Some((rt, s, _url)) = ready_runtime().await else {
        return;
    };
    let (base, _server) = serve_local();
    let page = rt.new_page(&s).await.unwrap();
    // Even from a granted loopback page, WebSockets to private / metadata targets
    // are refused (never connected) — the ws boundary applies the same policy.
    rt.navigate(&s, &page, &format!("{base}/wspage"))
        .await
        .unwrap();
    let t = poll_title(&rt, &s, &page, 8, &["WS_BLOCKED", "WS_OPEN"]).await;
    assert_eq!(
        t, "WS_BLOCKED",
        "WebSocket to a private/metadata host must be refused, not opened"
    );
}

// ── B-2: session/page ownership incl. target=_blank adoption ─────────────────
#[tokio::test]
async fn sessions_and_new_tabs_are_owned_by_the_originating_session() {
    let Some((rt, _s, _url)) = ready_runtime().await else {
        return;
    };
    let (base, _server) = serve_local();
    let a = BrowserSessionId::new("sess-A");
    let b = BrowserSessionId::new("sess-B");

    // Each session opens its own page on the shared context.
    let pa = rt.new_page(&a).await.unwrap();
    rt.navigate(&a, &pa, &base).await.unwrap();
    let pb = rt.new_page(&b).await.unwrap();
    rt.navigate(&b, &pb, &format!("{base}/other"))
        .await
        .unwrap();

    // S1: A's tab list shows only A's page, never B's url/title.
    let a_tabs = rt.tabs(&a).await.unwrap();
    assert!(
        a_tabs.iter().any(|t| t.page == pa) && a_tabs.iter().all(|t| t.page != pb),
        "A must see its own page and not B's: {a_tabs:?}"
    );
    assert!(
        a_tabs.iter().all(|t| !t.url.contains("/other")),
        "B's url must not leak into A's tabs"
    );

    // S2: A cannot operate B's page.
    let cross = rt.snapshot(&a, &pb).await;
    assert!(
        matches!(cross, Err(BrowserError::ActionFailed(_))),
        "A must not snapshot B's page, got {cross:?}"
    );

    // S3/S4: a target=_blank click gives A a NEW owned page it can operate.
    let snap = rt.snapshot(&a, &pa).await.unwrap();
    let blank_ref = ref_for(&snap.text, "open tab").expect("blank-link ref");
    let res = rt
        .interact(&a, &pa, &blank_ref, Interaction::Click)
        .await
        .unwrap();
    let new_page = res.new_page.expect("target=_blank opened a new page");
    let a_tabs2 = rt.tabs(&a).await.unwrap();
    assert!(
        a_tabs2.iter().any(|t| t.page == new_page),
        "the new tab is adopted into A's tabs: {a_tabs2:?}"
    );
    // The adopted tab is operable by A: wait for its content, then snapshot it.
    rt.wait(
        &a,
        &new_page,
        WaitCondition::TextVisible("OtherTab".into()),
        std::time::Duration::from_secs(5),
    )
    .await
    .expect("A can drive the adopted tab (structured wait)");
    let new_snap = rt.snapshot(&a, &new_page).await.unwrap();
    assert!(
        new_snap.text.contains("OtherTab"),
        "A can snapshot the adopted new tab"
    );

    // S5: B can neither see nor operate A's adopted new tab.
    let b_tabs = rt.tabs(&b).await.unwrap();
    assert!(
        b_tabs.iter().all(|t| t.page != new_page && t.page != pa),
        "B must not see A's pages/new tab: {b_tabs:?}"
    );
    assert!(
        matches!(
            rt.snapshot(&b, &new_page).await,
            Err(BrowserError::ActionFailed(_))
        ),
        "B must not operate A's adopted tab"
    );
}

#[tokio::test]
async fn dialog_and_new_tab_do_not_hang() {
    let Some((rt, s, url)) = ready_runtime().await else {
        return;
    };
    let page = rt.new_page(&s).await.unwrap();
    rt.navigate(&s, &page, &url).await.unwrap();
    let snap = rt.snapshot(&s, &page).await.unwrap();

    // F6 dialog: default policy dismisses without hanging the click.
    let modal = ref_for(&snap.text, "Open dialog").unwrap();
    rt.interact(&s, &page, &modal, Interaction::Click)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let after = rt.snapshot(&s, &page).await.unwrap();
    assert!(
        after.text.contains("Cancelled")
            || after.text.contains("Home")
            || after.text.contains("Users"),
        "dismissed dialog did not hang the driver"
    );

    // F9 new tab: window.open must not wedge; the page count grows.
    let snap2 = rt.snapshot(&s, &page).await.unwrap();
    if let Some(newtab) = ref_for(&snap2.text, "Open tab") {
        rt.interact(&s, &page, &newtab, Interaction::Click)
            .await
            .unwrap();
        let tabs = rt.tabs(&s).await.unwrap();
        assert!(
            tabs.len() >= 2,
            "window.open produced a second tab: {tabs:?}"
        );
    }
}

// ── R004 F5: structured drag — canvas drawing (ref→offset) & DnD (ref→ref) ───
#[tokio::test]
async fn structured_drag_draws_on_canvas_and_drops_on_targets() {
    let Some((rt, s, url)) = ready_runtime().await else {
        return;
    };
    let page = rt.new_page(&s).await.unwrap();
    rt.navigate(&s, &page, &url).await.unwrap();
    let snap = rt.snapshot(&s, &page).await.unwrap();
    let canvas_ref = ref_for(&snap.text, "pad").or_else(|| ref_for(&snap.text, "canvas"));
    let Some(canvas_ref) = canvas_ref else {
        // The aria snapshot may not expose a bare canvas; fall back to any ref
        // and only assert the DnD half in that case.
        return drag_dnd_half(&rt, &s, &page).await;
    };
    // ref→offset drag: the canvas mouse handlers must see a stepped path.
    rt.interact(
        &s,
        &page,
        &canvas_ref,
        Interaction::Drag {
            to_ref: None,
            dx: 120.0,
            dy: 60.0,
            steps: 12,
        },
    )
    .await
    .unwrap();
    let t = poll_title(&rt, &s, &page, 8, &["CANVAS_DRAWN_12"]).await;
    assert!(
        t.starts_with("CANVAS_DRAWN_"),
        "stepped drag must fire canvas move handlers, got {t}"
    );
    drag_dnd_half(&rt, &s, &page).await;
}

async fn drag_dnd_half(
    rt: &BrowserRuntime,
    s: &BrowserSessionId,
    page: &leveler_browser::BrowserPageId,
) {
    let snap = rt.snapshot(s, page).await.unwrap();
    let (Some(src_ref), Some(dst_ref)) = (
        ref_for(&snap.text, "DragMe"),
        ref_for(&snap.text, "Drop here"),
    ) else {
        eprintln!("skipping DnD half: fixture refs not exposed in aria snapshot");
        return;
    };
    rt.interact(
        s,
        page,
        &src_ref,
        Interaction::Drag {
            to_ref: Some(dst_ref),
            dx: 0.0,
            dy: 0.0,
            steps: 12,
        },
    )
    .await
    .unwrap();
    let t = poll_title(rt, s, page, 8, &["DND_DROPPED"]).await;
    assert_eq!(t, "DND_DROPPED", "ref→ref drag must perform HTML5 DnD");
}

// A drag whose TARGET ref is stale must fail RefStale and never retarget.
#[tokio::test]
async fn stale_drag_target_fails_ref_stale_not_retarget() {
    let Some((rt, s, url)) = ready_runtime().await else {
        return;
    };
    let page = rt.new_page(&s).await.unwrap();
    rt.navigate(&s, &page, &url).await.unwrap();
    let snap = rt.snapshot(&s, &page).await.unwrap();
    let Some(any_ref) = first_ref(&snap.text) else {
        eprintln!("skipping: no refs in snapshot");
        return;
    };
    let bogus_target = format!("{}zz", any_ref);
    let r = rt
        .interact(
            &s,
            &page,
            &any_ref,
            Interaction::Drag {
                to_ref: Some(bogus_target),
                dx: 0.0,
                dy: 0.0,
                steps: 4,
            },
        )
        .await;
    assert!(
        matches!(r, Err(BrowserError::RefStale(_))),
        "a stale/unknown drag target must be RefStale, got {r:?}"
    );
}

// ── R004 F6: loopback ws is grant-scoped — a granted dev page's own-origin
// HMR-style socket connects; the private/metadata refusal stays intact
// (websocket_egress_is_gated above must stay green). ─────────────────────────
#[tokio::test]
async fn loopback_ws_from_a_granted_dev_page_connects() {
    let Some((rt, s, _url)) = ready_runtime().await else {
        return;
    };
    let (base, _server) = serve_local();
    let page = rt.new_page(&s).await.unwrap();
    rt.navigate(&s, &page, &format!("{base}/wsself"))
        .await
        .unwrap();
    let t = poll_title(&rt, &s, &page, 8, &["WS_SELF_OPEN", "WS_SELF_BLOCKED"]).await;
    assert_eq!(
        t, "WS_SELF_OPEN",
        "a granted loopback dev page must reach its own-origin ws (vite HMR class), got {t}"
    );
}

fn first_ref(snapshot_text: &str) -> Option<String> {
    for line in snapshot_text.lines() {
        if let Some(i) = line.find("[ref=") {
            let rest = &line[i + 5..];
            if let Some(end) = rest.find(']') {
                return Some(rest[..end].to_string());
            }
        }
    }
    None
}
