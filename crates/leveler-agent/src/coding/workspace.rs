//! Where the work happens, as the checkpoint records it.
//!
//! Git is a Coding concern, so the engine asks for these facts through a port
//! and this is the implementation behind it. Bounded on purpose: branch, head,
//! dirty flag and a capped list of changed paths — never a diff, never file
//! contents.

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use leveler_engine::WorkspaceFacts;
use leveler_lifecycle::CheckpointWorkspace;

/// How many changed paths a checkpoint records before it stops listing.
const MAX_REFS: usize = 20;

/// The repository a Coding task runs in.
pub struct GitWorkspace {
    repo: PathBuf,
}

impl GitWorkspace {
    pub fn new(repo: impl Into<PathBuf>) -> Self {
        Self { repo: repo.into() }
    }
}

#[async_trait]
impl WorkspaceFacts for GitWorkspace {
    async fn capture(&self) -> CheckpointWorkspace {
        capture_workspace(&self.repo).await
    }
}

/// Bounded git metadata. Every failure yields `None` — unknown, never an
/// assumed-clean workspace. Never captures diffs or file contents.
pub(crate) async fn capture_workspace(repo: &Path) -> CheckpointWorkspace {
    let head = git_line(repo, &["rev-parse", "HEAD"]).await;
    let branch = git_line(repo, &["rev-parse", "--abbrev-ref", "HEAD"]).await;
    let status = git_output(repo, &["status", "--porcelain"]).await;
    let (dirty, changed_paths) = match status {
        Some(text) => {
            let paths: Vec<String> = text
                .lines()
                .filter(|l| l.len() > 3)
                .take(MAX_REFS)
                .map(|l| l[3..].trim().to_string())
                .collect();
            (Some(!text.trim().is_empty()), paths)
        }
        None => (None, Vec::new()),
    };
    CheckpointWorkspace {
        branch,
        head,
        dirty,
        changed_paths,
    }
}

async fn git_line(repo: &Path, args: &[&str]) -> Option<String> {
    let text = git_output(repo, args).await?;
    let line = text.trim().to_string();
    (!line.is_empty()).then_some(line)
}

async fn git_output(repo: &Path, args: &[&str]) -> Option<String> {
    let repo = repo.to_path_buf();
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        leveler_core::git_stdout(&repo, &args)
    })
    .await
    .ok()?
}
