//! The `update` command and the start-up auto-update.
//!
//! Presentation only: every decision — version ordering, which asset, whether
//! the checksum matches, how to replace the binary — lives in `leveler-update`.
//! This module turns that service into terminal output and drives the one
//! surface that must never block a normal launch.

use std::process::ExitCode;

use leveler_update::{ReleaseSource, UpdatePolicy, UpdateService, UpdateStep};

use crate::output::Line;

/// Replace the process with the just-installed binary, or report why not.
///
/// Called after the TUI has restored the terminal, so the new image never
/// inherits the alternate screen.
pub(crate) fn restart_after_update() {
    if let Err(error) = leveler_update::restart() {
        eprintln!("{}", Line::warn(&format!("restart failed: {error}")));
        eprintln!("  run `leveler` again to use the new version");
    }
}

/// `leveler update`. Always checks; the configured interval does not apply.
pub(crate) async fn cmd_update(
    check: bool,
    force: bool,
    version: Option<String>,
) -> anyhow::Result<ExitCode> {
    let service = UpdateService::production()?;
    run_manual(&service, check, force, version.as_deref()).await
}

/// Testable manual path: resolve, compare, and install against any source.
pub(crate) async fn run_manual<S: ReleaseSource>(
    service: &UpdateService<S>,
    check: bool,
    force: bool,
    version: Option<&str>,
) -> anyhow::Result<ExitCode> {
    let current = service.current().clone();
    let release = match version {
        Some(tag) => service.resolve_tag(tag).await?,
        None => service.latest().await?,
    };
    let newer = leveler_update::should_upgrade(&current, &release.version, force);

    if check {
        println!("{}", Line::heading("CodeLeveler update --check"));
        println!("  current:  v{current}");
        println!("  latest:   v{} ({})", release.version, release.tag);
        if newer {
            println!(
                "{}",
                Line::warn(&format!(
                    "Update available: v{current} → v{}",
                    release.version
                ))
            );
            // The established contract: `--check` exits 2 when an update exists.
            return Ok(ExitCode::from(2));
        }
        println!("{}", Line::ok(&format!("v{current} is up to date.")));
        return Ok(ExitCode::SUCCESS);
    }

    if !newer {
        println!(
            "{}",
            Line::ok(&format!("CodeLeveler v{current} is already up to date."))
        );
        return Ok(ExitCode::SUCCESS);
    }

    let mut bar = ProgressBar::new();
    service.apply(&release, |step| bar.report(step)).await?;
    bar.finish();
    println!("{}", Line::ok(&format!("Updated to v{}.", release.version)));
    Ok(ExitCode::SUCCESS)
}

/// The best-effort start-up check.
///
/// Returns nothing and never fails: an unreachable GitHub, a bad checksum, or
/// a read-only install directory must leave the current version running. A
/// successful check is recorded so the interval is measured from real contact,
/// and a failed attempt is backstopped so restarting in a loop cannot hammer
/// the API.
pub(crate) async fn run_startup_update() {
    let Ok(config) = leveler_app::GlobalConfig::load() else {
        return;
    };
    if !config.update_auto() {
        return;
    }
    let policy = UpdatePolicy {
        auto_update: true,
        check_interval_hours: config.update_check_interval_hours(),
    };
    let mut state = leveler_update::state::load();
    let now = leveler_update::state::now_unix();
    if !state.is_due(policy.interval_hours(), now) {
        return;
    }
    state.mark_attempt(now);
    leveler_update::state::save(&state);

    let service = match UpdateService::production() {
        Ok(service) => service,
        Err(error) => {
            tracing::debug!(%error, "start-up update check skipped");
            return;
        }
    };

    let release = match service.check().await {
        Ok(release) => {
            // A completed query counts even when it found nothing: the next
            // check is an interval away.
            state.mark_success(now);
            leveler_update::state::save(&state);
            release
        }
        Err(error) => {
            // Fail open: a network failure is not the product's problem.
            tracing::debug!(%error, "start-up update check failed");
            return;
        }
    };
    let Some(release) = release else {
        return;
    };

    let mut bar = ProgressBar::new();
    match service.apply(&release, |step| bar.report(step)).await {
        Ok(to) => {
            bar.finish();
            println!("{}", Line::ok(&format!("Updated CodeLeveler to v{to}.")));
            println!("  Restarting…");
            // The terminal is not yet in the alternate screen, so replacing the
            // process here is safe and preserves the original arguments.
            if let Err(error) = leveler_update::restart() {
                tracing::error!(%error, "restart after start-up update failed");
                println!(
                    "{}",
                    Line::warn("restart failed — run `leveler` again to use the new version")
                );
            }
        }
        Err(error) => {
            bar.finish();
            // The user saw an update begin; say why it did not finish. A
            // no-update start prints nothing at all.
            println!(
                "{}",
                Line::warn(&format!(
                    "update skipped, staying on v{}: {error}",
                    service.current()
                ))
            );
            tracing::warn!(%error, "start-up update failed");
        }
    }
}

/// A one-line download progress reporter.
///
/// A real bar only on a terminal; a redirected stream gets a single
/// "Downloading …" line rather than a carriage-return light show. Nothing here
/// invents a percentage the download has not actually reported.
struct ProgressBar {
    announced: bool,
    bar_drawn: bool,
    color: bool,
}

impl ProgressBar {
    fn new() -> Self {
        Self {
            announced: false,
            bar_drawn: false,
            color: console::Term::stdout().is_term(),
        }
    }

    fn report(&mut self, step: UpdateStep) {
        match step {
            UpdateStep::Available { current, latest } => {
                println!("{}", Line::heading("CodeLeveler update"));
                println!("  current:  v{current}");
                println!("  latest:   v{latest}");
            }
            UpdateStep::Downloading {
                asset,
                received,
                total,
            } => {
                if let Some(total) = total.filter(|t| *t > 0)
                    && self.color
                {
                    let width = 20usize;
                    let done =
                        ((received.min(total) as u128 * width as u128) / total as u128) as usize;
                    let bar = format!("{}{}", "█".repeat(done), "░".repeat(width - done));
                    let pct = received.min(total) * 100 / total;
                    print!("\r  Downloading {bar} {pct:>3}%");
                    use std::io::Write;
                    let _ = std::io::stdout().flush();
                    self.bar_drawn = true;
                    return;
                }
                if !self.announced {
                    println!("  Downloading {asset}…");
                    self.announced = true;
                }
            }
            UpdateStep::Verifying => {
                self.newline();
                println!("  Verifying…");
            }
            UpdateStep::Installing => {
                self.newline();
                println!("  Installing…");
            }
            UpdateStep::Installed { to, .. } => {
                self.newline();
                println!("{}", Line::ok(&format!("Updated to v{to}.")));
            }
            UpdateStep::Checking | UpdateStep::UpToDate { .. } => {}
        }
    }

    fn newline(&mut self) {
        if self.bar_drawn {
            println!();
            self.bar_drawn = false;
        }
    }

    fn finish(&mut self) {
        self.newline();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_update::{Asset, Release, UpdateError, parse_version};
    use std::process::ExitCode;

    /// A source with no network: the update policy is what is under test.
    struct FakeSource {
        releases: Vec<Release>,
    }

    #[async_trait::async_trait]
    impl ReleaseSource for FakeSource {
        async fn latest(&self) -> Result<Release, UpdateError> {
            let mut releases = self.releases.clone();
            releases.sort_by(|a, b| a.version.cmp(&b.version));
            releases
                .pop()
                .ok_or_else(|| UpdateError::NoRelease("fake".into()))
        }

        async fn by_tag(&self, tag: &str) -> Result<Release, UpdateError> {
            let wanted = tag.trim().trim_start_matches('v');
            self.releases
                .iter()
                .find(|r| r.version.to_string() == wanted)
                .cloned()
                .ok_or_else(|| UpdateError::NoRelease(format!("fake {tag}")))
        }
    }

    fn release(tag: &str) -> Release {
        Release {
            tag: tag.to_string(),
            version: parse_version(tag).unwrap(),
            prerelease: false,
            assets: vec![Asset {
                name: "unused".into(),
                download_url: "http://127.0.0.1:0/unused".into(),
            }],
        }
    }

    fn service(tags: &[&str], current: &str) -> UpdateService<FakeSource> {
        UpdateService::new(
            FakeSource {
                releases: tags.iter().map(|t| release(t)).collect(),
            },
            reqwest::Client::new(),
        )
        .with_current(parse_version(current).unwrap())
        .with_triple(Some("aarch64-apple-darwin"))
    }

    /// Exit-code contract: a successful no-op is 0; `--check` says 2 when an
    /// update exists, 0 when none does. Failure paths never reach here because
    /// they propagate as `Err` and the top-level handler exits non-zero.
    #[tokio::test]
    async fn already_up_to_date_exits_zero() {
        let service = service(&["v1.0.0"], "1.0.0");
        let code = run_manual(&service, false, false, None).await.unwrap();
        assert_eq!(code, ExitCode::SUCCESS);
    }

    #[tokio::test]
    async fn check_exits_two_when_an_update_exists() {
        let service = service(&["v1.0.0", "v1.0.1"], "1.0.0");
        let code = run_manual(&service, true, false, None).await.unwrap();
        assert_eq!(code, ExitCode::from(2));
    }

    #[tokio::test]
    async fn check_exits_zero_when_already_latest() {
        let service = service(&["v1.0.0"], "1.0.0");
        let code = run_manual(&service, true, false, None).await.unwrap();
        assert_eq!(code, ExitCode::SUCCESS);
    }

    #[tokio::test]
    async fn an_unreachable_release_propagates_as_an_error_not_a_success() {
        let service = UpdateService::new(FakeSource { releases: vec![] }, reqwest::Client::new())
            .with_current(parse_version("1.0.0").unwrap())
            .with_triple(Some("aarch64-apple-darwin"));
        assert!(run_manual(&service, false, false, None).await.is_err());
    }
}
