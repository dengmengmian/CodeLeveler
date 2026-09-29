use std::path::Path;

use crate::error::{Fail, Report};
use crate::exec::{self, CommandSpec, Host};
use crate::git;
use crate::qualify;
use crate::root::{self, Env, GateOpts};
use crate::version::{self, Level};

struct Opts {
    level: Level,
    dry_run: bool,
    gate: GateOpts,
}

const VERSION_FILES: [&str; 4] = [
    "Cargo.toml",
    "Cargo.lock",
    "docs/RELEASE.md",
    "docs/RELEASE.zh-CN.md",
];

pub fn run(host: &dyn Host, repo: &Path, env: &Env, args: &[String]) -> Result<Report, Fail> {
    let opts = parse_args(args)?;
    let snap = git::snapshot(host, repo)?;
    git::require_clean(&snap)?;
    let branch = exec::ok_stdout(
        host,
        exec::git(repo, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "GIT_FAILED",
    )?;
    if branch == "HEAD" {
        return Err(Fail::new(
            "DETACHED_HEAD",
            "checkout is detached; publish needs a branch",
        ));
    }
    let remotes = exec::ok_stdout(host, exec::git(repo, &["remote"]), "GIT_FAILED")?;
    if !remotes.lines().any(|line| line.trim() == "origin") {
        return Err(Fail::new("REMOTE_MISSING", "git remote has no origin"));
    }
    let manifest = std::fs::read_to_string(repo.join("Cargo.toml")).map_err(|err| {
        Fail::new(
            "VERSION_SOURCE_MISSING",
            format!("could not read Cargo.toml: {err}"),
        )
    })?;
    let local = version::read_local(repo)?;
    version::require_local(&local)?;
    let current = local.workspace;
    let next = version::bump(current, opts.level)?;
    let tag = format!("v{next}");
    let tags = exec::ok_stdout(host, exec::git(repo, &["tag", "--list"]), "GIT_FAILED")?;
    if version::tag_listed(&tags, &tag) {
        return Err(Fail::new(
            "TAG_EXISTS",
            format!("{tag} already exists locally"),
        ));
    }
    let slug = version::github_slug(&manifest)?;
    let releases = github_releases(host, repo, &slug)?;
    version::require_publish(&local, &tags, &releases)?;
    if opts.dry_run {
        return Ok(Report::ok(format!(
            "current: {current}\n\
             next: {next}\n\
             candidate: {}\n\
             would-push: git push origin HEAD\n\
             would-dispatch: gh workflow run release.yml --repo {slug} --ref {branch} -f version={next} -f commit=<the version commit>\n\
             remote tag check: skipped\n\
             WOULD_RUN: pre-release\n\
             WOULD_RUN: dogfood\n\
             READY_TO_PUBLISH: NO\n\
             note: the tag and the GitHub release are created by the release workflow
             after every artifact verifies, at the commit that was built; the
             qualified commit does not exist until publish bumps the version",
            snap.head
        )));
    }
    let remote_tag = format!("refs/tags/{tag}");
    let remote = host.run(&exec::git(
        repo,
        &["ls-remote", "--tags", "origin", &remote_tag],
    ))?;
    if remote.status != 0 {
        return Err(Fail::new(
            "GIT_FAILED",
            format!(
                "git ls-remote failed\n{}",
                exec::first_lines(&remote.stderr, 20)
            ),
        ));
    }
    if version::tag_listed(&remote.stdout, &tag) {
        return Err(Fail::new(
            "TAG_EXISTS",
            format!("{tag} already exists on origin"),
        ));
    }
    write_version_files(repo, &manifest, next)?;
    let metadata = host.run(&CommandSpec::new(
        "cargo",
        ["metadata", "--format-version", "1"],
        repo,
    ))?;
    if metadata.status != 0 {
        return Err(Fail::new(
            "TOOL_FAILED",
            format!(
                "cargo metadata exited {}\n{}\nversion files may already be modified\n./dev did not reset them",
                metadata.status,
                exec::first_lines(&metadata.stderr, 20)
            ),
        ));
    }
    let dirty = host.run(&exec::git(repo, &["status", "--porcelain"]))?;
    if dirty.status != 0 {
        return Err(Fail::new(
            "GIT_FAILED",
            format!("git status failed\n{}", dirty.stderr.trim()),
        ));
    }
    let paths = porcelain_paths(&dirty.stdout);
    let unexpected: Vec<_> = paths
        .iter()
        .filter(|path| !VERSION_FILES.contains(&path.as_str()))
        .cloned()
        .collect();
    if !unexpected.is_empty() {
        return Err(Fail::new(
            "PUBLISH_UNEXPECTED_MUTATION",
            format!(
                "version bump changed files outside the version set: {}\n./dev did not commit or reset",
                unexpected.join(", ")
            ),
        ));
    }
    let mut add = vec!["add".to_string(), "--".to_string()];
    add.extend(VERSION_FILES.iter().map(|path| (*path).to_string()));
    let add_refs: Vec<&str> = add.iter().map(String::as_str).collect();
    let added = host.run(&exec::git(repo, &add_refs))?;
    if added.status != 0 {
        return Err(Fail::new(
            "COMMIT_FAILED",
            format!(
                "git add failed\n{}\nversion files may already be modified\n./dev did not reset them",
                exec::first_lines(&added.stderr, 20)
            ),
        ));
    }
    let message = format!("release: v{next}");
    let committed = host.run(&exec::git(repo, &["commit", "-m", &message]))?;
    if committed.status != 0 {
        return Err(Fail::new(
            "COMMIT_FAILED",
            format!(
                "git commit failed\n{}\nversion files may already be modified\n./dev did not reset them",
                exec::first_lines(&committed.stderr, 40)
            ),
        ));
    }
    let published = git::snapshot(host, repo)?;
    let _pre = qualify::run_pre_release_locked(host, repo, env, &opts.gate, &published)
        .map_err(|err| after_commit(err, &published.head))?;
    let qual = qualify::run_dogfood_locked(host, repo, env, &opts.gate, &published)
        .map_err(|err| after_commit(err, &published.head))?;
    if qual.exit_code != 0 && qual.exit_code != 2 {
        let rc = qual.rc_exit.map(qualify::verdict).unwrap_or("NOT_STARTED");
        return Ok(Report::with_code(
            qual.exit_code,
            format!(
                "candidate: {}\n\
                 Dogfood: {}\n\
                 RC Qualification: {rc}\n\
                 READY_TO_PUBLISH: NO\n\
                 qualification failed after the version commit\n\
                 local commit: {}\n\
                 ./dev did not tag, push, or reset this commit",
                published.head,
                qualify::verdict(qual.release_exit),
                published.head
            ),
        ));
    }
    git::confirm(host, repo, &published)?;
    // Push the version commit first: the release workflow dispatches on the
    // default branch, and its cache lives there so the next version can restore
    // it. Tag and release are created by the workflow, after the build.
    push_ref(
        host,
        repo,
        &["push", "origin", "HEAD"],
        &format!(
            "release commit {} was not pushed to origin\n./dev did not tag, dispatch, or reset this commit",
            published.head
        ),
    )?;
    dispatch_release(host, repo, &slug, &branch, next, &published.head)?;
    let ready = if qual.exit_code == 2 {
        "CONDITIONAL"
    } else {
        "YES"
    };
    Ok(Report::with_code(
        qual.exit_code,
        format!(
            "current: {current}\n\
             next: {next}\n\
             candidate: {}\n\
             tag: {tag} (created by the release workflow after every artifact verifies)\n\
             Release Gate: PASS\n\
             Dogfood: {}\n\
             RC Qualification: {}\n\
             pushed: origin HEAD\n\
             dispatched: release.yml version={next} commit={}\n\
             READY_TO_PUBLISH: {ready}",
            published.head,
            qualify::verdict(qual.release_exit),
            qual.rc_exit.map(qualify::verdict).unwrap_or("NOT_STARTED"),
            published.head
        ),
    ))
}

fn parse_args(args: &[String]) -> Result<Opts, Fail> {
    let (gate, rest) = root::extract_gate_opts(args)?;
    let mut level = None;
    let mut dry_run = false;
    for arg in &rest {
        match arg.as_str() {
            "--dry-run" => dry_run = true,
            "--patch" => set_level(&mut level, Level::Patch)?,
            "--minor" => set_level(&mut level, Level::Minor)?,
            "--major" => set_level(&mut level, Level::Major)?,
            other => {
                return Err(
                    Fail::new("UNKNOWN_COMMAND", format!("unknown argument: {other}")).exit(2),
                );
            }
        }
    }
    Ok(Opts {
        level: level.unwrap_or(Level::Patch),
        dry_run,
        gate,
    })
}

fn set_level(slot: &mut Option<Level>, level: Level) -> Result<(), Fail> {
    if let Some(existing) = *slot
        && existing != level
    {
        return Err(Fail::new(
            "PUBLISH_LEVEL_CONFLICT",
            "pass only one of --patch, --minor, --major",
        ));
    }
    *slot = Some(level);
    Ok(())
}

fn github_releases(host: &dyn Host, repo: &Path, slug: &str) -> Result<String, Fail> {
    let spec = CommandSpec::new(
        "gh",
        [
            "release",
            "list",
            "--repo",
            slug,
            "--exclude-drafts",
            "--exclude-pre-releases",
            "--limit",
            "30",
        ],
        repo,
    );
    match host.run(&spec) {
        Err(err) => Err(Fail::new("GITHUB_RELEASE_UNKNOWN", err.detail)),
        Ok(out) if out.status != 0 => Err(Fail::new(
            "GITHUB_RELEASE_UNKNOWN",
            format!(
                "gh release list exited {}\n{}",
                out.status,
                exec::first_lines(&out.stderr, 20)
            ),
        )),
        Ok(out) => Ok(out.stdout),
    }
}

fn write_version_files(repo: &Path, manifest: &str, next: version::SemVer) -> Result<(), Fail> {
    let en = std::fs::read_to_string(repo.join("docs/RELEASE.md"))
        .map_err(|err| Fail::new("VERSION_SOURCE_MISSING", format!("docs/RELEASE.md: {err}")))?;
    let zh = std::fs::read_to_string(repo.join("docs/RELEASE.zh-CN.md")).map_err(|err| {
        Fail::new(
            "VERSION_SOURCE_MISSING",
            format!("docs/RELEASE.zh-CN.md: {err}"),
        )
    })?;
    let files = [
        (
            "Cargo.toml",
            version::render_bumped_manifest(manifest, next)?,
        ),
        (
            "docs/RELEASE.md",
            version::bump_heading(&en, next, "docs/RELEASE.md")?,
        ),
        (
            "docs/RELEASE.zh-CN.md",
            version::bump_heading(&zh, next, "docs/RELEASE.zh-CN.md")?,
        ),
    ];
    for (rel, contents) in files {
        std::fs::write(repo.join(rel), contents).map_err(|err| {
            Fail::new(
                "TOOL_FAILED",
                format!(
                    "could not write {rel}: {err}\nversion files may already be partially updated\n./dev did not reset them"
                ),
            )
        })?;
    }
    Ok(())
}

fn porcelain_paths(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            if line.len() < 4 {
                return None;
            }
            let path = line[3..].rsplit(" -> ").next().unwrap_or("").trim();
            if path.is_empty() {
                None
            } else {
                Some(path.to_string())
            }
        })
        .collect()
}

fn after_commit(err: Fail, sha: &str) -> Fail {
    Fail::new(
        err.code,
        format!(
            "{}\n\nqualification failed after the version commit\nlocal commit: {sha}\n./dev did not tag, push, or reset this commit",
            err.detail
        ),
    )
    .exit(err.exit)
}

fn dispatch_release(
    host: &dyn Host,
    repo: &Path,
    slug: &str,
    branch: &str,
    version: version::SemVer,
    commit: &str,
) -> Result<(), Fail> {
    let spec = CommandSpec::new(
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
            &format!("commit={commit}"),
        ],
        repo,
    );
    let out = host.run(&spec)?;
    if out.status == 0 {
        return Ok(());
    }
    Err(Fail::new(
        "RELEASE_DISPATCH_FAILED",
        format!(
            "gh workflow run release.yml exited {}\n{}\norigin has the release commit but the workflow was not dispatched; re-run it with:\n  gh workflow run release.yml --repo {slug} --ref {branch} -f version={version} -f commit={commit}",
            out.status,
            exec::first_lines(&out.stderr, 20)
        ),
    ))
}

fn push_ref(host: &dyn Host, repo: &Path, args: &[&str], recovery: &str) -> Result<(), Fail> {
    let out = host.run(&exec::git(repo, args))?;
    if out.status == 0 {
        return Ok(());
    }
    Err(Fail::new(
        "PUSH_FAILED",
        format!(
            "git {} failed\n{}\n{recovery}",
            args.join(" "),
            exec::first_lines(&out.stderr, 20)
        ),
    ))
}
