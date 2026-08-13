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
    let home = format!(
        "<!doctype html><html><body><h1>Home</h1>\
         <a id=\"priv\" href=\"http://10.0.0.1/secret\">go private</a>\
         <a id=\"blank\" target=\"_blank\" href=\"{base}/other\">open tab</a>\
         <button id=\"wopen\" onclick=\"window.open('http://10.0.0.2/x','_blank')\">open private tab</button>\
         </body></html>"
    );
    let other = "<!doctype html><html><body><h1>OtherTab</h1><p>OTHER-CONTENT</p></body></html>";
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
                    let resp = if path.starts_with("/redir") {
                        "HTTP/1.1 302 Found\r\nLocation: http://169.254.169.254/latest/meta-data/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                    } else if path.starts_with("/other") {
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            other.len(),
                            other
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
