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
///
/// The pre-release part is carried rather than discarded. It has to be: it
/// decides ordering (`0.2.0-beta.1` precedes `0.2.0`), it is what the binary
/// calls itself, and it is part of the release asset's file name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    /// Dot-separated identifiers from `-a.b.c`; empty for a release.
    pub pre: Vec<String>,
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if !self.pre.is_empty() {
            write!(f, "-{}", self.pre.join("."))?;
        }
        Ok(())
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                // SemVer §11: a release outranks any pre-release of the same
                // `x.y.z`. This is the line that lets a beta user be offered
                // the stable release their beta led to.
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => cmp_pre(&self.pre, &other.pre),
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Compare two non-empty pre-release identifier lists (SemVer §11): numeric
/// identifiers compare numerically and rank below alphanumeric ones, and when
/// every shared identifier is equal, the longer list wins (`beta` < `beta.1`).
fn cmp_pre(a: &[String], b: &[String]) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    for (x, y) in a.iter().zip(b.iter()) {
        // An identifier too large for u64 is treated as alphanumeric rather
        // than panicking; no real tag reaches that, and failing safe beats
        // failing loudly in the upgrade path.
        let ord = match (x.parse::<u64>(), y.parse::<u64>()) {
            (Ok(nx), Ok(ny)) => nx.cmp(&ny),
            (Ok(_), Err(_)) => Ordering::Less,
            (Err(_), Ok(_)) => Ordering::Greater,
            (Err(_), Err(_)) => x.as_str().cmp(y.as_str()),
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    a.len().cmp(&b.len())
}

/// Parse `0.1.0`, `v0.1.0`, or a tag with a pre-release suffix (`0.1.0-beta.1`).
///
/// Build metadata (`+abc123`) is dropped: SemVer §10 keeps it out of
/// precedence, and no release asset carries it.
pub fn parse_version(raw: &str) -> Option<Version> {
    let s = raw.trim().trim_start_matches('v');
    let s = s.split('+').next().unwrap_or("");
    let (core, pre) = match s.split_once('-') {
        // `1.0.0-` has a pre-release marker and no identifiers: not a version.
        Some((_, "")) => return None,
        Some((core, rest)) => (
            core,
            rest.split('.').map(str::to_string).collect::<Vec<_>>(),
        ),
        None => (s, Vec::new()),
    };
    if pre.iter().any(String::is_empty) {
        return None;
    }
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
        pre,
    })
}

/// Whether an install should proceed.
pub fn should_upgrade(current: &Version, target: &Version, force: bool) -> bool {
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
pub fn release_asset_name(version: &Version, triple: &str) -> String {
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
    /// The `<asset>.sha256` sibling asset. Installation refuses to proceed
    /// without it — the self-update channel never installs unverified bytes.
    pub checksum_url: Option<String>,
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

    let (asset_name, download_url, checksum_url) = match host_target_triple() {
        Some(triple) => {
            let want = release_asset_name(&ver, triple);
            let checksum = release
                .assets
                .iter()
                .find(|a| a.name == format!("{want}.sha256"))
                .map(|a| a.browser_download_url.clone());
            release
                .assets
                .into_iter()
                .find(|a| a.name == want)
                .map(|a| (Some(a.name), Some(a.browser_download_url), checksum))
                .unwrap_or((None, None, None))
        }
        None => (None, None, None),
    };

    Ok(ReleaseInfo {
        tag: release.tag_name,
        version: ver,
        asset_name,
        download_url,
        checksum_url,
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

    if !should_upgrade(&current, &release.version, force) {
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
        install_from_asset(&client, url, name, release.checksum_url.as_deref()).await?;
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
    checksum_url: Option<&str>,
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

    // Fail closed: no published checksum → no install. Every release built by
    // the release workflow ships `<asset>.sha256`; its absence means either a
    // hand-rolled release or a stripped-down mirror — both untrusted.
    let checksum_url = checksum_url.with_context(|| {
        format!("release has no {asset_name}.sha256 asset; refusing unverified install")
    })?;
    let checksum_file = client
        .get(checksum_url)
        .send()
        .await
        .with_context(|| format!("download {checksum_url}"))?
        .error_for_status()
        .with_context(|| format!("download {checksum_url}"))?
        .text()
        .await
        .context("read checksum body")?;
    verify_sha256(&bytes, &checksum_file, asset_name)?;
    println!("  sha256:   verified");

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

/// Verify downloaded bytes against a `sha256sum`-style checksum file
/// (`<hex>  <name>`). The self-update channel replaces the running binary —
/// a corrupted or tampered download must never be installed.
fn verify_sha256(bytes: &[u8], checksum_file: &str, asset_name: &str) -> anyhow::Result<()> {
    use sha2::{Digest, Sha256};
    let expected = checksum_file
        .split_whitespace()
        .next()
        .filter(|t| t.len() == 64 && t.chars().all(|c| c.is_ascii_hexdigit()))
        .with_context(|| format!("unparseable sha256 checksum file for {asset_name}"))?;
    let actual = format!("{:x}", Sha256::digest(bytes));
    if !actual.eq_ignore_ascii_case(expected) {
        bail!(
            "sha256 mismatch for {asset_name}: expected {expected}, got {actual} — \
             refusing to install a corrupted or tampered download"
        );
    }
    Ok(())
}

#[cfg(test)]
mod verify_sha256_tests {
    use super::verify_sha256;

    // sha256("leveler-test-bytes")
    const GOOD: &str = "c22126f13fabb24a69455c85ff9e6821c68032ba91be2041b7275847e0e24906";

    #[test]
    fn matching_digest_passes() {
        let line = format!("{GOOD}  leveler-v0.1.0-x.tar.gz\n");
        verify_sha256(b"leveler-test-bytes", &line, "leveler-v0.1.0-x.tar.gz")
            .expect("matching digest must verify");
    }

    #[test]
    fn mismatched_digest_is_rejected() {
        let line = format!("{}  leveler-v0.1.0-x.tar.gz\n", "0".repeat(64));
        assert!(
            verify_sha256(b"leveler-test-bytes", &line, "leveler-v0.1.0-x.tar.gz").is_err(),
            "a wrong digest must reject the download"
        );
    }

    #[test]
    fn garbage_checksum_file_is_rejected() {
        assert!(
            verify_sha256(b"leveler-test-bytes", "not a checksum", "x.tar.gz").is_err(),
            "an unparseable checksum file must reject, not skip verification"
        );
        assert!(
            verify_sha256(b"leveler-test-bytes", "", "x.tar.gz").is_err(),
            "an empty checksum file must reject"
        );
    }

    #[test]
    fn uppercase_digest_is_accepted() {
        let line = format!("{}  x.tar.gz", GOOD.to_uppercase());
        verify_sha256(b"leveler-test-bytes", &line, "x.tar.gz")
            .expect("hex comparison must be case-insensitive");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_plain_and_v_prefix() {
        let v010 = Version {
            major: 0,
            minor: 1,
            patch: 0,
            pre: Vec::new(),
        };
        assert_eq!(parse_version("0.1.0").unwrap(), v010);
        assert_eq!(parse_version("v0.1.0").unwrap(), v010);
        // This used to assert that the suffix was thrown away. It is kept.
        assert_eq!(
            parse_version("v1.2.3-beta.1").unwrap(),
            Version {
                major: 1,
                minor: 2,
                patch: 3,
                pre: vec!["beta".into(), "1".into()],
            }
        );
        assert!(parse_version("").is_none());
        assert!(parse_version("v1.2").is_none());
        assert!(parse_version("nope").is_none());
        // A pre-release marker with nothing after it, and an empty identifier.
        assert!(parse_version("1.0.0-").is_none());
        assert!(parse_version("1.0.0-a..b").is_none());
    }

    #[test]
    fn version_ordering() {
        let a = parse_version("0.1.0").unwrap();
        let b = parse_version("0.1.1").unwrap();
        let c = parse_version("0.2.0").unwrap();
        assert!(a < b);
        assert!(b < c);
        assert!(!should_upgrade(&b, &a, false));
        assert!(should_upgrade(&a, &b, false));
        assert!(should_upgrade(&b, &a, true));
        assert!(!should_upgrade(&a, &a, false));
        assert!(should_upgrade(&a, &a, true));
    }

    #[test]
    fn asset_names_match_convention() {
        let v = parse_version("0.1.0").unwrap();
        assert_eq!(
            release_asset_name(&v, "aarch64-apple-darwin"),
            "leveler-v0.1.0-aarch64-apple-darwin.tar.gz"
        );
        assert_eq!(
            release_asset_name(&v, "x86_64-pc-windows-msvc"),
            "leveler-v0.1.0-x86_64-pc-windows-msvc.zip"
        );
        assert_eq!(
            release_asset_name(&v, "x86_64-unknown-linux-gnu"),
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

    // ── Pre-releases. Reproduced against the published v0.2.0-beta.1: the
    // suffix was dropped at parse time, so it was missing from ordering,
    // from display, and from the asset name the download path looks for.

    /// SemVer §11: a pre-release precedes the release it leads to. Without
    /// this, a `0.2.0-beta.n` user is never offered `0.2.0` stable — the
    /// versions compare equal, `should_upgrade` says no, and the people who
    /// volunteered to test are the ones stranded.
    #[test]
    fn a_prerelease_precedes_its_own_release() {
        let beta = parse_version("v0.2.0-beta.1").unwrap();
        let stable = parse_version("v0.2.0").unwrap();
        assert!(beta < stable, "0.2.0-beta.1 must sort before 0.2.0");
        assert!(
            should_upgrade(&beta, &stable, false),
            "a beta user must be offered the stable release it leads to"
        );
        assert!(
            !should_upgrade(&stable, &beta, false),
            "a stable user must never be offered a pre-release as an upgrade"
        );
    }

    /// Precedence *within* a pre-release line, so `beta.2` supersedes
    /// `beta.1` and `rc` supersedes `beta`. Numeric identifiers compare
    /// numerically (`beta.9` before `beta.10`, not after it, which is what a
    /// string comparison would give).
    #[test]
    fn prereleases_order_among_themselves() {
        let v = |s: &str| parse_version(s).unwrap();
        assert!(v("0.2.0-alpha.1") < v("0.2.0-beta.1"));
        assert!(v("0.2.0-beta.1") < v("0.2.0-beta.2"));
        assert!(v("0.2.0-beta.9") < v("0.2.0-beta.10"));
        assert!(v("0.2.0-beta.2") < v("0.2.0-rc.1"));
        assert!(v("0.2.0-beta") < v("0.2.0-beta.1"));
        assert!(v("0.1.9") < v("0.2.0-beta.1"));
    }

    /// `--version` prints `0.2.0-beta.1`; the upgrade path must not disagree
    /// with the binary about which build is running.
    #[test]
    fn a_prerelease_keeps_its_suffix_when_displayed() {
        assert_eq!(
            parse_version("v0.2.0-beta.1").unwrap().to_string(),
            "0.2.0-beta.1"
        );
    }

    /// The published asset really is named `leveler-v0.2.0-beta.1-…`. Building
    /// the name from a suffix-stripped version looked for
    /// `leveler-v0.2.0-…`, found nothing, and silently fell back to
    /// compiling from source — on a machine that may have no Rust toolchain.
    #[test]
    fn a_prerelease_asset_name_carries_the_suffix() {
        let v = parse_version("v0.2.0-beta.1").unwrap();
        assert_eq!(
            release_asset_name(&v, "aarch64-apple-darwin"),
            "leveler-v0.2.0-beta.1-aarch64-apple-darwin.tar.gz"
        );
        assert_eq!(
            release_asset_name(&v, "x86_64-pc-windows-msvc"),
            "leveler-v0.2.0-beta.1-x86_64-pc-windows-msvc.zip"
        );
    }

    /// Build metadata stays out of precedence (SemVer §10) and out of the
    /// asset name, which never carries it.
    #[test]
    fn build_metadata_is_ignored() {
        assert_eq!(
            parse_version("0.2.0+d0fc362").unwrap(),
            parse_version("0.2.0").unwrap()
        );
        assert_eq!(
            parse_version("0.2.0-beta.1+d0fc362").unwrap(),
            parse_version("0.2.0-beta.1").unwrap()
        );
    }
}
