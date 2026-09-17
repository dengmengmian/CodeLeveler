//! CLI surface for the in-repo config trust gate.
//!
//! `<repo>/.leveler/hooks.yaml` runs commands before every tool call and
//! `<repo>/.leveler/permissions.yaml` can grant standing approval, so both are
//! inert until the user trusts that repository's exact file contents. This is
//! where the human does that — the agent has no tool for it, and the workspace
//! layer refuses writes to either file.

use std::io::IsTerminal;
use std::path::PathBuf;

use leveler_execution::{
    TRUSTED_PROJECT_FILES, TrustStore, TrustedRead, read_trusted_project_file,
};
use leveler_project::Layout;

use crate::cli::TrustCommand;
use crate::output::Line;

/// One in-repo config file and where it stands with the trust store.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FileState {
    Absent,
    Trusted { digest: String },
    Untrusted { digest: String },
}

pub(crate) fn cmd_trust(
    layout: Layout,
    cmd: Option<TrustCommand>,
) -> anyhow::Result<std::process::ExitCode> {
    let home = leveler_home();
    let repo = layout.repo_root.clone();

    match cmd {
        Some(TrustCommand::Revoke) => {
            let mut store = TrustStore::load(&home);
            if store.revoke(&repo) {
                store.save()?;
                println!(
                    "{}",
                    Line::ok(&format!("revoked trust for {}", repo.display()))
                );
            } else {
                println!("{}", Line::warn("this repository had no trusted files"));
            }
            Ok(std::process::ExitCode::SUCCESS)
        }
        Some(TrustCommand::Show) => {
            print_status(&home, &repo);
            Ok(std::process::ExitCode::SUCCESS)
        }
        // Bare `leveler trust` is `trust allow` with the prompt.
        other => {
            let yes = matches!(other, Some(TrustCommand::Allow { yes: true }));

            // Trust granted from a store the repository itself contains means
            // nothing, and the gate ignores it. Say so instead of reporting a
            // write that will never take effect.
            if !leveler_execution::store_is_outside_repo(&home, &repo) {
                println!(
                    "{}",
                    Line::fail(&format!(
                        "the trust store ({}) is inside this repository, so trusting it would grant nothing",
                        trust_store_display(&home)
                    ))
                );
                println!("  set HOME (or LEVELER_HOME) to a directory outside the repository.");
                return Ok(std::process::ExitCode::FAILURE);
            }

            let states = states(&home, &repo);
            let pending: Vec<_> = states
                .iter()
                .filter(|(_, state)| matches!(state, FileState::Untrusted { .. }))
                .collect();

            if pending.is_empty() {
                print_status(&home, &repo);
                println!();
                println!("{}", Line::ok("nothing to trust"));
                return Ok(std::process::ExitCode::SUCCESS);
            }

            println!("{}", Line::heading("Untrusted in-repo configuration"));
            for (relative, state) in &pending {
                let FileState::Untrusted { digest } = state else {
                    continue;
                };
                println!("  {}  sha256:{}", relative, short(digest));
                println!("    {}", purpose(relative));
            }
            println!();

            // A trust decision is the user's, so a non-interactive run must not
            // make it silently — `--yes` is the explicit non-interactive path.
            if !yes && !std::io::stdin().is_terminal() {
                println!(
                    "{}",
                    Line::warn(
                        "not a terminal — re-run with `leveler trust allow --yes` to confirm"
                    )
                );
                return Ok(std::process::ExitCode::FAILURE);
            }
            if !yes && !confirm("Trust these files for this repository? [y/N] ")? {
                println!("{}", Line::warn("left untrusted"));
                return Ok(std::process::ExitCode::SUCCESS);
            }

            let mut store = TrustStore::load(&home);
            let mut trusted = 0usize;
            for (relative, _) in &pending {
                // Re-read at write time so the digest recorded is the file's
                // current bytes, not a stale one from the listing above.
                if let Ok(body) = std::fs::read(repo.join(relative)) {
                    store.trust(&repo, relative, &body);
                    trusted += 1;
                }
            }
            store.save()?;
            println!(
                "{}",
                Line::ok(&format!(
                    "trusted {trusted} file(s) for {} (re-run after any edit)",
                    repo.display()
                ))
            );
            Ok(std::process::ExitCode::SUCCESS)
        }
    }
}

/// Ignored in-repo config as repo-relative display paths, for the TUI.
///
/// The startup notice goes to stderr, which the alternate screen swallows, so
/// the TUI has to carry this itself — see `leveler_tui::Boot::untrusted_config`.
/// Paths are relative because they share a width-limited box column.
pub(crate) fn untrusted_config_display(repo_root: &std::path::Path) -> Vec<String> {
    let home = leveler_home();
    leveler_execution::untrusted_project_files(&home, repo_root)
        .into_iter()
        .map(|entry| {
            entry
                .path
                .strip_prefix(repo_root)
                .unwrap_or(&entry.path)
                .display()
                .to_string()
        })
        .collect()
}

/// State of every gated file for a repository, in a stable order.
pub(crate) fn states(home: &std::path::Path, repo: &std::path::Path) -> Vec<(String, FileState)> {
    TRUSTED_PROJECT_FILES
        .iter()
        .map(|relative| {
            let state = match read_trusted_project_file(home, repo, relative) {
                TrustedRead::Absent => FileState::Absent,
                TrustedRead::Trusted(raw) => FileState::Trusted {
                    digest: leveler_execution::content_digest(raw.as_bytes()),
                },
                TrustedRead::Untrusted { digest, .. } => FileState::Untrusted { digest },
            };
            ((*relative).to_string(), state)
        })
        .collect()
}

fn print_status(home: &std::path::Path, repo: &std::path::Path) {
    println!("{}  {}", Line::heading("Repository"), repo.display());
    println!();
    for (relative, state) in states(home, repo) {
        match state {
            FileState::Absent => println!("  {relative}\n    (no file)"),
            FileState::Trusted { digest } => println!(
                "  {}\n    {} sha256:{}",
                relative,
                Line::ok("trusted"),
                short(&digest)
            ),
            FileState::Untrusted { digest } => println!(
                "  {}\n    {} sha256:{} — {}",
                relative,
                Line::warn("untrusted (ignored)"),
                short(&digest),
                purpose(&relative)
            ),
        }
    }
}

/// Why trusting this file matters, in the user's terms.
fn purpose(relative: &str) -> &'static str {
    if relative.ends_with("hooks.yaml") {
        "runs commands before every tool call"
    } else {
        "can grant standing approval for tool calls"
    }
}

fn short(digest: &str) -> String {
    digest.chars().take(12).collect()
}

fn confirm(prompt: &str) -> anyhow::Result<bool> {
    use std::io::Write as _;
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "YES"))
}

fn trust_store_display(home: &std::path::Path) -> String {
    leveler_execution::trust_store_path(home)
        .display()
        .to_string()
}

fn leveler_home() -> PathBuf {
    leveler_core::LevelerHome::resolve(leveler_core::environment())
        .root()
        .to_path_buf()
}
