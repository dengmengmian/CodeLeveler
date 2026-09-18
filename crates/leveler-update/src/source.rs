//! Where releases come from: GitHub Releases, and nothing else.
//!
//! The trait exists so the update policy can be tested against fake metadata
//! without a network. The production implementation talks to the anonymous
//! releases API; `releases/latest` is GitHub's own "latest stable" — it already
//! excludes drafts and pre-releases.

use serde::Deserialize;

use crate::error::UpdateError;
use crate::target::{checksum_asset_name, release_asset_name};
use crate::version::{Version, parse_version};

/// One downloadable file on a release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub download_url: String,
}

/// A resolved release, presentation- and transport-free.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub tag: String,
    pub version: Version,
    pub prerelease: bool,
    pub assets: Vec<Asset>,
}

/// The three URLs an install needs for this host, derived from one release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseAssets {
    pub asset_name: String,
    pub download_url: String,
    pub checksum_url: String,
}

/// Resolve the release for a host, failing closed when anything is missing.
///
/// Exact-name matching: the workflow builds exactly
/// `leveler-v{version}-{triple}.{ext}`, so "no asset" is the honest answer for
/// a host whose name is not that. A duplicate exact name is malformed
/// metadata, not something to pick from.
pub fn select_asset(release: &Release, triple: &str) -> Result<ReleaseAssets, UpdateError> {
    let asset_name = release_asset_name(&release.version, triple);
    let checksum_name = checksum_asset_name(&asset_name);

    let mut matching = release.assets.iter().filter(|a| a.name == asset_name);
    let asset = matching.next().ok_or_else(|| UpdateError::NoAsset {
        triple: triple.to_string(),
        tag: release.tag.clone(),
    })?;
    if matching.next().is_some() {
        return Err(UpdateError::Validation(format!(
            "release {} carries two `{asset_name}` assets",
            release.tag
        )));
    }

    let checksum_url = release
        .assets
        .iter()
        .find(|a| a.name == checksum_name)
        .map(|a| a.download_url.clone())
        .ok_or_else(|| UpdateError::NoChecksum {
            tag: release.tag.clone(),
            asset: asset_name.clone(),
        })?;

    Ok(ReleaseAssets {
        asset_name,
        download_url: asset.download_url.clone(),
        checksum_url,
    })
}

/// A source of GitHub releases.
///
/// `async_trait` (not bare `async fn`) so the futures are `Send`: the TUI
/// spawns an update on its runtime, which requires it.
#[async_trait::async_trait]
pub trait ReleaseSource {
    /// The latest stable release.
    async fn latest(&self) -> Result<Release, UpdateError>;

    /// A specific release by tag (accepts `v1.0.0` or `1.0.0`).
    async fn by_tag(&self, tag: &str) -> Result<Release, UpdateError>;
}

#[derive(Debug, Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Debug, Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

/// The production source: `api.github.com/repos/{owner}/{repo}/releases`.
#[derive(Clone)]
pub struct GitHubReleaseSource {
    repo: String,
    client: reqwest::Client,
}

impl GitHubReleaseSource {
    pub fn new(repo: impl Into<String>, client: reqwest::Client) -> Self {
        Self {
            repo: repo.into(),
            client,
        }
    }

    pub fn repo(&self) -> &str {
        &self.repo
    }

    /// GET a URL and decode it as JSON, mapping transport and HTTP failures.
    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T, UpdateError> {
        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| UpdateError::Network(e.to_string()))?;

        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(UpdateError::NoRelease(self.repo.clone()));
        }
        if !status.is_success() {
            return Err(UpdateError::Http {
                status: status.as_u16(),
                url: url.to_string(),
            });
        }

        response
            .json()
            .await
            .map_err(|e| UpdateError::Network(format!("decode release JSON: {e}")))
    }
}

/// Decode one GitHub release, keeping the tag's own pre-release identifiers.
fn decode(raw: GhRelease) -> Result<Release, UpdateError> {
    let version = parse_version(&raw.tag_name)
        .ok_or_else(|| UpdateError::UnparseableTag(raw.tag_name.clone()))?;
    Ok(Release {
        tag: raw.tag_name,
        version,
        prerelease: raw.prerelease,
        assets: raw
            .assets
            .into_iter()
            .map(|a| Asset {
                name: a.name,
                download_url: a.browser_download_url,
            })
            .collect(),
    })
}

/// Whether a release is a candidate for automatic update: not a draft, not
/// flagged pre-release, and not a `-`-suffixed tag.
///
/// The tag check is the load-bearing one. GitHub's `releases/latest` only
/// honours the *flag*, and a `v1.0.0-beta.1` tag that was published without it
/// still reads as "latest" — so a stable build would be offered a beta. The
/// updater never trusts the flag alone.
fn is_stable_release(raw: &GhRelease) -> bool {
    if raw.draft || raw.prerelease {
        return false;
    }
    parse_version(&raw.tag_name).is_some_and(|v| !v.is_prerelease())
}

#[async_trait::async_trait]
impl ReleaseSource for GitHubReleaseSource {
    /// The newest stable release.
    ///
    /// Reads the release list rather than `/releases/latest`, because a
    /// hyphen-tagged release published without the pre-release flag is
    /// returned by `latest` and would otherwise be handed to a stable build.
    async fn latest(&self) -> Result<Release, UpdateError> {
        let url = format!(
            "https://api.github.com/repos/{}/releases?per_page=100",
            self.repo
        );
        let raw: Vec<GhRelease> = self.get_json(&url).await?;
        let newest = raw
            .into_iter()
            .filter(is_stable_release)
            .filter_map(|r| decode(r).ok())
            .max_by(|a, b| a.version.cmp(&b.version));
        newest.ok_or(UpdateError::NoStableRelease {
            repo: self.repo.clone(),
        })
    }

    /// A specific release by tag. May be a pre-release — an explicit install
    /// is the one way to get one.
    async fn by_tag(&self, tag: &str) -> Result<Release, UpdateError> {
        let tag = normalize_tag(tag);
        let url = format!(
            "https://api.github.com/repos/{}/releases/tags/{tag}",
            self.repo
        );
        let raw: GhRelease = self.get_json(&url).await?;
        decode(raw)
    }
}

/// GitHub tags are `v`-prefixed; accept either form from a user or a caller.
pub fn normalize_tag(raw: &str) -> String {
    let t = raw.trim();
    if t.starts_with('v') {
        t.to_string()
    } else {
        format!("v{t}")
    }
}

/// The `owner/repo` used for releases.
///
/// One source: the crate's Cargo `repository` metadata, overridable for
/// forks/tests. No endpoint is spelled out anywhere else in the product.
pub fn github_repo() -> String {
    if let Ok(value) = std::env::var("LEVELER_GITHUB_REPO")
        && !value.trim().is_empty()
    {
        return value.trim().to_string();
    }
    repo_from_url(env!("CARGO_PKG_REPOSITORY")).unwrap_or_else(|| {
        const REPO_URL: &str = env!("CARGO_PKG_REPOSITORY");
        panic!("CARGO_PKG_REPOSITORY must name a GitHub repository, got {REPO_URL}")
    })
}

/// `https://github.com/owner/repo(.git)` → `owner/repo`.
pub fn repo_from_url(url: &str) -> Option<String> {
    let rest = url
        .trim()
        .strip_prefix("https://github.com/")
        .or_else(|| url.trim().strip_prefix("http://github.com/"))
        .or_else(|| url.trim().strip_prefix("git@github.com:"))?;
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let rest = rest.trim_end_matches('/');
    let (owner, repo) = rest.split_once('/')?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(format!("{owner}/{repo}"))
}

/// The HTTP client used for releases and downloads.
///
/// Reuses the process proxy settings (reqwest reads `HTTPS_PROXY` etc.) and
/// sets a finite timeout: a startup check must never hang the product.
pub fn http_client() -> Result<reqwest::Client, UpdateError> {
    let mut builder = reqwest::Client::builder()
        .user_agent(format!("leveler/{}", crate::version::current_version()))
        .timeout(std::time::Duration::from_secs(60))
        .connect_timeout(std::time::Duration::from_secs(15));

    // Optional token raises the unauthenticated rate limit. Never required.
    if let Ok(token) = std::env::var("GITHUB_TOKEN")
        && !token.is_empty()
    {
        let mut headers = reqwest::header::HeaderMap::new();
        let value = format!("Bearer {token}");
        let parsed = value
            .parse()
            .map_err(|_| UpdateError::Validation("GITHUB_TOKEN is not a valid header".into()))?;
        headers.insert(reqwest::header::AUTHORIZATION, parsed);
        headers.insert(
            reqwest::header::ACCEPT,
            "application/vnd.github+json"
                .parse()
                .expect("static header"),
        );
        builder = builder.default_headers(headers);
    }

    builder
        .build()
        .map_err(|e| UpdateError::Network(format!("build HTTP client: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::version::parse_version;

    fn release(tag: &str, assets: &[&str]) -> Release {
        Release {
            tag: tag.to_string(),
            version: parse_version(tag).unwrap(),
            prerelease: false,
            assets: assets
                .iter()
                .map(|name| Asset {
                    name: (*name).to_string(),
                    download_url: format!("https://example.test/{name}"),
                })
                .collect(),
        }
    }

    #[test]
    fn selects_the_exact_asset_and_its_checksum() {
        let rel = release(
            "v1.0.0",
            &[
                "leveler-v1.0.0-aarch64-apple-darwin.tar.gz",
                "leveler-v1.0.0-aarch64-apple-darwin.tar.gz.sha256",
                "leveler-v1.0.0-x86_64-unknown-linux-gnu.tar.gz",
                "leveler-v1.0.0-x86_64-unknown-linux-gnu.tar.gz.sha256",
            ],
        );
        let picked = select_asset(&rel, "aarch64-apple-darwin").unwrap();
        assert_eq!(
            picked.asset_name,
            "leveler-v1.0.0-aarch64-apple-darwin.tar.gz"
        );
        assert!(picked.download_url.ends_with(".tar.gz"));
        assert!(picked.checksum_url.ends_with(".tar.gz.sha256"));
    }

    #[test]
    fn windows_selects_the_zip_never_a_tarball() {
        let rel = release(
            "v1.0.0",
            &[
                "leveler-v1.0.0-x86_64-unknown-linux-gnu.tar.gz",
                "leveler-v1.0.0-x86_64-pc-windows-msvc.zip",
                "leveler-v1.0.0-x86_64-pc-windows-msvc.zip.sha256",
            ],
        );
        let picked = select_asset(&rel, "x86_64-pc-windows-msvc").unwrap();
        assert_eq!(
            picked.asset_name,
            "leveler-v1.0.0-x86_64-pc-windows-msvc.zip"
        );
    }

    #[test]
    fn a_missing_asset_fails_closed() {
        let rel = release("v1.0.0", &["leveler-v1.0.0-aarch64-apple-darwin.tar.gz"]);
        let err = select_asset(&rel, "x86_64-unknown-linux-gnu").unwrap_err();
        assert!(matches!(err, UpdateError::NoAsset { .. }), "{err}");
    }

    #[test]
    fn a_missing_checksum_fails_closed() {
        let rel = release("v1.0.0", &["leveler-v1.0.0-aarch64-apple-darwin.tar.gz"]);
        let err = select_asset(&rel, "aarch64-apple-darwin").unwrap_err();
        assert!(matches!(err, UpdateError::NoChecksum { .. }), "{err}");
    }

    #[test]
    fn a_duplicate_asset_name_is_rejected() {
        let rel = release(
            "v1.0.0",
            &[
                "leveler-v1.0.0-aarch64-apple-darwin.tar.gz",
                "leveler-v1.0.0-aarch64-apple-darwin.tar.gz",
                "leveler-v1.0.0-aarch64-apple-darwin.tar.gz.sha256",
            ],
        );
        let err = select_asset(&rel, "aarch64-apple-darwin").unwrap_err();
        assert!(matches!(err, UpdateError::Validation(_)), "{err}");
    }

    #[test]
    fn repo_url_is_normalized() {
        assert_eq!(
            repo_from_url("https://github.com/dengmengmian/CodeLeveler").as_deref(),
            Some("dengmengmian/CodeLeveler")
        );
        assert_eq!(
            repo_from_url("https://github.com/dengmengmian/CodeLeveler.git").as_deref(),
            Some("dengmengmian/CodeLeveler")
        );
        assert_eq!(
            repo_from_url("git@github.com:dengmengmian/CodeLeveler.git").as_deref(),
            Some("dengmengmian/CodeLeveler")
        );
        assert_eq!(repo_from_url("https://example.com/x/y"), None);
    }

    #[test]
    fn the_crate_repository_is_a_github_repo() {
        // The updater's owner/repo has exactly one source; if this fails, the
        // product has no release source and must not pretend otherwise.
        let repo = repo_from_url(env!("CARGO_PKG_REPOSITORY"));
        assert!(repo.is_some(), "{}", env!("CARGO_PKG_REPOSITORY"));
    }

    /// The observed real-world case: this repository's `v0.1.0-beta.1` was
    /// published with `prerelease: false`, so GitHub's `releases/latest`
    /// returns it. The updater must refuse it on the tag alone.
    #[test]
    fn a_hyphen_tag_is_never_stable_even_without_the_prerelease_flag() {
        let raw = |tag: &str, prerelease: bool, draft: bool| GhRelease {
            tag_name: tag.to_string(),
            prerelease,
            draft,
            assets: Vec::new(),
        };
        assert!(!is_stable_release(&raw("v0.1.0-beta.1", false, false)));
        assert!(!is_stable_release(&raw("v1.0.0-rc.1", false, false)));
        assert!(!is_stable_release(&raw("v1.1.0", true, false)));
        assert!(!is_stable_release(&raw("v1.1.0", false, true)));
        assert!(is_stable_release(&raw("v1.0.0", false, false)));
    }
}
