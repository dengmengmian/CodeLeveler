//! Live browser acceptance, against a REAL browser.
//!
//! Opt-in by design (§42): a plain `cargo test --workspace` must never be able
//! to imply that browser acceptance passed. Set the product to drive and run
//! the suite explicitly:
//!
//! ```text
//! LEVELER_BROWSER_ACCEPTANCE=chrome  cargo test -p leveler-browser --test acceptance -- --test-threads=1
//! LEVELER_BROWSER_ACCEPTANCE=safari  cargo test -p leveler-browser --test acceptance -- --test-threads=1
//! ```
//!
//! Unset, every test here reports SKIP and does nothing. A skip is never a
//! pass: the acceptance report records it as SKIP with the reason.
//!
//! Each test owns a unique temp profile and an ephemeral fixture port, so
//! nothing is shared between tests or between runs (§41).

mod support;

use std::collections::HashMap;
use std::path::PathBuf;

use leveler_browser::{Act, Browser, BrowserProduct, BrowserSessionId, InspectKind, InspectReport};
use support::{LocalServer, ref_for};

/// The product this run drives, or `None` when acceptance was not requested.
fn requested() -> Option<BrowserProduct> {
    let raw = std::env::var("LEVELER_BROWSER_ACCEPTANCE").ok()?;
    match BrowserProduct::parse(&raw) {
        Some(p) => Some(p),
        None => panic!("LEVELER_BROWSER_ACCEPTANCE={raw} is not a browser CodeLeveler drives"),
    }
}

/// A browser handle over a private profile, or a printed SKIP.
fn browser(test: &str) -> Option<(Browser, BrowserProduct, PathBuf)> {
    let Some(product) = requested() else {
        println!("SKIP {test}: LEVELER_BROWSER_ACCEPTANCE is unset");
        return None;
    };
    let env = leveler_core::environment().clone();
    if let Err(e) = leveler_browser::availability(&env, product) {
        println!("SKIP {test}: {product} is not drivable here — {e}");
        return None;
    }
    let profile = std::env::temp_dir().join(format!(
        "leveler-browser-acceptance-{}-{}",
        std::process::id(),
        test
    ));
    let _ = std::fs::remove_dir_all(&profile);
    // The product is passed EXPLICITLY on every navigate, so this suite tests
    // the product it says it tests and never inherits a system default.
    Some((Browser::new(env, profile.clone(), None), product, profile))
}

fn session() -> BrowserSessionId {
    BrowserSessionId::new("acceptance")
}

/// The first navigate of a test, which is also where a browser is started.
///
/// A prerequisite only the user can satisfy — Safari's Remote Automation
/// toggle — surfaces here rather than from `availability`, so this is where a
/// run turns into a SKIP. `false` means the test printed SKIP and must return;
/// anything else is a real failure and panics.
async fn started(
    b: &Browser,
    s: &BrowserSessionId,
    p: BrowserProduct,
    url: &str,
    test: &str,
) -> bool {
    match b.navigate(s, Some(p), url).await {
        Ok(_) => true,
        Err(leveler_browser::BrowserError::Unavailable(why)) => {
            println!("SKIP {test}: {why}");
            false
        }
        Err(e) => panic!("{test}: navigate failed: {e}"),
    }
}

fn page(body: &str) -> String {
    format!("<!doctype html><html><head><title>Fixture</title></head><body>{body}</body></html>")
}

fn routes(pairs: &[(&str, String)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

// ── B1: interaction ─────────────────────────────────────────────────────────

#[tokio::test]
async fn b1_click_changes_the_page() {
    let Some((b, product, profile)) = browser("b1") else {
        return;
    };
    let server = LocalServer::start(routes(&[(
        "/",
        page(
            r#"<h1>Counter</h1><div id="out">idle</div>
               <button onclick="document.getElementById('out').textContent='clicked'">Run</button>"#,
        ),
    )]));
    let s = session();

    if !started(&b, &s, product, &server.base, "b1").await {
        return;
    }
    let before = b.snapshot(&s, None).await.expect("snapshot");
    assert!(
        before.text.contains("idle"),
        "fixture text:\n{}",
        before.text
    );
    let r = ref_for(&before.text, "button \"Run\"").expect("a ref for the button");

    b.act(&s, None, Act::Click { r#ref: &r })
        .await
        .expect("click");

    let after = b.snapshot(&s, None).await.expect("snapshot");
    assert!(
        after.text.contains("clicked"),
        "DOM did not change:\n{}",
        after.text
    );
    assert!(
        after.generation > before.generation,
        "every snapshot mints a new generation"
    );
    println!("PASS b1 {product}");
    b.shutdown().await;
    let _ = std::fs::remove_dir_all(&profile);
}

// ── B2: form ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn b2_fill_and_submit_a_form() {
    let Some((b, product, profile)) = browser("b2") else {
        return;
    };
    let server = LocalServer::start(routes(&[(
        "/",
        page(
            r#"<h1>Sign in</h1>
               <label for="u">Username</label><input id="u">
               <select id="role"><option>reader</option><option>admin</option></select>
               <div id="out">empty</div>
               <button onclick="document.getElementById('out').textContent =
                 'hello ' + document.getElementById('u').value + ' as ' +
                 document.getElementById('role').value">Save</button>"#,
        ),
    )]));
    let s = session();

    if !started(&b, &s, product, &server.base, "b2").await {
        return;
    }
    let snap = b.snapshot(&s, None).await.expect("snapshot");
    let field = ref_for(&snap.text, "textbox").expect("a ref for the textbox");
    let role = ref_for(&snap.text, "combobox").expect("a ref for the select");

    b.act(
        &s,
        None,
        Act::Fill {
            r#ref: &field,
            text: "ada",
        },
    )
    .await
    .expect("fill");
    b.act(
        &s,
        None,
        Act::Select {
            r#ref: &role,
            values: &["admin".to_string()],
        },
    )
    .await
    .expect("select");

    // The refs above came from `snap`; acting does not supersede them, so the
    // button ref from the same snapshot is still the current generation.
    let button = ref_for(&snap.text, "button \"Save\"").expect("a ref for the button");
    b.act(&s, None, Act::Click { r#ref: &button })
        .await
        .expect("click");

    let after = b.snapshot(&s, None).await.expect("snapshot");
    assert!(
        after.text.contains("hello ada as admin"),
        "form did not submit what was entered:\n{}",
        after.text
    );
    println!("PASS b2 {product}");
    b.shutdown().await;
    let _ = std::fs::remove_dir_all(&profile);
}

// ── B3: navigation + state ──────────────────────────────────────────────────

#[tokio::test]
async fn b3_state_survives_navigation() {
    let Some((b, product, profile)) = browser("b3") else {
        return;
    };
    let server = LocalServer::start(routes(&[
        (
            "/a",
            page(r#"<h1>A</h1><script>localStorage.setItem('token','pear')</script>"#),
        ),
        (
            "/b",
            page(
                r#"<h1>B</h1><div id="out">none</div>
                   <script>document.getElementById('out').textContent =
                     'token=' + (localStorage.getItem('token') || 'none')</script>"#,
            ),
        ),
    ]));
    let s = session();

    let page_a = format!("{}/a", server.base);
    if !started(&b, &s, product, &page_a, "b3").await {
        return;
    }
    let a = b.snapshot(&s, None).await.expect("snapshot A");
    let stale = ref_for(&a.text, "heading").expect("a ref on page A");

    let outcome = b
        .navigate(&s, Some(product), &format!("{}/b", server.base))
        .await
        .expect("navigate B");
    assert!(outcome.url.ends_with("/b"), "url: {}", outcome.url);

    let after = b.snapshot(&s, None).await.expect("snapshot B");
    assert!(
        after.text.contains("token=pear"),
        "state did not survive the navigation:\n{}",
        after.text
    );

    // A ref minted on page A must be mechanically stale on page B, never
    // retargeted onto B's similar-looking heading.
    let err = b
        .act(&s, None, Act::Click { r#ref: &stale })
        .await
        .expect_err("a ref from the previous page must not resolve");
    assert!(
        err.to_string().contains("stale"),
        "expected a stale ref, got: {err}"
    );
    println!("PASS b3 {product}");
    b.shutdown().await;
    let _ = std::fs::remove_dir_all(&profile);
}

// ── network: localhost and the public internet are both reachable ───────────

#[tokio::test]
async fn localhost_is_reachable() {
    let Some((b, product, profile)) = browser("localhost") else {
        return;
    };
    let server = LocalServer::start(routes(&[("/", page("<h1>dev server</h1>"))]));
    let s = session();
    if !started(&b, &s, product, &server.base, "localhost").await {
        return;
    }
    let snap = b.snapshot(&s, None).await.expect("snapshot");
    assert!(snap.text.contains("dev server"), "{}", snap.text);
    println!("PASS localhost {product}");
    b.shutdown().await;
    let _ = std::fs::remove_dir_all(&profile);
}

/// The browser is a network-authorised capability: the public internet is
/// reachable, with no per-request gate to satisfy.
#[tokio::test]
async fn the_public_internet_is_reachable() {
    let Some((b, product, profile)) = browser("public") else {
        return;
    };
    let s = session();
    let outcome = match b.navigate(&s, Some(product), "https://example.com/").await {
        Ok(o) => o,
        Err(leveler_browser::BrowserError::Unavailable(why)) => {
            println!("SKIP public: {why}");
            return;
        }
        Err(e) => panic!("the public internet must be reachable: {e}"),
    };
    assert!(
        outcome.url.starts_with("https://example.com"),
        "{}",
        outcome.url
    );
    let snap = b.snapshot(&s, None).await.expect("snapshot");
    assert!(
        snap.text.to_lowercase().contains("example domain"),
        "the page did not load:\n{}",
        snap.text
    );
    println!("PASS public-internet {product}");
    b.shutdown().await;
    let _ = std::fs::remove_dir_all(&profile);
}

// ── tabs ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn tabs_open_list_and_close() {
    let Some((b, product, profile)) = browser("tabs") else {
        return;
    };
    let server = LocalServer::start(routes(&[("/", page("<h1>one</h1>"))]));
    let s = session();
    if !started(&b, &s, product, &server.base, "tabs").await {
        return;
    }

    let second = b.new_tab(&s).await.expect("new tab");
    b.navigate(&s, Some(product), &server.base)
        .await
        .expect("navigate 2");
    let listed = b.tabs(&s).await.expect("tabs");
    assert!(listed.len() >= 2, "expected two tabs, got {listed:?}");

    b.close_tab(&s, &second).await.expect("close");
    let after = b.tabs(&s).await.expect("tabs");
    assert!(
        !after.iter().any(|t| t.tab == second),
        "the closed tab is still listed: {after:?}"
    );
    println!("PASS tabs {product}");
    b.shutdown().await;
    let _ = std::fs::remove_dir_all(&profile);
}

// ── screenshot ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn screenshot_returns_a_png() {
    let Some((b, product, profile)) = browser("screenshot") else {
        return;
    };
    let server = LocalServer::start(routes(&[("/", page("<h1>shot</h1>"))]));
    let s = session();
    if !started(&b, &s, product, &server.base, "screenshot").await {
        return;
    }
    let b64 = b.screenshot(&s, None).await.expect("screenshot");
    let bytes = base64_decode(&b64).expect("valid base64");
    assert_eq!(&bytes[..4], b"\x89PNG", "not a PNG");
    println!("PASS screenshot {product}");
    b.shutdown().await;
    let _ = std::fs::remove_dir_all(&profile);
}

// ── inspect: real where the protocol has it, explicit where it does not ─────

#[tokio::test]
async fn inspect_reports_what_the_backend_can_actually_see() {
    let Some((b, product, profile)) = browser("inspect") else {
        return;
    };
    let server = LocalServer::start(routes(&[(
        "/",
        page(
            r#"<h1>diag</h1>
               <script>
                 console.error('boom from console');
                 fetch('/missing-endpoint').catch(function(){});
                 setTimeout(function(){ throw new Error('uncaught page error') }, 0);
               </script>"#,
        ),
    )]));
    let s = session();
    if !started(&b, &s, product, &server.base, "inspect").await {
        return;
    }
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    for kind in [
        InspectKind::Console,
        InspectKind::PageErrors,
        InspectKind::Network,
    ] {
        let out = b.inspect(&s, None, kind).await;
        match (product, out) {
            // WebDriver has no observation channel at all; the answer must say
            // so rather than return an empty list.
            (BrowserProduct::Safari, Err(e)) => {
                assert!(
                    e.to_string().contains("does not support"),
                    "safari {kind:?} must be explicitly unsupported, got: {e}"
                );
                println!("UNSUPPORTED inspect/{} safari — {e}", kind.as_str());
            }
            (BrowserProduct::Safari, Ok(_)) => {
                panic!("safari cannot observe {kind:?} and must not claim to")
            }
            (_, Ok(report)) => {
                let found = match (&kind, &report) {
                    (InspectKind::Console, InspectReport::Console(e)) => {
                        e.iter().any(|c| c.text.contains("boom from console"))
                    }
                    (InspectKind::PageErrors, InspectReport::Console(e)) => {
                        e.iter().any(|c| c.text.contains("uncaught page error"))
                    }
                    (InspectKind::Network, InspectReport::Network(n)) => {
                        n.iter().any(|r| r.url.contains("missing-endpoint"))
                    }
                    _ => false,
                };
                assert!(found, "{product} {kind:?} saw nothing: {report:?}");
                println!("PASS inspect/{} {product}", kind.as_str());
            }
            (_, Err(e)) => panic!("{product} {kind:?} failed: {e}"),
        }
    }
    b.shutdown().await;
    let _ = std::fs::remove_dir_all(&profile);
}

// ── lifecycle: cancellation, disconnect, cleanup ────────────────────────────

/// A cancelled call ends when the token fires, not when the deadline expires,
/// and it does NOT take the session down with it (§23).
#[tokio::test]
async fn cancelling_an_operation_ends_it_without_ending_the_session() {
    let Some((b, product, profile)) = browser("cancel") else {
        return;
    };
    // A route that never answers: only cancellation can end this navigate.
    let hang = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = hang.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in hang.incoming() {
            // Accept and hold: the browser waits for a response that never comes.
            std::mem::forget(stream);
        }
    });
    let server = LocalServer::start(routes(&[("/", page("<h1>alive</h1>"))]));
    let s = session();
    if !started(&b, &s, product, &server.base, "cancel").await {
        return;
    }

    let cancel = tokio_util::sync::CancellationToken::new();
    let token = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        token.cancel();
    });

    let hanging = format!("http://127.0.0.1:{port}/");
    let started = std::time::Instant::now();
    let outcome = tokio::select! {
        r = b.navigate(&s, Some(product), &hanging) => Some(r),
        _ = cancel.cancelled() => None,
    };
    let elapsed = started.elapsed();
    assert!(
        outcome.is_none(),
        "the hanging navigate should not have finished"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "cancellation waited for a timeout instead of the token: {elapsed:?}"
    );

    // The session survives: a cancelled operation is not a dead browser.
    //
    // How SOON it is reusable is a protocol difference, not a defect. CDP is
    // multiplexed, so the next call goes straight through. WebDriver has no
    // cancel: the abandoned command keeps running on the server and the
    // session accepts nothing else until the page-load timeout ends it. Both
    // recover; only one recovers instantly, so this polls and reports which.
    let recovery = std::time::Instant::now();
    let deadline = recovery + std::time::Duration::from_secs(60);
    loop {
        match b.navigate(&s, Some(product), &server.base).await {
            Ok(_) => break,
            Err(e) if std::time::Instant::now() < deadline => {
                let _ = e;
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            Err(e) => panic!("the session never became usable after a cancel: {e}"),
        }
    }
    let recovered_in = recovery.elapsed();
    let snap = b.snapshot(&s, None).await.expect("snapshot");
    assert!(snap.text.contains("alive"), "{}", snap.text);
    println!(
        "PASS cancellation {product} (call ended in {elapsed:?}, session reusable after {recovered_in:?})"
    );
    b.shutdown().await;
    let _ = std::fs::remove_dir_all(&profile);
}

/// Killing the browser must produce a disconnected status and a disconnected
/// error — never a Live status over a dead connection (§24).
#[tokio::test]
async fn killing_the_browser_is_reported_as_a_disconnect() {
    let Some((b, product, profile)) = browser("crash") else {
        return;
    };
    let server = LocalServer::start(routes(&[("/", page("<h1>alive</h1>"))]));
    let s = session();
    if !started(&b, &s, product, &server.base, "crash").await {
        return;
    }
    assert!(matches!(
        b.status().await,
        leveler_browser::BrowserStatus::Live { .. }
    ));

    kill_browser_processes(product, &profile);
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

    let err = b
        .snapshot(&s, None)
        .await
        .expect_err("a killed browser must not answer a snapshot");
    println!("after kill: {err}");
    assert!(
        matches!(
            b.status().await,
            leveler_browser::BrowserStatus::Disconnected { .. }
        ),
        "status stayed Live over a dead connection: {:?}",
        b.status().await
    );

    // A navigate is the one operation that starts a fresh session.
    if !started(&b, &s, product, &server.base, "cleanup").await {
        return;
    }
    assert!(matches!(
        b.status().await,
        leveler_browser::BrowserStatus::Live { .. }
    ));
    println!("PASS crash-lifecycle {product}");
    b.shutdown().await;
    let _ = std::fs::remove_dir_all(&profile);
}

/// After shutdown no browser process of ours is left behind (§25).
#[tokio::test]
async fn shutdown_leaves_no_process_behind() {
    let Some((b, product, profile)) = browser("cleanup") else {
        return;
    };
    let server = LocalServer::start(routes(&[("/", page("<h1>bye</h1>"))]));
    let s = session();
    if !started(&b, &s, product, &server.base, "cleanup").await {
        return;
    }
    let before = count_browser_processes(product, &profile);
    assert!(before > 0, "expected a running browser to count");

    b.shutdown().await;
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    let after = count_browser_processes(product, &profile);
    assert_eq!(after, 0, "{after} browser processes outlived the session");
    println!("PASS process-cleanup {product} ({before} → 0)");
    let _ = std::fs::remove_dir_all(&profile);
}

// ── helpers ─────────────────────────────────────────────────────────────────

/// How this test recognises the processes it started.
///
/// A CDP product is identified by the private profile directory it was given,
/// so a browser the developer happens to have open is never counted. Safari
/// has no such thing — WebDriver drives the installed Safari under the user's
/// own profile — so its marker is `safaridriver` itself. That asymmetry is the
/// protocol's, and the test states it rather than papering over it.
fn process_marker(product: BrowserProduct, profile: &std::path::Path) -> String {
    match product {
        BrowserProduct::Safari => "safaridriver".to_string(),
        _ => profile.display().to_string(),
    }
}

fn count_browser_processes(product: BrowserProduct, profile: &std::path::Path) -> usize {
    let needle = process_marker(product, profile);
    let out = std::process::Command::new("ps")
        .args(["-Ao", "args="])
        .output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter(|l| l.contains(&needle))
            .count(),
        Err(_) => 0,
    }
}

fn kill_browser_processes(product: BrowserProduct, profile: &std::path::Path) {
    let _ = std::process::Command::new("pkill")
        .args(["-9", "-f", &process_marker(product, profile)])
        .status();
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let mut acc: u32 = 0;
    let mut bits = 0;
    for c in s.bytes().filter(|c| !c.is_ascii_whitespace() && *c != b'=') {
        let v = T.iter().position(|t| *t == c)? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}
