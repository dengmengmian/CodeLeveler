//! The update lifecycle, owned in one place.
//!
//! The CLI, the start-up check, and the TUI's `/update` all drive this service.
//! It resolves a release, downloads it, verifies it, installs it, and reports
//! each step; it never restarts the process (the caller owns that, because the
//! terminal must be restored first).

use std::path::PathBuf;

use crate::error::UpdateError;
use crate::install::{download, download_text, install_from_archive, verify_sha256};
use crate::source::{GitHubReleaseSource, Release, ReleaseSource, select_asset};
use crate::target::host_target_triple;
use crate::version::{Version, current_version, should_upgrade};

/// One observable step of an update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateStep {
    /// Asking GitHub for the latest stable release.
    Checking,
    /// A newer stable release exists.
    Available { current: Version, latest: Version },
    /// Already on the newest stable release.
    UpToDate { current: Version },
    /// Streaming the release asset.
    Downloading {
        asset: String,
        received: u64,
        total: Option<u64>,
    },
    /// Comparing the download against the published SHA-256.
    Verifying,
    /// Unpacking, validating and replacing the executable.
    Installing,
    /// The running binary has been replaced on disk.
    Installed { from: Version, to: Version },
}

/// What an update run ended up doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateOutcome {
    UpToDate { current: Version },
    Installed { from: Version, to: Version },
}

impl UpdateOutcome {
    pub fn installed(&self) -> bool {
        matches!(self, Self::Installed { .. })
    }
}

/// Drives one release source for one running version.
pub struct UpdateService<S: ReleaseSource> {
    source: S,
    client: reqwest::Client,
    current: Version,
    triple: Option<&'static str>,
}

impl UpdateService<GitHubReleaseSource> {
    /// The production service: GitHub releases for the configured repo.
    pub fn production() -> Result<Self, UpdateError> {
        let client = crate::source::http_client()?;
        let repo = crate::source::github_repo();
        Ok(Self {
            source: GitHubReleaseSource::new(repo, client.clone()),
            client,
            current: current_version(),
            triple: host_target_triple(),
        })
    }
}

impl<S: ReleaseSource> UpdateService<S> {
    pub fn new(source: S, client: reqwest::Client) -> Self {
        Self {
            source,
            client,
            current: current_version(),
            triple: host_target_triple(),
        }
    }

    /// Override the running version (tests).
    pub fn with_current(mut self, current: Version) -> Self {
        self.current = current;
        self
    }

    /// Override the host triple (tests).
    pub fn with_triple(mut self, triple: Option<&'static str>) -> Self {
        self.triple = triple;
        self
    }

    pub fn current(&self) -> &Version {
        &self.current
    }

    pub fn triple(&self) -> Option<&'static str> {
        self.triple
    }

    /// The latest stable release, regardless of whether it is newer.
    pub async fn latest(&self) -> Result<Release, UpdateError> {
        self.source.latest().await
    }

    /// A specific release by tag (accepts a pre-release, for explicit installs).
    pub async fn resolve_tag(&self, tag: &str) -> Result<Release, UpdateError> {
        self.source.by_tag(tag).await
    }

    /// The latest stable release when it is newer than the running one.
    pub async fn check(&self) -> Result<Option<Release>, UpdateError> {
        let release = self.latest().await?;
        Ok(should_upgrade(&self.current, &release.version, false).then_some(release))
    }

    /// Download, verify, unpack, validate, and replace. Returns the installed
    /// version. Emits progress but never restarts.
    pub async fn apply(
        &self,
        release: &Release,
        mut on_step: impl FnMut(UpdateStep),
    ) -> Result<Version, UpdateError> {
        let triple = self.triple.ok_or_else(|| {
            UpdateError::Unsupported(format!(
                "no published binary for {} / {}",
                std::env::consts::OS,
                std::env::consts::ARCH
            ))
        })?;
        let assets = select_asset(release, triple)?;

        on_step(UpdateStep::Available {
            current: self.current.clone(),
            latest: release.version.clone(),
        });

        let asset_name = assets.asset_name.clone();
        let bytes = download(&self.client, &assets.download_url, |received, total| {
            on_step(UpdateStep::Downloading {
                asset: asset_name.clone(),
                received,
                total,
            });
        })
        .await?;

        on_step(UpdateStep::Verifying);
        let checksum = download_text(&self.client, &assets.checksum_url).await?;
        verify_sha256(&bytes, &checksum, &assets.asset_name)?;

        on_step(UpdateStep::Installing);
        let current_exe = std::env::current_exe().map_err(|e| UpdateError::Io(e.to_string()))?;
        let current_exe = std::fs::canonicalize(&current_exe).unwrap_or(current_exe);
        let install_dir = current_exe
            .parent()
            .map(PathBuf::from)
            .ok_or_else(|| UpdateError::Unsupported("executable has no parent directory".into()))?;

        let archive_dir = tempfile::tempdir().map_err(|e| UpdateError::Io(e.to_string()))?;
        let archive_path = archive_dir.path().join(&assets.asset_name);
        std::fs::write(&archive_path, &bytes).map_err(|e| UpdateError::Io(e.to_string()))?;
        install_from_archive(&archive_path, &install_dir, triple, &release.version)?;

        on_step(UpdateStep::Installed {
            from: self.current.clone(),
            to: release.version.clone(),
        });
        Ok(release.version.clone())
    }

    /// Check and, when newer, install. The whole manual/startup pipeline.
    pub async fn run(
        &self,
        force: bool,
        mut on_step: impl FnMut(UpdateStep),
    ) -> Result<UpdateOutcome, UpdateError> {
        on_step(UpdateStep::Checking);
        let release = if force {
            self.latest().await?
        } else {
            match self.check().await? {
                Some(release) => release,
                None => {
                    on_step(UpdateStep::UpToDate {
                        current: self.current.clone(),
                    });
                    return Ok(UpdateOutcome::UpToDate {
                        current: self.current.clone(),
                    });
                }
            }
        };
        let to = self.apply(&release, &mut on_step).await?;
        Ok(UpdateOutcome::Installed {
            from: self.current.clone(),
            to,
        })
    }
}
