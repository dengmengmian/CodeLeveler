use std::path::Path;

use crate::error::Fail;
use crate::exec::{self, Host};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub head: String,
    pub porcelain: String,
}

pub fn snapshot(host: &dyn Host, repo: &Path) -> Result<Snapshot, Fail> {
    let head = exec::ok_stdout(host, exec::git(repo, &["rev-parse", "HEAD"]), "GIT_FAILED")?;
    let status = host.run(&exec::git(repo, &["status", "--porcelain"]))?;
    if status.status != 0 {
        return Err(Fail::new(
            "GIT_FAILED",
            format!("git status failed\n{}", status.stderr.trim()),
        ));
    }
    Ok(Snapshot {
        head,
        porcelain: status.stdout.trim_end_matches(['\n', '\r']).to_string(),
    })
}

/// The candidate commit and the worktree must still be the snapshot we locked.
pub fn confirm(host: &dyn Host, repo: &Path, locked: &Snapshot) -> Result<(), Fail> {
    let now = snapshot(host, repo)?;
    if now.head != locked.head {
        return Err(Fail::new(
            "CANDIDATE_CHANGED",
            format!("candidate changed from {} to {}", locked.head, now.head),
        ));
    }
    if now.porcelain != locked.porcelain {
        return Err(Fail::new(
            "WORKTREE_CHANGED",
            "the working tree changed while this candidate was being verified",
        ));
    }
    Ok(())
}

pub fn require_clean(snap: &Snapshot) -> Result<(), Fail> {
    if snap.porcelain.is_empty() {
        Ok(())
    } else {
        Err(Fail::new(
            "WORKTREE_DIRTY",
            "the candidate is not a frozen commit; commit or discard this worktree first\n\
             ./dev does not stash, reset, or clean unrelated files",
        ))
    }
}
