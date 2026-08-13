//! Real end-to-end acceptance of the browser runtime against system Chrome.
//!
//! Gated: needs Node + npm + a system browser, and installs the pinned
//! Playwright into a SHARED cache home on first run (so later runs are fast).
//! When the environment can't support it (no Node/Chrome, or offline install),
//! the test self-skips with a message rather than failing — CI without a
//! browser stays green; a dev/CI machine with Chrome exercises the full stack.

use std::ffi::OsString;
use std::path::PathBuf;

use leveler_browser::{BrowserError, BrowserRuntime, BrowserSessionId, Interaction, which};
use leveler_core::{EnvSnapshot, LevelerHome};

fn process_env() -> EnvSnapshot {
    EnvSnapshot::new(
        std::env::vars_os().collect::<Vec<_>>(),
        std::env::current_dir().unwrap_or_default(),
        std::env::temp_dir(),
    )
}

/// Extract the `[ref=…]` token for the line whose text contains `needle`.
fn ref_for(snapshot_text: &str, needle: &str) -> Option<String> {
    let line = snapshot_text.lines().find(|l| l.contains(needle))?;
    let start = line.find("[ref=")? + 5;
    let end = line[start..].find(']')? + start;
    Some(line[start..end].to_string())
}

const FIXTURE: &str = r#"<!doctype html><html><head><title>Users</title></head><body>
<h1>Users</h1>
<input aria-label="Search users" id="q">
<button id="create">Create user</button>
<script>
document.getElementById('create').addEventListener('click',()=>{
  document.querySelector('h1').textContent='Create user form';
});
</script></body></html>"#;

#[tokio::test]
async fn browser_runtime_end_to_end_against_system_chrome() {
    let env = process_env();
    if which(&env, "node").is_none() || which(&env, "npm").is_none() {
        eprintln!("skipping: node/npm not on PATH");
        return;
    }
    if leveler_browser::discover_system_chrome(&env).is_none() {
        eprintln!("skipping: no system Chrome/Chromium");
        return;
    }

    // Fixture on disk (navigated via file://).
    let fix = tempfile::tempdir().unwrap();
    let html = fix.path().join("users.html");
    std::fs::write(&html, FIXTURE).unwrap();
    let url = format!("file://{}", html.display());

    // Shared install cache home (persists across runs), fresh profile per run.
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

    let runtime = BrowserRuntime::new(home, install_env, profile);
    match runtime.ensure_ready().await {
        Ok(info) => eprintln!("runtime ready: {info:?}"),
        Err(e) => {
            eprintln!("skipping: runtime unavailable (likely offline install): {e}");
            return;
        }
    }

    let session = BrowserSessionId::new("accept");
    let page = runtime.new_page(&session).await.expect("new page");
    runtime
        .navigate(&session, &page, &url)
        .await
        .expect("navigate");

    let snap = runtime.snapshot(&session, &page).await.expect("snapshot");
    assert!(
        snap.text.contains("[ref="),
        "snapshot carries refs:\n{}",
        snap.text
    );
    assert!(snap.generation >= 1);
    assert!(snap.text.contains("Create user"), "sees the button");

    // type into the search box
    let search = ref_for(&snap.text, "Search users").expect("search ref");
    runtime
        .interact(
            &session,
            &page,
            &search,
            Interaction::Type {
                text: "alice".into(),
                append: false,
            },
        )
        .await
        .expect("type");

    // click the create button → DOM mutates
    let create = ref_for(&snap.text, "Create user").expect("create ref");
    runtime
        .interact(&session, &page, &create, Interaction::Click)
        .await
        .expect("click");

    let snap2 = runtime.snapshot(&session, &page).await.expect("snapshot2");
    assert!(
        snap2.text.contains("Create user form"),
        "DOM changed after click:\n{}",
        snap2.text
    );
    assert!(
        snap2.generation > snap.generation,
        "generation advanced on change"
    );

    // §53 BLOCKER invariant: a ref from the OLD generation must be rejected as
    // stale, never retargeted, once the page structurally changed.
    let err = runtime
        .interact(&session, &page, &create, Interaction::Click)
        .await;
    assert!(
        matches!(err, Err(BrowserError::RefStale(_))),
        "old-generation ref must be RefStale, got {err:?}"
    );

    // session isolation: another session cannot touch this page.
    let other = BrowserSessionId::new("intruder");
    let cross = runtime.snapshot(&other, &page).await;
    assert!(cross.is_err(), "cross-session page access must be refused");
}
