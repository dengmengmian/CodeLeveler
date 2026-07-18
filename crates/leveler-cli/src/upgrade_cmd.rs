//! Self-update: check GitHub releases and install a newer `leveler`.
//!
//! Preferred path: download a matching release asset for the host triple.
//! Fallback: `cargo install --git … --locked --force` when no asset exists.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};
use serde::Deserialize;

use crate::output::Line;

/// Default GitHub repository (`owner/name`). Override with `LEVELER_GITHUB_REPO`.
pub const DEFAULT_GITHUB_REPO: &str = "dengmengmian/CodeLeveler";

/// Semantic version used for update comparisons (release tags only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Parse `0.1.0`, `v0.1.0`, or a tag with a pre-release suffix (`0.1.0-beta.1`).
/// Pre-release / build metadata is ignored for ordering; only `x.y.z` is kept.
pub fn parse_version(raw: &str) -> Option<Version> {
    let s = raw.trim().trim_start_matches('v');
    let core = s.split(['-', '+']).next().unwrap_or("");
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(Version {
        major,
        minor,
        patch,
    })
}

/// Whether an install should proceed.
pub fn should_upgrade(current: Version, target: Version, force: bool) -> bool {
    force || target > current
}

/// Host target triple used to match GitHub release assets.
pub fn host_target_triple() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        ("linux", "x86_64") => Some("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-gnu"),
        ("windows", "x86_64") => Some("x86_64-pc-windows-msvc"),
        ("windows", "aarch64") => Some("aarch64-pc-windows-msvc"),
        _ => None,
    }
}

/// Expected asset file name for a release (without path).
///
/// Unix: `leveler-v{version}-{triple}.tar.gz`
/// Windows: `leveler-v{version}-{triple}.zip`
pub fn release_asset_name(version: Version, triple: &str) -> String {
    let ext = if triple.contains("windows") {
        "zip"
    } else {
        "tar.gz"
    };
    format!("leveler-v{version}-{triple}.{ext}")
}

/// Resolve the GitHub `owner/repo` used for releases.
pub fn github_repo() -> String {
    std::env::var("LEVELER_GITHUB_REPO").unwrap_or_else(|_| DEFAULT_GITHUB_REPO.to_string())
}

#[derive(Debug, Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Debug, Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

/// A resolved release target.
#[derive(Debug, Clone)]
pub struct ReleaseInfo {
    pub tag: String,
    pub version: Version,
    pub asset_name: Option<String>,
    pub download_url: Option<String>,
}

/// Look up a release: latest, or a specific tag (`v0.1.0` / `0.1.0`).
pub async fn fetch_release(
    client: &reqwest::Client,
    repo: &str,
    version: Option<&str>,
) -> anyhow::Result<ReleaseInfo> {
    let url = match version {
        Some(v) => {
            let tag = normalize_tag(v);
            format!("https://api.github.com/repos/{repo}/releases/tags/{tag}")
        }
        None => format!("https://api.github.com/repos/{repo}/releases/latest"),
    };

    let response = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("request GitHub release: {url}"))?;

    if response.status() == reqwest::StatusCode::NOT_FOUND {
        bail!(
            "no GitHub release found for {repo}{}",
            version
                .map(|v| format!(" tag {}", normalize_tag(v)))
                .unwrap_or_default()
        );
    }
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        bail!("GitHub API {status}: {body}");
    }

    let release: GhRelease = response
        .json()
        .await
        .context("decode GitHub release JSON")?;
    let ver = parse_version(&release.tag_name)
        .with_context(|| format!("unparseable release tag `{}`", release.tag_name))?;

    let (asset_name, download_url) = match host_target_triple() {
        Some(triple) => {
            let want = release_asset_name(ver, triple);
            release
                .assets
                .into_iter()
                .find(|a| a.name == want)
                .map(|a| (Some(a.name), Some(a.browser_download_url)))
                .unwrap_or((None, None))
        }
        None => (None, None),
    };

    Ok(ReleaseInfo {
        tag: release.tag_name,
        version: ver,
        asset_name,
        download_url,
    })
}

fn normalize_tag(raw: &str) -> String {
    let t = raw.trim();
    if t.starts_with('v') {
        t.to_string()
    } else {
        format!("v{t}")
    }
}

fn http_client() -> anyhow::Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .user_agent(format!("leveler/{}", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(60));

    // Optional token raises the unauthenticated rate limit and helps private forks.
    if let Ok(token) = std::env::var("GITHUB_TOKEN")
        && !token.is_empty()
    {
        let mut headers = reqwest::header::HeaderMap::new();
        let value = format!("Bearer {token}");
        headers.insert(
            reqwest::header::AUTHORIZATION,
            value
                .parse()
                .context("GITHUB_TOKEN is not a valid Authorization header value")?,
        );
        headers.insert(
            reqwest::header::ACCEPT,
            "application/vnd.github+json"
                .parse()
                .expect("static header"),
        );
        builder = builder.default_headers(headers);
    }

    builder.build().context("build HTTP client")
}

pub(crate) async fn cmd_upgrade(
    check_only: bool,
    force: bool,
    version: Option<String>,
) -> anyhow::Result<std::process::ExitCode> {
    let current = parse_version(env!("CARGO_PKG_VERSION"))
        .context("built binary has an unparseable CARGO_PKG_VERSION")?;
    let repo = github_repo();
    let client = http_client()?;

    println!("{}", Line::heading("leveler upgrade"));
    println!("  current:  v{current}");
    println!("  repo:     {repo}");

    let release = fetch_release(&client, &repo, version.as_deref()).await?;
    println!("  latest:   {} ({})", release.tag, release.version);

    if !should_upgrade(current, release.version, force) {
        println!("{}", Line::ok("Already up to date."));
        return Ok(std::process::ExitCode::SUCCESS);
    }

    if check_only {
        println!(
            "{}",
            Line::warn(&format!(
                "Update available: v{current} → {} ({})",
                release.version, release.tag
            ))
        );
        if release.download_url.is_some() {
            if let Some(name) = &release.asset_name {
                println!("  asset:    {name}");
            }
        } else {
            println!(
                "  note:     no prebuilt asset for this host; `leveler upgrade` will use cargo"
            );
        }
        return Ok(std::process::ExitCode::from(2));
    }

    if let (Some(url), Some(name)) = (&release.download_url, &release.asset_name) {
        println!("{}", Line::heading("Downloading release asset"));
        println!("  {name}");
        install_from_asset(&client, url, name).await?;
        println!(
            "{}",
            Line::ok(&format!("Installed {} successfully.", release.tag))
        );
        return Ok(std::process::ExitCode::SUCCESS);
    }

    println!("{}", Line::heading("Installing from source (cargo)"));
    install_via_cargo(&repo, &release.tag)?;
    println!(
        "{}",
        Line::ok(&format!(
            "Installed {} via cargo. Restart any open leveler sessions.",
            release.tag
        ))
    );
    Ok(std::process::ExitCode::SUCCESS)
}

async fn install_from_asset(
    client: &reqwest::Client,
    url: &str,
    asset_name: &str,
) -> anyhow::Result<()> {
    let bytes = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("download {url}"))?
        .error_for_status()
        .with_context(|| format!("download {url}"))?
        .bytes()
        .await
        .context("read asset body")?;

    let current_exe = std::env::current_exe().context("resolve current executable")?;
    let current_exe = std::fs::canonicalize(&current_exe).unwrap_or(current_exe);
    let install_dir = current_exe
        .parent()
        .map(Path::to_path_buf)
        .context("current executable has no parent directory")?;

    let tmp = tempfile::tempdir().context("create temp dir for release asset")?;
    let archive_path = tmp.path().join(asset_name);
    std::fs::write(&archive_path, &bytes).context("write downloaded asset")?;

    let extracted = extract_binary(&archive_path, tmp.path(), asset_name)?;
    replace_executable(&extracted, &current_exe, &install_dir)?;
    Ok(())
}

fn extract_binary(archive: &Path, dest_dir: &Path, asset_name: &str) -> anyhow::Result<PathBuf> {
    let status = if asset_name.ends_with(".zip") {
        Command::new("tar")
            // Windows 10+ ships tar that can read zip.
            .args(["-xf"])
            .arg(archive)
            .arg("-C")
            .arg(dest_dir)
            .status()
    } else {
        Command::new("tar")
            .args(["-xzf"])
            .arg(archive)
            .arg("-C")
            .arg(dest_dir)
            .status()
    }
    .with_context(|| format!("extract {}", archive.display()))?;

    if !status.success() {
        bail!(
            "failed to extract {} (is tar available?)",
            archive.display()
        );
    }

    // Prefer a file named `leveler` / `leveler.exe` anywhere under dest_dir.
    let want = if cfg!(windows) {
        "leveler.exe"
    } else {
        "leveler"
    };
    let mut found = None;
    for entry in walkdir_shallow(dest_dir)? {
        if entry.file_name().is_some_and(|n| n == want) {
            found = Some(entry);
            break;
        }
    }
    found.with_context(|| format!("archive did not contain `{want}`"))
}

/// One-level-deep then recursive scan without a walkdir dependency.
fn walkdir_shallow(root: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out)?;
            } else {
                out.push(path);
            }
        }
        Ok(())
    }
    walk(root, &mut out).with_context(|| format!("scan {}", root.display()))?;
    Ok(out)
}

fn replace_executable(src: &Path, current_exe: &Path, install_dir: &Path) -> anyhow::Result<()> {
    let file_name = current_exe
        .file_name()
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            if cfg!(windows) {
                PathBuf::from("leveler.exe")
            } else {
                PathBuf::from("leveler")
            }
        });
    let dest = install_dir.join(&file_name);

    // Stage next to the destination so rename stays on the same filesystem.
    let staged = install_dir.join(format!(
        ".{}.new-{}",
        file_name.to_string_lossy(),
        std::process::id()
    ));
    std::fs::copy(src, &staged).with_context(|| format!("copy to {}", staged.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&staged)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&staged, perms)?;
    }

    if dest.exists() {
        let backup = install_dir.join(format!("{}.old", file_name.to_string_lossy()));
        let _ = std::fs::remove_file(&backup);
        // On Windows the running image may still be locked; rename current → .old first.
        std::fs::rename(&dest, &backup)
            .with_context(|| format!("move current binary aside to {}", backup.display()))?;
        if let Err(e) = std::fs::rename(&staged, &dest) {
            // Best-effort restore.
            let _ = std::fs::rename(&backup, &dest);
            return Err(e).with_context(|| format!("install new binary to {}", dest.display()));
        }
        let _ = std::fs::remove_file(&backup);
    } else {
        std::fs::rename(&staged, &dest)
            .with_context(|| format!("install new binary to {}", dest.display()))?;
    }

    println!("  installed: {}", dest.display());
    Ok(())
}

fn install_via_cargo(repo: &str, tag: &str) -> anyhow::Result<()> {
    let git_url = format!("https://github.com/{repo}.git");
    println!("  cargo install --git {git_url} --tag {tag} --locked --force --bin leveler");

    let status = Command::new("cargo")
        .args([
            "install", "--git", &git_url, "--tag", tag, "--locked", "--force", "--bin", "leveler",
        ])
        .status()
        .context("spawn cargo (is the Rust toolchain installed and on PATH?)")?;

    if !status.success() {
        bail!(
            "cargo install failed with {status}. \
             Build from source: git clone {git_url} && cargo install --path crates/leveler-cli --locked --force"
        );
    }

    if let Ok(exe) = std::env::current_exe() {
        let exe_s = exe.display().to_string();
        if !exe_s.contains(".cargo") {
            println!(
                "{}",
                Line::warn(&format!(
                    "cargo installs into ~/.cargo/bin; this process is still {}",
                    exe.display()
                ))
            );
            println!("  put ~/.cargo/bin first on PATH, or re-run the installed binary.");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_plain_and_v_prefix() {
        assert_eq!(
            parse_version("0.1.0").unwrap(),
            Version {
                major: 0,
                minor: 1,
                patch: 0
            }
        );
        assert_eq!(
            parse_version("v0.1.0").unwrap(),
            Version {
                major: 0,
                minor: 1,
                patch: 0
            }
        );
        assert_eq!(
            parse_version("v1.2.3-beta.1").unwrap(),
            Version {
                major: 1,
                minor: 2,
                patch: 3
            }
        );
        assert!(parse_version("").is_none());
        assert!(parse_version("v1.2").is_none());
        assert!(parse_version("nope").is_none());
    }

    #[test]
    fn version_ordering() {
        let a = parse_version("0.1.0").unwrap();
        let b = parse_version("0.1.1").unwrap();
        let c = parse_version("0.2.0").unwrap();
        assert!(a < b);
        assert!(b < c);
        assert!(!should_upgrade(b, a, false));
        assert!(should_upgrade(a, b, false));
        assert!(should_upgrade(b, a, true));
        assert!(!should_upgrade(a, a, false));
        assert!(should_upgrade(a, a, true));
    }

    #[test]
    fn asset_names_match_convention() {
        let v = parse_version("0.1.0").unwrap();
        assert_eq!(
            release_asset_name(v, "aarch64-apple-darwin"),
            "leveler-v0.1.0-aarch64-apple-darwin.tar.gz"
        );
        assert_eq!(
            release_asset_name(v, "x86_64-pc-windows-msvc"),
            "leveler-v0.1.0-x86_64-pc-windows-msvc.zip"
        );
        assert_eq!(
            release_asset_name(v, "x86_64-unknown-linux-gnu"),
            "leveler-v0.1.0-x86_64-unknown-linux-gnu.tar.gz"
        );
    }

    #[test]
    fn normalize_tag_adds_v() {
        assert_eq!(normalize_tag("0.1.0"), "v0.1.0");
        assert_eq!(normalize_tag("v0.1.0"), "v0.1.0");
    }

    #[test]
    fn display_version() {
        assert_eq!(parse_version("v0.1.0").unwrap().to_string(), "0.1.0");
    }
}
