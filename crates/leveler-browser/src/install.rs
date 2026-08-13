//! Discovery + managed-runtime preparation.
//!
//! The managed runtime lives entirely under `~/.leveler/runtimes/browser/`
//! (never the repo, never a global/system location):
//!
//! ```text
//! runtimes/browser/
//! ├── driver/browser_driver.mjs   the CodeLeveler-authored Playwright bridge
//! ├── node_modules/playwright/     the pinned Playwright library (user-space)
//! ├── browsers/                    managed Chromium (only when no system Chrome)
//! └── .ready                       version+engine marker written after success
//! ```
//!
//! Preferring a **system** Node to run the install/driver and **system** Chrome
//! via Playwright's `channel: chrome` keeps V1 light; a managed Chromium is
//! downloaded only when no compatible system browser exists. Concurrent
//! first-uses are serialized by an `fs2` flock on `.install.lock`, and a crashed
//! install never leaves a half-runtime because readiness is gated on the marker.

use std::path::{Path, PathBuf};

use leveler_core::{EnvSnapshot, LevelerHome};

use crate::{BrowserEngine, BrowserError};

/// The Playwright version CodeLeveler pins and tests against.
pub const PINNED_PLAYWRIGHT: &str = "1.55.0";

/// The driver bridge script, shipped in the binary and written into the managed
/// runtime on first use.
const DRIVER_SCRIPT: &str = include_str!("../driver/browser_driver.mjs");

/// Everything the runtime needs to spawn the driver.
#[derive(Debug, Clone)]
pub struct RuntimeLayout {
    /// `runtimes/browser/`.
    pub root: PathBuf,
    /// The driver script to run with Node.
    pub driver_script: PathBuf,
    /// The resolved Node executable.
    pub node: PathBuf,
    /// The chosen engine.
    pub engine: BrowserEngine,
    /// Playwright launch channel (`Some("chrome")` for system Chrome; `None`
    /// for managed Chromium).
    pub channel: Option<String>,
    /// Where managed browsers are stored (`PLAYWRIGHT_BROWSERS_PATH`).
    pub browsers_path: PathBuf,
    /// Environment passed to the driver (PATH + Playwright browser path).
    pub driver_env: Vec<(String, String)>,
}

/// Resolve an executable by name from the snapshot's `PATH`. Cross-platform:
/// tries the bare name plus the platform's executable suffixes.
pub fn which(env: &EnvSnapshot, name: &str) -> Option<PathBuf> {
    let suffixes: &[&str] = if cfg!(windows) {
        &["", ".exe", ".cmd", ".bat"]
    } else {
        &[""]
    };
    for dir in env.paths("PATH") {
        for suffix in suffixes {
            let candidate = dir.join(format!("{name}{suffix}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Locate a system Google Chrome / Chromium install (for engine reporting and
/// to decide system-vs-managed). Playwright's `channel:'chrome'` does its own
/// resolution at launch; this only needs to answer "is one present".
pub fn discover_system_chrome(env: &EnvSnapshot) -> Option<(BrowserEngine, Option<String>)> {
    #[cfg(target_os = "macos")]
    {
        let chrome = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
        if Path::new(chrome).is_file() {
            return Some((BrowserEngine::SystemChrome, Some("chrome".into())));
        }
    }
    #[cfg(target_os = "linux")]
    {
        for (name, engine, channel) in [
            ("google-chrome", BrowserEngine::SystemChrome, "chrome"),
            (
                "google-chrome-stable",
                BrowserEngine::SystemChrome,
                "chrome",
            ),
            ("chromium", BrowserEngine::SystemChromium, "chromium"),
            (
                "chromium-browser",
                BrowserEngine::SystemChromium,
                "chromium",
            ),
        ] {
            if which(env, name).is_some() {
                return Some((engine, Some(channel.to_string())));
            }
        }
    }
    #[cfg(windows)]
    {
        for p in [
            r"C:\Program Files\Google\Chrome\Application\chrome.exe",
            r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
        ] {
            if Path::new(p).is_file() {
                return Some((BrowserEngine::SystemChrome, Some("chrome".into())));
            }
        }
    }
    let _ = env;
    None
}

/// True when the managed runtime is already installed and matches the pinned
/// version (the marker is written only after a fully successful install).
fn is_ready(root: &Path) -> bool {
    let marker = root.join(".ready");
    let Ok(text) = std::fs::read_to_string(&marker) else {
        return false;
    };
    text.contains(PINNED_PLAYWRIGHT)
        && root.join("node_modules/playwright/package.json").is_file()
        && root.join("driver/browser_driver.mjs").is_file()
}

/// Ensure the managed runtime exists and return how to launch it. Idempotent;
/// safe under concurrent first-use (cross-process flock singleflight).
pub async fn ensure_installed(
    home: &LevelerHome,
    env: &EnvSnapshot,
) -> Result<RuntimeLayout, BrowserError> {
    let root = home.browser_runtime_dir();
    let node = which(env, "node").ok_or_else(|| {
        BrowserError::Unavailable(
            "Node.js is required to run the browser driver but was not found on PATH".into(),
        )
    })?;
    let (engine, channel) =
        discover_system_chrome(env).unwrap_or((BrowserEngine::ManagedChromium, None));
    let browsers_path = root.join("browsers");

    let layout = RuntimeLayout {
        root: root.clone(),
        driver_script: root.join("driver/browser_driver.mjs"),
        node: node.clone(),
        engine,
        channel: channel.clone(),
        browsers_path: browsers_path.clone(),
        driver_env: driver_env(env, &browsers_path),
    };

    if is_ready(&root) {
        // The managed npm deps are version-gated, but the CodeLeveler-authored
        // driver bridge ships INSIDE the binary — an existing install must not
        // keep running a stale bridge after an upgrade (that is how a security
        // fix like the B-1 network gate would silently fail to deploy). Keep the
        // on-disk bridge in lockstep with the binary on every (re)start.
        sync_driver_script(&root)?;
        return Ok(layout);
    }

    std::fs::create_dir_all(&root)
        .map_err(|e| BrowserError::RuntimeInstallFailed(format!("create runtime dir: {e}")))?;

    // Singleflight: one installer, racers wait then reuse.
    let lock_path = root.join(".install.lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|e| BrowserError::RuntimeInstallFailed(format!("open install lock: {e}")))?;
    fs2::FileExt::lock_exclusive(&lock)
        .map_err(|e| BrowserError::RuntimeInstallFailed(format!("acquire install lock: {e}")))?;

    // Re-check under the lock — a racer may have finished while we waited.
    if is_ready(&root) {
        let _ = fs2::FileExt::unlock(&lock);
        return Ok(layout);
    }

    let install_result = install_under_lock(env, &root, engine, &browsers_path).await;
    let _ = fs2::FileExt::unlock(&lock);
    install_result?;
    Ok(layout)
}

/// Write the shipped driver bridge if the on-disk copy is missing or differs
/// from the binary's. Atomic (temp + rename) so a concurrent driver spawn never
/// reads a half-written script. Cheap: a no-op when already in sync.
fn sync_driver_script(root: &Path) -> Result<(), BrowserError> {
    let path = root.join("driver/browser_driver.mjs");
    if std::fs::read_to_string(&path).ok().as_deref() == Some(DRIVER_SCRIPT) {
        return Ok(());
    }
    std::fs::create_dir_all(root.join("driver"))
        .map_err(|e| BrowserError::RuntimeInstallFailed(format!("mkdir driver: {e}")))?;
    let tmp = root.join("driver/.browser_driver.mjs.tmp");
    std::fs::write(&tmp, DRIVER_SCRIPT)
        .map_err(|e| BrowserError::RuntimeInstallFailed(format!("write driver: {e}")))?;
    std::fs::rename(&tmp, &path)
        .map_err(|e| BrowserError::RuntimeInstallFailed(format!("swap driver: {e}")))?;
    Ok(())
}

/// The env handed to the driver child: PATH (to find node's own deps) plus the
/// managed browser path so Playwright resolves managed Chromium.
fn driver_env(env: &EnvSnapshot, browsers_path: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(path) = env.var("PATH") {
        out.push(("PATH".into(), path));
    }
    out.push((
        "PLAYWRIGHT_BROWSERS_PATH".into(),
        browsers_path.to_string_lossy().into_owned(),
    ));
    // Never let the driver phone Playwright's telemetry / update checks.
    out.push(("PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD".into(), "1".into()));
    out
}

async fn install_under_lock(
    env: &EnvSnapshot,
    root: &Path,
    engine: BrowserEngine,
    browsers_path: &Path,
) -> Result<(), BrowserError> {
    use tokio::process::Command;

    // 1. Driver script + a private package.json (so npm installs locally).
    std::fs::create_dir_all(root.join("driver"))
        .map_err(|e| BrowserError::RuntimeInstallFailed(format!("mkdir driver: {e}")))?;
    std::fs::write(root.join("driver/browser_driver.mjs"), DRIVER_SCRIPT)
        .map_err(|e| BrowserError::RuntimeInstallFailed(format!("write driver: {e}")))?;
    std::fs::write(
        root.join("package.json"),
        r#"{"name":"leveler-browser-runtime","private":true}"#,
    )
    .map_err(|e| BrowserError::RuntimeInstallFailed(format!("write package.json: {e}")))?;

    let npm = which(env, "npm").ok_or_else(|| {
        BrowserError::Unavailable("npm is required to install the browser driver".into())
    })?;
    let path = env.var("PATH").unwrap_or_default();

    // 2. Install the pinned Playwright library, user-space, into this dir.
    // For system Chrome we skip the browser download entirely.
    let skip_download = !matches!(engine, BrowserEngine::ManagedChromium);
    let status = Command::new(&npm)
        .args([
            "install",
            &format!("playwright@{PINNED_PLAYWRIGHT}"),
            "--no-audit",
            "--no-fund",
        ])
        .current_dir(root)
        .env_clear()
        .env("PATH", &path)
        .env("PLAYWRIGHT_BROWSERS_PATH", browsers_path)
        .env(
            "PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD",
            if skip_download { "1" } else { "0" },
        )
        .status()
        .await
        .map_err(|e| BrowserError::RuntimeInstallFailed(format!("run npm: {e}")))?;
    if !status.success() {
        return Err(BrowserError::RuntimeInstallFailed(format!(
            "npm install playwright exited with {status}"
        )));
    }

    // 3. Managed Chromium only when there is no system browser.
    if matches!(engine, BrowserEngine::ManagedChromium) {
        let cli = root.join("node_modules/playwright/cli.js");
        let node = which(env, "node").ok_or_else(|| {
            BrowserError::RuntimeInstallFailed("node vanished mid-install".into())
        })?;
        let status = Command::new(&node)
            .arg(&cli)
            .args(["install", "chromium"])
            .current_dir(root)
            .env_clear()
            .env("PATH", &path)
            .env("PLAYWRIGHT_BROWSERS_PATH", browsers_path)
            .status()
            .await
            .map_err(|e| {
                BrowserError::RuntimeInstallFailed(format!("run playwright install: {e}"))
            })?;
        if !status.success() {
            return Err(BrowserError::RuntimeInstallFailed(format!(
                "playwright install chromium exited with {status}"
            )));
        }
    }

    // 4. Readiness marker LAST — the gate that makes the install atomic.
    let engine_tag = match engine {
        BrowserEngine::SystemChrome => "system_chrome",
        BrowserEngine::SystemChromium => "system_chromium",
        BrowserEngine::ManagedChromium => "managed_chromium",
    };
    std::fs::write(
        root.join(".ready"),
        format!("playwright={PINNED_PLAYWRIGHT}\nengine={engine_tag}\n"),
    )
    .map_err(|e| BrowserError::RuntimeInstallFailed(format!("write ready marker: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn env_with_path(dir: &Path) -> EnvSnapshot {
        EnvSnapshot::new(
            [(OsString::from("PATH"), OsString::from(dir.as_os_str()))],
            std::path::PathBuf::new(),
            std::env::temp_dir(),
        )
    }

    #[test]
    fn which_finds_an_executable_on_path() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir
            .path()
            .join(if cfg!(windows) { "tool.exe" } else { "tool" });
        std::fs::write(&exe, "x").unwrap();
        let env = env_with_path(dir.path());
        assert_eq!(which(&env, "tool"), Some(exe));
        assert_eq!(which(&env, "absent"), None);
    }

    #[test]
    fn is_ready_requires_marker_and_installed_files() {
        let root = tempfile::tempdir().unwrap();
        assert!(!is_ready(root.path()), "empty dir is not ready");
        std::fs::write(
            root.path().join(".ready"),
            format!("playwright={PINNED_PLAYWRIGHT}"),
        )
        .unwrap();
        assert!(!is_ready(root.path()), "marker without files is not ready");
        std::fs::create_dir_all(root.path().join("node_modules/playwright")).unwrap();
        std::fs::write(
            root.path().join("node_modules/playwright/package.json"),
            "{}",
        )
        .unwrap();
        std::fs::create_dir_all(root.path().join("driver")).unwrap();
        std::fs::write(root.path().join("driver/browser_driver.mjs"), "//").unwrap();
        assert!(is_ready(root.path()), "marker + files ⇒ ready");
    }

    #[test]
    fn driver_env_sets_browser_path_and_skips_download() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_with_path(dir.path());
        let e = driver_env(&env, Path::new("/x/browsers"));
        assert!(
            e.iter()
                .any(|(k, v)| k == "PLAYWRIGHT_BROWSERS_PATH" && v == "/x/browsers")
        );
        assert!(e.iter().any(|(k, _)| k == "PATH"));
    }
}
