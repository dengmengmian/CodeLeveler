//! Remote release facts: CI status, the release workflow run, tag, assets.
//!
//! Stage 4 is the only stage whose effect is outside this machine, so it is the
//! only stage that must distinguish "we asked" from "it happened". Everything
//! here reads back what GitHub reports for one exact commit; a dispatch is never
//! treated as a result. A resumed publish may only repeat a remote action whose
//! effect is not already visible.

use std::path::Path;

use crate::error::Fail;
use crate::exec::{self, CommandSpec, Host};
use crate::json::Json;

/// The workflow that must pass for a commit to be releasable.
pub const CI_WORKFLOW_FILE: &str = ".github/workflows/ci.yml";
pub const RELEASE_WORKFLOW_FILE: &str = ".github/workflows/release.yml";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunState {
    Queued,
    InProgress,
    Success,
    Failure,
    Cancelled,
    TimedOut,
    /// Completed with a conclusion that is not a pass and not a cancellation.
    Other(String),
}

impl RunState {
    pub fn label(&self) -> String {
        match self {
            RunState::Queued => "QUEUED".to_string(),
            RunState::InProgress => "IN_PROGRESS".to_string(),
            RunState::Success => "SUCCESS".to_string(),
            RunState::Failure => "FAILURE".to_string(),
            RunState::Cancelled => "CANCELLED".to_string(),
            RunState::TimedOut => "TIMED_OUT".to_string(),
            RunState::Other(text) => text.to_uppercase(),
        }
    }

    pub fn is_terminal_pass(&self) -> bool {
        matches!(self, RunState::Success)
    }

    pub fn is_terminal(&self) -> bool {
        !matches!(self, RunState::Queued | RunState::InProgress)
    }
}

#[derive(Clone, Debug)]
pub struct RunInfo {
    pub id: String,
    pub workflow: String,
    pub event: String,
    pub head_sha: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub url: Option<String>,
    pub created_at: Option<String>,
}

impl RunInfo {
    pub fn from_json(value: &Json) -> RunInfo {
        RunInfo {
            id: match value.get("databaseId").and_then(Json::as_f64) {
                Some(number) => format!("{}", number as i64),
                None => value
                    .at("databaseId")
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            },
            workflow: value
                .at("workflowName")
                .as_str()
                .unwrap_or("unknown")
                .to_string(),
            event: value.at("event").as_str().unwrap_or("").to_string(),
            head_sha: value.at("headSha").as_str().unwrap_or_default().to_string(),
            status: value.at("status").as_str().unwrap_or("unknown").to_string(),
            conclusion: value
                .get("conclusion")
                .and_then(Json::as_str)
                .map(str::to_string),
            url: value.get("url").and_then(Json::as_str).map(str::to_string),
            created_at: value
                .get("createdAt")
                .and_then(Json::as_str)
                .map(str::to_string),
        }
    }

    pub fn state(&self) -> RunState {
        match self.status.as_str() {
            "queued" | "waiting" | "pending" | "requested" => return RunState::Queued,
            "in_progress" => return RunState::InProgress,
            _ => {}
        }
        match self.conclusion.as_deref() {
            Some("success") => RunState::Success,
            Some("failure") | Some("startup_failure") => RunState::Failure,
            Some("cancelled") => RunState::Cancelled,
            Some("timed_out") => RunState::TimedOut,
            Some(other) => RunState::Other(other.to_string()),
            None => RunState::InProgress,
        }
    }
}

#[derive(Clone, Debug)]
pub struct JobInfo {
    pub name: String,
    pub conclusion: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ReleaseInfo {
    pub tag: String,
    pub is_draft: bool,
    pub is_prerelease: bool,
    pub url: Option<String>,
    pub assets: Vec<AssetInfo>,
}

#[derive(Clone, Debug)]
pub struct AssetInfo {
    pub name: String,
    pub state: String,
}

/// `gh run list` for one exact commit. Nothing else is acceptable: a run for a
/// different SHA says nothing about this candidate.
pub fn runs_for_commit(
    host: &dyn Host,
    repo: &Path,
    slug: &str,
    sha: &str,
) -> Result<Vec<RunInfo>, Fail> {
    let fields = "databaseId,workflowName,event,headSha,status,conclusion,url,createdAt";
    let out = host.run(&CommandSpec::new(
        "gh",
        [
            "run", "list", "--repo", slug, "--commit", sha, "--limit", "50", "--json", fields,
        ],
        repo,
    ))?;
    if out.status != 0 {
        return Err(Fail::new(
            "CI_QUERY_FAILED",
            format!(
                "gh run list --commit {sha} exited {}\n{}",
                out.status,
                exec::first_lines(&out.stderr, 20)
            ),
        ));
    }
    let doc = Json::parse(&out.stdout).map_err(|err| {
        Fail::new(
            "CI_QUERY_FAILED",
            format!("could not read gh run list output: {err}"),
        )
    })?;
    let runs = doc
        .as_arr()
        .ok_or_else(|| Fail::new("CI_QUERY_FAILED", "gh run list did not return an array"))?;
    let mut parsed: Vec<RunInfo> = runs.iter().map(RunInfo::from_json).collect();
    // Every run in this list is for `sha`; a run whose headSha disagrees is a
    // contradiction in the source of truth, not something to filter away.
    if let Some(bad) = parsed
        .iter()
        .find(|run| !run.head_sha.is_empty() && !run.head_sha.starts_with(sha))
    {
        return Err(Fail::new(
            "CI_SHA_MISMATCH",
            format!(
                "gh returned a run for {} when asked about {sha}; the release would not be pinned to its commit",
                bad.head_sha
            ),
        ));
    }
    parsed.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    Ok(parsed)
}

pub fn run_view(host: &dyn Host, repo: &Path, slug: &str, id: &str) -> Result<RunInfo, Fail> {
    let out = host.run(&CommandSpec::new(
        "gh",
        [
            "run",
            "view",
            id,
            "--repo",
            slug,
            "--json",
            "databaseId,status,conclusion,headSha,url,event,workflowName,createdAt",
        ],
        repo,
    ))?;
    if out.status != 0 {
        return Err(Fail::new(
            "CI_QUERY_FAILED",
            format!(
                "gh run view {id} exited {}\n{}",
                out.status,
                exec::first_lines(&out.stderr, 20)
            ),
        ));
    }
    let doc = Json::parse(&out.stdout).map_err(|err| {
        Fail::new(
            "CI_QUERY_FAILED",
            format!("could not read gh run view: {err}"),
        )
    })?;
    Ok(RunInfo::from_json(&doc))
}

pub fn run_jobs(host: &dyn Host, repo: &Path, slug: &str, id: &str) -> Result<Vec<JobInfo>, Fail> {
    let out = host.run(&CommandSpec::new(
        "gh",
        ["run", "view", id, "--repo", slug, "--json", "jobs"],
        repo,
    ))?;
    if out.status != 0 {
        return Err(Fail::new(
            "CI_QUERY_FAILED",
            format!(
                "gh run view {id} --json jobs exited {}\n{}",
                out.status,
                exec::first_lines(&out.stderr, 20)
            ),
        ));
    }
    let doc = Json::parse(&out.stdout)
        .map_err(|err| Fail::new("CI_QUERY_FAILED", format!("could not read gh jobs: {err}")))?;
    let jobs = doc.at("jobs").as_arr().unwrap_or(&[]);
    Ok(jobs
        .iter()
        .map(|job| JobInfo {
            name: job.at("name").as_str().unwrap_or("job").to_string(),
            conclusion: job
                .get("conclusion")
                .and_then(Json::as_str)
                .map(str::to_string),
        })
        .collect())
}

pub fn release_view(
    host: &dyn Host,
    repo: &Path,
    slug: &str,
    tag: &str,
) -> Result<Option<ReleaseInfo>, Fail> {
    let out = host.run(&CommandSpec::new(
        "gh",
        [
            "release",
            "view",
            tag,
            "--repo",
            slug,
            "--json",
            "tagName,isDraft,isPrerelease,url,assets",
        ],
        repo,
    ))?;
    if out.status != 0 {
        let stderr = out.stderr.to_lowercase();
        if stderr.contains("not found") || stderr.contains("could not find") {
            return Ok(None);
        }
        return Err(Fail::new(
            "RELEASE_QUERY_FAILED",
            format!(
                "gh release view {tag} exited {}\n{}",
                out.status,
                exec::first_lines(&out.stderr, 20)
            ),
        ));
    }
    let doc = Json::parse(&out.stdout).map_err(|err| {
        Fail::new(
            "RELEASE_QUERY_FAILED",
            format!("could not read gh release view: {err}"),
        )
    })?;
    let assets = doc
        .at("assets")
        .as_arr()
        .unwrap_or(&[])
        .iter()
        .map(|asset| AssetInfo {
            name: asset.at("name").as_str().unwrap_or_default().to_string(),
            state: asset.at("state").as_str().unwrap_or("").to_string(),
        })
        .collect();
    Ok(Some(ReleaseInfo {
        tag: doc.at("tagName").as_str().unwrap_or(tag).to_string(),
        is_draft: doc.at("isDraft").as_bool().unwrap_or(false),
        is_prerelease: doc.at("isPrerelease").as_bool().unwrap_or(false),
        url: doc.get("url").and_then(Json::as_str).map(str::to_string),
        assets,
    }))
}

pub fn dispatch_release(
    host: &dyn Host,
    repo: &Path,
    slug: &str,
    branch: &str,
    version: &str,
    sha: &str,
) -> Result<(), Fail> {
    let out = host.run(&CommandSpec::new(
        "gh",
        [
            "workflow",
            "run",
            "release.yml",
            "--repo",
            slug,
            "--ref",
            branch,
            "-f",
            &format!("version={version}"),
            "-f",
            &format!("commit={sha}"),
        ],
        repo,
    ))?;
    if out.status != 0 {
        return Err(Fail::new(
            "RELEASE_DISPATCH_FAILED",
            format!(
                "gh workflow run release.yml exited {}\n{}",
                out.status,
                exec::first_lines(&out.stderr, 20)
            ),
        ));
    }
    Ok(())
}

/// Remote tag fact. `None` means the tag does not exist on origin yet.
pub fn remote_tag_sha(host: &dyn Host, repo: &Path, tag: &str) -> Result<Option<String>, Fail> {
    let out = host.run(&exec::git(
        repo,
        &[
            "ls-remote",
            "origin",
            &format!("refs/tags/{tag}"),
            &format!("refs/tags/{tag}^{{}}"),
        ],
    ))?;
    if out.status != 0 {
        return Err(Fail::new(
            "GIT_FAILED",
            format!(
                "git ls-remote failed\n{}",
                exec::first_lines(&out.stderr, 20)
            ),
        ));
    }
    // Prefer the peeled commit: an annotated tag's first line is the tag object.
    let mut peeled = None;
    let mut direct = None;
    for line in out.stdout.lines() {
        let Some((sha, _)) = line.split_once('\t').or_else(|| line.split_once(' ')) else {
            continue;
        };
        let sha = sha.trim();
        if sha.is_empty() {
            continue;
        }
        if line.contains("^{}") {
            peeled = Some(sha.to_string());
        } else if direct.is_none() {
            direct = Some(sha.to_string());
        }
    }
    Ok(peeled.or(direct))
}

/// Download the published checksum files, then the artifacts they cover.
pub fn download_assets(
    host: &dyn Host,
    repo: &Path,
    slug: &str,
    tag: &str,
    dir: &Path,
    pattern: &str,
) -> Result<(), Fail> {
    std::fs::create_dir_all(dir).map_err(|err| {
        Fail::new(
            "RELEASE_STATE_IO",
            format!("could not create {}: {err}", dir.display()),
        )
    })?;
    let out = host.run(&CommandSpec::new(
        "gh",
        [
            "release",
            "download",
            tag,
            "--repo",
            slug,
            "--dir",
            &dir.display().to_string(),
            "--clobber",
            "--pattern",
            pattern,
        ],
        repo,
    ))?;
    if out.status != 0 {
        return Err(Fail::new(
            "RELEASE_ASSET_DOWNLOAD_FAILED",
            format!(
                "gh release download {tag} --pattern {pattern} exited {}\n{}",
                out.status,
                exec::first_lines(&out.stderr, 20)
            ),
        ));
    }
    Ok(())
}

/// The `name:` of a workflow file, so the CI check names the workflow GitHub
/// actually runs instead of one this tool guessed.
pub fn workflow_name(repo: &Path, relative: &str) -> Result<String, Fail> {
    let path = repo.join(relative);
    let text = std::fs::read_to_string(&path).map_err(|err| {
        Fail::new(
            "WORKFLOW_MISSING",
            format!("could not read {}: {err}", path.display()),
        )
    })?;
    for line in text.lines() {
        let trimmed = line.trim_end();
        if let Some(rest) = trimmed.strip_prefix("name:") {
            let name = rest.trim().trim_matches('"').trim_matches('\'').to_string();
            if !name.is_empty() {
                return Ok(name);
            }
        }
    }
    Err(Fail::new(
        "WORKFLOW_MISSING",
        format!("{relative} has no top-level name:"),
    ))
}

/// Whether a workflow file reacts to a push on the release branch. A workflow
/// that only runs on pull requests cannot qualify a commit.
pub fn triggers_on_push(repo: &Path, relative: &str) -> Result<bool, Fail> {
    let path = repo.join(relative);
    let text = std::fs::read_to_string(&path).map_err(|err| {
        Fail::new(
            "WORKFLOW_MISSING",
            format!("could not read {}: {err}", path.display()),
        )
    })?;
    let mut in_on = false;
    for line in text.lines() {
        if line.starts_with("on:") {
            in_on = true;
            continue;
        }
        if in_on {
            if !line.starts_with(' ') && !line.trim().is_empty() {
                break;
            }
            if line.trim_start().starts_with("push:") {
                return Ok(true);
            }
        }
    }
    Ok(false)
}
