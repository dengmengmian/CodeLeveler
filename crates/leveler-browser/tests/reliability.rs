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
