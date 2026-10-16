//! The four-stage release transaction.
//!
//! One command drives: build & static, automated tests, real dogfood, CI/CD and
//! publish. Each stage has an input fingerprint, an evidence hash, a status and
//! a log; a failure stops the run immediately, and `--resume` continues from the
//! first stage whose evidence no longer matches its inputs.
//!
//! Ownership: this owns *which* stages ran and on what. The dogfood lab owns
//! whether a stage's evidence is valid (it holds the layer reuse cache, the
//! model runs and the gate thresholds). GitHub owns whether the release exists.
//! Nothing here re-derives one of those facts from a printed line.

use std::path::{Path, PathBuf};

use crate::error::{Fail, Report};
use crate::exec::{self, CommandSpec, Host};
use crate::git;
use crate::github;
use crate::hash::sha256_file;
use crate::json::Json;
use crate::lock::Lock;
use crate::qualify;
use crate::report;
use crate::root::{self, Env, GateOpts, Lab};
use crate::state::{
    self, Cases, STAGE_BUILD, STAGE_DOGFOOD, STAGE_PUBLISH, STAGE_TESTS, Status, Transaction,
};
use crate::version::{self, Level};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Run,
    Resume,
    Status,
    Report,
    DryRun,
}

#[derive(Clone, Debug)]
pub struct PublishOpts {
    pub level: Level,
    pub mode: Mode,
    pub gate: GateOpts,
    /// The gate's own scope concept. `full` is what this pipeline used before;
    /// nothing is moved out of scope by this tool.
    pub release_scope: String,
    pub stop_after: u8,
    pub reuse_prior_rc: bool,
    pub retry_remote: bool,
    pub ci_timeout_seconds: u64,
    pub poll_seconds: u64,
    pub verify_artifacts: bool,
}

impl Default for PublishOpts {
    fn default() -> Self {
        PublishOpts {
            level: Level::Patch,
            mode: Mode::Run,
            gate: GateOpts::default(),
            release_scope: "full".to_string(),
            stop_after: STAGE_PUBLISH,
            reuse_prior_rc: true,
            retry_remote: false,
            ci_timeout_seconds: 3600,
            poll_seconds: 30,
            verify_artifacts: true,
        }
    }
}

const VERSION_FILES: [&str; 4] = [
    "Cargo.toml",
    "Cargo.lock",
    "docs/RELEASE.md",
    "docs/RELEASE.zh-CN.md",
];

pub fn parse_args(args: &[String]) -> Result<PublishOpts, Fail> {
    let (gate, rest) = root::extract_gate_opts(args)?;
    let mut opts = PublishOpts {
        gate,
        ..PublishOpts::default()
    };
    let mut level: Option<Level> = None;
    let mut index = 0;
    while index < rest.len() {
        match rest[index].as_str() {
            "--patch" => set_level(&mut level, Level::Patch)?,
            "--minor" => set_level(&mut level, Level::Minor)?,
            "--major" => set_level(&mut level, Level::Major)?,
            "--dry-run" => opts.mode = Mode::DryRun,
            "--resume" => opts.mode = Mode::Resume,
            "--status" => opts.mode = Mode::Status,
            "--report" => opts.mode = Mode::Report,
            "--no-rc-reuse" => opts.reuse_prior_rc = false,
            "--retry-remote" => opts.retry_remote = true,
            "--no-artifact-verify" => opts.verify_artifacts = false,
            "--release-scope" => {
                let value = value_of(&rest, &mut index, "--release-scope")?;
                if value != "full" && value != "cli-tui" {
                    return Err(Fail::new(
                        "PUBLISH_SCOPE_INVALID",
                        format!("--release-scope takes full or cli-tui, got {value}"),
                    )
                    .exit(2));
                }
                opts.release_scope = value;
            }
            "--stop-after" => {
                let value = value_of(&rest, &mut index, "--stop-after")?;
                let stage: u8 = value.parse().map_err(|_| stage_error(&value))?;
                if !(1..=4).contains(&stage) {
                    return Err(stage_error(&value));
                }
                opts.stop_after = stage;
            }
            "--ci-timeout" => {
                let value = value_of(&rest, &mut index, "--ci-timeout")?;
                opts.ci_timeout_seconds = value
                    .parse()
                    .map_err(|_| seconds_error("--ci-timeout", &value))?;
            }
            "--poll-seconds" => {
                let value = value_of(&rest, &mut index, "--poll-seconds")?;
                opts.poll_seconds = value
                    .parse()
                    .map_err(|_| seconds_error("--poll-seconds", &value))?;
            }
            other => {
                return Err(
                    Fail::new("UNKNOWN_COMMAND", format!("unknown argument: {other}")).exit(2),
                );
            }
        }
        index += 1;
    }
    opts.level = level.unwrap_or(Level::Patch);
    if matches!(opts.mode, Mode::Resume | Mode::Status | Mode::Report) && level.is_some() {
        return Err(Fail::new(
            "PUBLISH_MODE_CONFLICT",
            "--patch/--minor/--major describe a new release; --resume/--status/--report keep the version already in progress",
        ));
    }
    Ok(opts)
}

fn stage_error(value: &str) -> Fail {
    Fail::new(
        "PUBLISH_STAGE_INVALID",
        format!("--stop-after takes 1..4, got {value}"),
    )
    .exit(2)
}

fn seconds_error(flag: &str, value: &str) -> Fail {
    Fail::new(
        "PUBLISH_TIMEOUT_INVALID",
        format!("{flag} takes a whole number of seconds, got {value}"),
    )
    .exit(2)
}

fn value_of(args: &[String], index: &mut usize, flag: &str) -> Result<String, Fail> {
    *index += 1;
    args.get(*index)
        .cloned()
        .ok_or_else(|| Fail::new("UNKNOWN_COMMAND", format!("{flag} needs a value")).exit(2))
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

/// Everything a stage needs besides the transaction itself.
pub struct Context<'a> {
    pub host: &'a dyn Host,
    pub repo: PathBuf,
    pub lab: Lab,
    pub branch: String,
    pub slug: String,
    pub rustc: String,
    pub definition_hash: String,
    pub opts: PublishOpts,
    pub baseline_sha: String,
    /// `./dev publish` holds the publish lock; `./dev release` cannot publish and
    /// therefore does not take it.
    pub holds_lock: bool,
}

pub fn run(host: &dyn Host, repo: &Path, env: &Env, args: &[String]) -> Result<Report, Fail> {
    let opts = parse_args(args)?;
    match opts.mode {
        Mode::Status => status(host, repo),
        Mode::Report => {
            let (transaction, context) = load_in_progress(host, repo, env, &opts)?;
            let paths = report::write(&context, &transaction)?;
            Ok(Report::ok(format!(
                "release: {}\nstatus:  {}\nreport:  {}\ndata:    {}{}",
                transaction.release_id,
                transaction.status.label(),
                paths.markdown.display(),
                paths.json.display(),
                report_tail(&paths)
            )))
        }
        Mode::DryRun => dry_run(host, repo, env, &opts),
        Mode::Run | Mode::Resume => {
            let _lock = Lock::acquire(host, repo, "publish")?;
            match opts.mode {
                Mode::Run => start(host, repo, env, &opts),
                _ => resume(host, repo, env, &opts),
            }
        }
    }
}

/// `./dev release`: stages 1-3 on the commit that is checked out, with no
/// version bump, no push and no tag. It is the same stage machinery as publish,
/// stopped before stage 4, so a local qualification cannot disagree with a
/// release about what a stage means.
pub fn qualify_only(
    host: &dyn Host,
    repo: &Path,
    env: &Env,
    args: &[String],
) -> Result<Report, Fail> {
    let mut parsed = vec!["--stop-after".to_string(), "3".to_string()];
    parsed.extend(args.iter().cloned());
    let opts = parse_args(&parsed)?;
    if opts.mode != Mode::Run {
        return Err(Fail::new(
            "PUBLISH_MODE_CONFLICT",
            "--resume/--status/--report/--dry-run belong to ./dev publish; ./dev release always qualifies the current commit",
        ));
    }
    let snap = git::snapshot(host, repo)?;
    git::require_clean(&snap)?;
    let local = version::read_local(repo)?;
    let current = version::require_local(&local)?;
    let lab = root::resolve_lab(repo, &opts.gate, env)?;
    let baseline_sha = qualify::read_baseline(&lab.root)?;
    let definition_hash = definition_hash(&lab.root)?;
    let manifest = std::fs::read_to_string(repo.join("Cargo.toml")).unwrap_or_default();
    let slug = version::github_slug(&manifest)?;
    let branch = exec::ok_stdout(
        host,
        exec::git(repo, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "GIT_FAILED",
    )?;
    let context = Context {
        host,
        repo: repo.to_path_buf(),
        lab,
        branch,
        slug,
        rustc: rustc_version(host, repo)?,
        definition_hash,
        opts: opts.clone(),
        baseline_sha,
        holds_lock: false,
    };
    // The transaction is named for what it is: a qualification of one commit,
    // not a version. It deliberately does not take the publish lock or the
    // CURRENT pointer, because it cannot publish anything.
    let mut transaction = Transaction::new(
        format!("qualify-v{current}-{}", state::short_sha(&snap.head)),
        current.to_string(),
        snap.head.clone(),
        crate::clock::now(),
    );
    transaction.model = Some(context.lab.model.clone());
    transaction.baseline_sha = Some(context.baseline_sha.clone());
    let release_id = transaction.release_id.clone();
    state::save_state(repo, &transaction)?;
    let outcome = drive(repo, &context, transaction, Vec::new());
    let report_path = state::state_dir(repo, &release_id).join("RELEASE_REPORT.md");
    outcome.map(|report| {
        Report::with_code(
            report.code,
            format!(
                "{}\nreport: {}",
                report.text.trim_end(),
                report_path.display()
            ),
        )
    })
}

// ── read-only surfaces ──────────────────────────────────────────────────────

fn status(host: &dyn Host, repo: &Path) -> Result<Report, Fail> {
    let Some(release_id) = state::read_current(repo) else {
        return Ok(Report::ok(
            "no release in progress\nstart one with ./dev publish --patch",
        ));
    };
    let Some(transaction) = state::read_state(repo, &release_id)? else {
        return Err(Fail::new(
            "RELEASE_STATE_MISSING",
            format!(
                "{} points at {release_id}, but its state.json is gone",
                state::current_path(repo).display()
            ),
        ));
    };
    let mut lines = vec![
        format!("release:       {}", transaction.release_id),
        format!("version:       {}", transaction.version),
        format!("candidate:     {}", transaction.candidate_sha),
        format!("status:        {}", transaction.status.label()),
        format!(
            "current stage: {} {}",
            transaction.stage,
            state::stage_name(transaction.stage)
        ),
        format!(
            "started:       {}",
            crate::clock::iso8601(transaction.started_at)
        ),
        format!(
            "elapsed:       {}",
            crate::clock::human_duration(transaction.duration())
        ),
        format!("resumes:       {}", transaction.resume_count),
        format!(
            "publish lock:  {}",
            if lock_is_held(host, repo)? {
                "held"
            } else {
                "free"
            }
        ),
    ];
    if let Some(reason) = &transaction.failure_reason {
        lines.push(format!("failure:       {reason}"));
    }
    if transaction.candidate_history.len() > 1 {
        lines.push(format!(
            "candidates:    {} (oldest first)",
            transaction.candidate_history.join(" -> ")
        ));
    }
    lines.push(String::new());
    lines.push("stages:".to_string());
    for record in &transaction.stages {
        lines.push(format!(
            "  {} {:<16} {:<11} {}",
            record.stage,
            state::stage_name(record.stage),
            record.status.label(),
            if record.seconds > 0.0 {
                crate::clock::human_duration(record.seconds)
            } else {
                "-".to_string()
            }
        ));
        if let Some(reason) = &record.failure_reason {
            lines.push(format!("      reason: {reason}"));
        }
        if let Some(cases) = &record.cases {
            lines.push(format!(
                "      cases: {} total, {} pass, {} fail, {} warn, {} reused, {} unproven, {} unstable",
                cases.total, cases.passed, cases.failed, cases.warn, cases.reused, cases.unproven, cases.unstable
            ));
        }
        if let Some(run_id) = &record.run_id {
            lines.push(format!("      run: {run_id}"));
        }
    }
    if let Some(run_id) = &transaction.remote.workflow_run_id {
        lines.push(format!(
            "workflow run:  {run_id} ({}/{:?})",
            transaction
                .remote
                .workflow_run_status
                .clone()
                .unwrap_or_default(),
            transaction.remote.workflow_run_conclusion
        ));
    }
    lines.push(String::new());
    lines.push(next_step(&transaction));
    Ok(Report::with_code(0, lines.join("\n")))
}

fn next_step(transaction: &Transaction) -> String {
    match transaction.status {
        Status::Pass => "next: nothing — this release is published".to_string(),
        Status::Fail | Status::Blocked => format!(
            "next: fix the cause, then ./dev publish --resume  (keeps {})",
            transaction.release_id
        ),
        _ => format!(
            "next: ./dev publish --resume  (keeps {})",
            transaction.release_id
        ),
    }
}

fn lock_is_held(host: &dyn Host, repo: &Path) -> Result<bool, Fail> {
    let Ok(text) = std::fs::read_to_string(state::lock_path(repo)) else {
        return Ok(false);
    };
    let Some(pid) = Json::parse(&text)
        .ok()
        .and_then(|doc| doc.path_f64("pid"))
        .map(|value| value as u32)
    else {
        return Ok(true);
    };
    let out = host.run(&CommandSpec::new(
        "ps",
        ["-p", &pid.to_string(), "-o", "pid="],
        repo,
    ))?;
    Ok(out.status == 0 && !out.stdout.trim().is_empty())
}

fn dry_run(host: &dyn Host, repo: &Path, env: &Env, opts: &PublishOpts) -> Result<Report, Fail> {
    let snap = git::snapshot(host, repo)?;
    git::require_clean(&snap)?;
    let plan = plan(host, repo, env, opts)?;
    let mut lines = vec![
        format!("current:    {}", plan.current),
        format!("next:       {}", plan.next),
        format!("candidate:  {}", snap.head),
        format!(
            "tag:        {} (created by the release workflow after every artifact verifies)",
            plan.tag
        ),
        format!("scope:      {}", opts.release_scope),
        format!("model:      {}", plan.model),
        format!("baseline:   {} (dogfood release baseline)", plan.baseline),
        String::new(),
        "stages (nothing is executed, nothing is mutated):".to_string(),
    ];
    for stage in state::STAGES {
        lines.push(format!(
            "  {} — {}",
            state::stage_title(stage),
            stage_plan(stage)
        ));
    }
    lines.push(String::new());
    lines.push(format!(
        "would-commit:   release: v{} at a new commit",
        plan.next
    ));
    lines.push("would-push:     git push origin HEAD  (stage 4)".to_string());
    lines.push(format!(
        "would-dispatch: gh workflow run release.yml --repo {} --ref {} -f version={} -f commit=<the version commit>",
        plan.slug, plan.branch, plan.next
    ));
    lines.push(format!(
        "would-wait:     CI bound to that commit, then the release workflow to a terminal state (timeout {}s)",
        opts.ci_timeout_seconds
    ));
    lines.push(format!(
        "would-archive:  {}",
        report::report_dir(
            repo,
            &Transaction::new(
                format!("v{}", plan.next),
                plan.next.to_string(),
                snap.head.clone(),
                crate::clock::now(),
            )
        )
        .display()
    ));
    lines.push("READY_TO_PUBLISH: NO".to_string());
    lines.push(
        "note: the qualified commit does not exist until publish bumps the version; the tag and the\n\
         GitHub release are created by the release workflow, after every artifact verifies"
            .to_string(),
    );
    Ok(Report::ok(lines.join("\n")))
}

fn stage_plan(stage: u8) -> &'static str {
    match stage {
        STAGE_BUILD => {
            "cargo fmt/clippy/release build + supply chain + binary identity (dogfood L0)"
        }
        STAGE_TESTS => {
            "full workspace tests, contracts, permission gates, TUI + PTY (dogfood L1,L2)"
        }
        STAGE_DOGFOOD => {
            "real model dogfood + performance evidence, then RC qualification (dogfood L3,L4 + rc_check)"
        }
        STAGE_PUBLISH => {
            "freeze SHA, push, wait for required CI, dispatch release workflow, verify tag/artifacts"
        }
        _ => "unknown",
    }
}

struct Plan {
    current: version::SemVer,
    next: version::SemVer,
    tag: String,
    slug: String,
    branch: String,
    model: String,
    baseline: String,
}

fn plan(host: &dyn Host, repo: &Path, env: &Env, opts: &PublishOpts) -> Result<Plan, Fail> {
    let manifest = std::fs::read_to_string(repo.join("Cargo.toml")).map_err(|err| {
        Fail::new(
            "VERSION_SOURCE_MISSING",
            format!("could not read Cargo.toml: {err}"),
        )
    })?;
    let local = version::read_local(repo)?;
    let current = version::require_local(&local)?;
    let next = version::bump(current, opts.level)?;
    let tag = format!("v{next}");
    // Local tags plus the ones origin has: the release workflow creates the tag
    // remotely, so a clone that has not fetched it would otherwise read the
    // version it just released as "not yet released".
    let local_tags = exec::ok_stdout(host, exec::git(repo, &["tag", "--list"]), "GIT_FAILED")?;
    let remote_tags = exec::ok_stdout(
        host,
        exec::git(repo, &["ls-remote", "--tags", "origin"]),
        "GIT_FAILED",
    )?;
    let remote = version::remote_tag_names(&remote_tags);
    let tags = format!("{local_tags}\n{}", remote.join("\n"));
    if version::tag_listed(&tags, &tag) {
        return Err(Fail::new(
            "TAG_EXISTS",
            format!("{tag} already exists locally"),
        ));
    }
    let slug = version::github_slug(&manifest)?;
    let releases = github_release_list(host, repo, &slug)?;
    version::require_publish(&local, &tags, &releases)?;
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
    let lab = root::resolve_lab(repo, &opts.gate, env)?;
    Ok(Plan {
        current,
        next,
        tag,
        slug,
        branch,
        model: lab.model.clone(),
        baseline: qualify::read_baseline(&lab.root)?,
    })
}

fn github_release_list(host: &dyn Host, repo: &Path, slug: &str) -> Result<String, Fail> {
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

// ── starting and resuming ───────────────────────────────────────────────────

fn start(host: &dyn Host, repo: &Path, env: &Env, opts: &PublishOpts) -> Result<Report, Fail> {
    let snap = git::snapshot(host, repo)?;
    git::require_clean(&snap)?;
    if let Some(existing) = state::read_current(repo) {
        return Err(Fail::new(
            "RELEASE_IN_PROGRESS",
            format!(
                "{existing} is still in progress\n\
                 finish it with ./dev publish --resume, or inspect it with ./dev publish --status\n\
                 a second release would share this lab, this lock and this version number"
            ),
        ));
    }
    let plan = plan(host, repo, env, opts)?;
    let remote_tags = exec::ok_stdout(
        host,
        exec::git(
            repo,
            &[
                "ls-remote",
                "--tags",
                "origin",
                &format!("refs/tags/{}", plan.tag),
            ],
        ),
        "GIT_FAILED",
    )?;
    if version::tag_listed(&remote_tags, &plan.tag) {
        return Err(Fail::new(
            "TAG_EXISTS",
            format!(
                "{} already exists on origin; a released version is never reused",
                plan.tag
            ),
        ));
    }
    if let Some(release) = github::release_view(host, repo, &plan.slug, &plan.tag)?
        && !release.is_draft
    {
        return Err(Fail::new(
            "RELEASE_EXISTS",
            format!(
                "{} is already published at {}\na published version is immutable; release a new patch version instead",
                plan.tag,
                release.url.unwrap_or_default()
            ),
        ));
    }

    let lab = root::resolve_lab(repo, &opts.gate, env)?;
    let baseline = qualify::read_baseline(&lab.root)?;
    bump_version_files(repo, &plan)?;
    commit_version_files(host, repo, &plan)?;
    let published = git::snapshot(host, repo)?;
    let rustc = rustc_version(host, repo)?;
    let definition_hash = definition_hash(&lab.root)?;
    let mut transaction = Transaction::new(
        format!("v{}", plan.next),
        plan.next.to_string(),
        published.head.clone(),
        crate::clock::now(),
    );
    transaction.previous_release = Some(format!("v{}", plan.current));
    transaction.version_commit = Some(published.head.clone());
    transaction.model = Some(plan.model.clone());
    transaction.baseline_sha = Some(baseline.clone());
    let context = Context {
        host,
        repo: repo.to_path_buf(),
        lab,
        branch: plan.branch.clone(),
        slug: plan.slug.clone(),
        rustc,
        definition_hash,
        opts: opts.clone(),
        baseline_sha: baseline,
        holds_lock: true,
    };
    state::write_current(repo, &transaction.release_id)?;
    state::save_state(repo, &transaction)?;
    let note = format!(
        "release {} started: candidate {} (was {})",
        transaction.release_id,
        transaction.candidate_short,
        transaction.previous_release.clone().unwrap_or_default()
    );
    println!("{note}");
    drive(repo, &context, transaction, vec![note])
}

fn bump_version_files(repo: &Path, plan: &Plan) -> Result<(), Fail> {
    let manifest = std::fs::read_to_string(repo.join("Cargo.toml")).map_err(|err| {
        Fail::new(
            "VERSION_SOURCE_MISSING",
            format!("could not read Cargo.toml: {err}"),
        )
    })?;
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
            version::render_bumped_manifest(&manifest, plan.next)?,
        ),
        (
            "docs/RELEASE.md",
            version::bump_heading(&en, plan.next, "docs/RELEASE.md")?,
        ),
        (
            "docs/RELEASE.zh-CN.md",
            version::bump_heading(&zh, plan.next, "docs/RELEASE.zh-CN.md")?,
        ),
    ];
    for (relative, contents) in files {
        std::fs::write(repo.join(relative), contents).map_err(|err| {
            Fail::new(
                "TOOL_FAILED",
                format!(
                    "could not write {relative}: {err}\nversion files may already be partially updated\n./dev did not reset them"
                ),
            )
        })?;
    }
    Ok(())
}

/// Commit the version files. From here on the release candidate is a frozen
/// commit; the transaction records it before anything is qualified.
fn commit_version_files(host: &dyn Host, repo: &Path, plan: &Plan) -> Result<(), Fail> {
    // `cargo metadata` proves the manifest is still parseable before it is
    // committed: a half-written version file must not become a candidate.
    let metadata = host.run(&CommandSpec::new(
        "cargo",
        ["metadata", "--format-version", "1", "--no-deps"],
        repo,
    ))?;
    if metadata.status != 0 {
        return Err(Fail::new(
            "TOOL_FAILED",
            format!(
                "cargo metadata exited {}\n{}\nthe version files are modified; ./dev did not reset them",
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
    let unexpected: Vec<String> = porcelain_paths(&dirty.stdout)
        .into_iter()
        .filter(|path| !VERSION_FILES.contains(&path.as_str()))
        .collect();
    if !unexpected.is_empty() {
        return Err(Fail::new(
            "PUBLISH_UNEXPECTED_MUTATION",
            format!(
                "the version bump changed files outside the version set: {}\n./dev did not commit or reset",
                unexpected.join(", ")
            ),
        ));
    }
    let mut add = vec!["add".to_string(), "--".to_string()];
    add.extend(VERSION_FILES.iter().map(|path| (*path).to_string()));
    let refs: Vec<&str> = add.iter().map(String::as_str).collect();
    let added = host.run(&exec::git(repo, &refs))?;
    if added.status != 0 {
        return Err(Fail::new(
            "COMMIT_FAILED",
            format!(
                "git add failed\n{}\nversion files may already be modified\n./dev did not reset them",
                exec::first_lines(&added.stderr, 20)
            ),
        ));
    }
    let message = format!("release: v{}", plan.next);
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

fn rustc_version(host: &dyn Host, repo: &Path) -> Result<String, Fail> {
    let out = host.run(&CommandSpec::new("rustc", ["--version"], repo))?;
    if out.status != 0 {
        return Err(Fail::new(
            "TOOL_MISSING",
            format!(
                "rustc --version exited {}\n{}",
                out.status,
                exec::first_lines(&out.stderr, 10)
            ),
        ));
    }
    Ok(out.stdout.trim().to_string())
}

/// Hash of the lab's own gate harness. A changed gate definition is a changed
/// test, so it invalidates every stage fingerprint, exactly as it invalidates
/// the lab's own layer reuse keys.
fn definition_hash(lab: &Path) -> Result<String, Fail> {
    let files = [
        "eval/scripts/release_check.py",
        "eval/release_gate.py",
        "eval/release_gate.yaml",
        "eval/scripts/release_contracts.py",
        "eval/scripts/rc_check.py",
        "eval/rc_gate.py",
        "eval/rc_gate.yaml",
        "eval/scripts/feature_coverage.py",
        "eval/feature_inventory.yaml",
    ];
    let mut digest = crate::hash::Sha256::new();
    for relative in files {
        digest.update(relative.as_bytes());
        digest.update(b"=");
        match sha256_file(&lab.join(relative)) {
            Some(hash) => digest.update(hash.as_bytes()),
            None => digest.update(b"MISSING"),
        }
        digest.update(b"\n");
    }
    Ok(digest.finish())
}

fn load_in_progress<'a>(
    host: &'a dyn Host,
    repo: &Path,
    env: &Env,
    opts: &PublishOpts,
) -> Result<(Transaction, Context<'a>), Fail> {
    let Some(release_id) = state::read_current(repo) else {
        return Err(Fail::new(
            "RELEASE_NOT_IN_PROGRESS",
            "there is no release in progress\nstart one with ./dev publish --patch",
        ));
    };
    let Some(transaction) = state::read_state(repo, &release_id)? else {
        return Err(Fail::new(
            "RELEASE_STATE_MISSING",
            format!(
                "{} points at {release_id}, but its state.json is gone",
                state::current_path(repo).display()
            ),
        ));
    };
    let lab = root::resolve_lab(repo, &opts.gate, env)?;
    let baseline_sha = qualify::read_baseline(&lab.root)?;
    let rustc = rustc_version(host, repo)?;
    let definition_hash = definition_hash(&lab.root)?;
    let branch = exec::ok_stdout(
        host,
        exec::git(repo, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "GIT_FAILED",
    )?;
    let manifest = std::fs::read_to_string(repo.join("Cargo.toml")).unwrap_or_default();
    let slug = version::github_slug(&manifest)?;
    let context = Context {
        host,
        repo: repo.to_path_buf(),
        lab,
        branch,
        slug,
        rustc,
        definition_hash,
        opts: opts.clone(),
        baseline_sha,
        holds_lock: true,
    };
    Ok((transaction, context))
}

fn resume(host: &dyn Host, repo: &Path, env: &Env, opts: &PublishOpts) -> Result<Report, Fail> {
    let (mut transaction, context) = load_in_progress(host, repo, env, opts)?;
    if transaction.status == Status::Pass {
        let paths = report::write(&context, &transaction)?;
        return Ok(Report::ok(format!(
            "release {} is already published; nothing to resume\nreport: {}",
            transaction.release_id,
            paths.markdown.display()
        )));
    }
    // The version files are the release identity. A tree that no longer says
    // this version is a different release, not a resume of this one.
    let local = version::read_local(repo)?;
    let declared = version::require_local(&local)?;
    if declared.to_string() != transaction.version {
        return Err(Fail::new(
            "RELEASE_VERSION_CHANGED",
            format!(
                "release {} is for version {}, but the tree now says {declared}\n\
                 a resumed release keeps its version; a new version is a new release",
                transaction.release_id, transaction.version
            ),
        ));
    }
    let head = exec::ok_stdout(host, exec::git(repo, &["rev-parse", "HEAD"]), "GIT_FAILED")?;
    if head != transaction.candidate_sha {
        note_candidate_change(host, repo, &mut transaction, &head)?;
    }
    transaction.resume_count += 1;
    if !transaction.candidate_history.contains(&head) {
        transaction.candidate_history.push(head.clone());
    }
    state::save_state(repo, &transaction)?;
    let mut leading = Vec::new();
    if let Some(note) = transaction.changed_paths_note.clone() {
        leading.push(note);
        transaction.changed_paths_note = None;
    }
    let note = format!(
        "resuming {} at {} (attempt {})",
        transaction.release_id,
        state::short_sha(&head),
        transaction.resume_count
    );
    println!("{note}");
    leading.push(note);
    drive(repo, &context, transaction, leading)
}

/// A resumed release may point at a new commit. Record exactly what changed;
/// the stage fingerprints decide what has to be re-earned.
fn note_candidate_change(
    host: &dyn Host,
    repo: &Path,
    transaction: &mut Transaction,
    new_sha: &str,
) -> Result<(), Fail> {
    let old = transaction.candidate_sha.clone();
    let diff = host.run(&exec::git(repo, &["diff", "--name-only", &old, new_sha]))?;
    if diff.status != 0 {
        return Err(Fail::new(
            "GIT_FAILED",
            format!(
                "could not diff {old}..{new_sha}\n{}\n\
                 a resumed release must be built on the commit it qualifies",
                exec::first_lines(&diff.stderr, 20)
            ),
        ));
    }
    let changed: Vec<String> = diff
        .stdout
        .lines()
        .map(str::to_string)
        .filter(|line| !line.is_empty())
        .collect();
    transaction.retry_count += 1;
    transaction.candidate_sha = new_sha.to_string();
    transaction.candidate_short = state::short_sha(new_sha);
    transaction.changed_paths = changed.clone();
    let note = format!(
        "candidate moved {} -> {} ({} changed path(s)); stages whose inputs changed are re-earned",
        state::short_sha(&old),
        state::short_sha(new_sha),
        changed.len()
    );
    println!("{note}");
    transaction.changed_paths_note = Some(note);
    Ok(())
}

// ── the stage loop ──────────────────────────────────────────────────────────

fn drive(
    repo: &Path,
    context: &Context,
    mut transaction: Transaction,
    leading: Vec<String>,
) -> Result<Report, Fail> {
    let mut summary: Vec<String> = leading;
    let mut stop_reason: Option<String> = None;
    for stage in state::STAGES {
        if stage > context.opts.stop_after {
            stop_reason = Some(format!(
                "stopped after stage {} as requested",
                context.opts.stop_after
            ));
            break;
        }
        if context.holds_lock {
            crate::lock::verify_held(repo)?;
        }
        let fingerprint = stage_fingerprint(context, &transaction, stage);
        let previous = transaction.stage_record(stage).clone();
        if previous.status.is_decided_pass()
            && previous.input_fingerprint.as_deref() == Some(fingerprint.as_str())
        {
            match evidence_still_matches(context, &transaction, &previous) {
                Ok(()) => {
                    let note = format!(
                        "stage {stage} {}: REUSED ({})",
                        state::stage_name(stage),
                        if previous.reused {
                            "dogfood evidence cache"
                        } else {
                            "same candidate, same inputs, same evidence"
                        }
                    );
                    println!("{note}");
                    summary.push(note);
                    continue;
                }
                Err(reason) => {
                    {
                        let record = transaction.stage_record_mut(stage);
                        record.status = Status::Invalidated;
                        record.reused = false;
                        record.failure_reason = Some(reason.clone());
                    }
                    let note = format!(
                        "stage {stage} {}: INVALIDATED ({reason})",
                        state::stage_name(stage)
                    );
                    println!("{note}");
                    summary.push(note);
                }
            }
        } else if previous.status.is_decided_pass() {
            {
                let record = transaction.stage_record_mut(stage);
                record.status = Status::Invalidated;
                record.reused = false;
                record.failure_reason = Some("its inputs changed".to_string());
            }
            let note = format!(
                "stage {stage} {}: INVALIDATED (its inputs changed)",
                state::stage_name(stage)
            );
            println!("{note}");
            summary.push(note);
        }
        {
            let record = transaction.stage_record_mut(stage);
            record.status = Status::Running;
            record.started_at = Some(crate::clock::now());
            record.finished_at = None;
            record.seconds = 0.0;
            record.failure_reason = None;
        }
        transaction.stage = stage;
        state::save_state(repo, &transaction)?;

        let started = crate::clock::now();
        let outcome = match stage {
            STAGE_BUILD => stage_build(context, &mut transaction),
            STAGE_TESTS => stage_tests(context, &mut transaction),
            STAGE_DOGFOOD => stage_dogfood(context, &mut transaction),
            STAGE_PUBLISH => stage_publish(context, &mut transaction),
            _ => Err(Fail::new("PUBLISH_STAGE_INVALID", "unknown stage")),
        };
        let finished = crate::clock::now();
        let (status, detail, reused) = match outcome {
            Ok(detail) => {
                let reused = transaction.stage_record(stage).reused;
                let status = if reused { Status::Reused } else { Status::Pass };
                (status, detail, reused)
            }
            Err(err) => {
                let reason = format!("{}: {}", err.code, first_line(&err.detail));
                {
                    let record = transaction.stage_record_mut(stage);
                    record.status = Status::Fail;
                    record.finished_at = Some(finished);
                    record.seconds = finished - started;
                    record.input_fingerprint = Some(fingerprint);
                    record.failure_reason = Some(reason.clone());
                }
                transaction.status = Status::Fail;
                transaction.failure_reason = Some(format!(
                    "stage {stage} ({}): {}",
                    state::stage_name(stage),
                    first_line(&err.detail)
                ));
                transaction.finished_at = Some(finished);
                transaction.note_attempt(stage, Status::Fail, Some(reason), finished);
                state::save_state(repo, &transaction)?;
                let detail = transaction.stage_record(stage).detail.clone();
                write_stage_log(
                    context,
                    &transaction,
                    stage,
                    &format!("status: FAIL\nreason: {}\n", err.code),
                    &format!("{}\n{}", err.detail.trim_end(), detail.join("\n")),
                );
                let paths = report::write(context, &transaction).unwrap_or_else(|report_err| {
                    eprintln!(
                        "WARNING: the release report could not be written: {}",
                        report_err.detail
                    );
                    report::Paths::default()
                });
                return Ok(Report::with_code(
                    err.exit,
                    failure_text(repo, &transaction, stage, &err, &paths),
                ));
            }
        };
        {
            let record = transaction.stage_record_mut(stage);
            record.status = status;
            record.finished_at = Some(finished);
            record.seconds = finished - started;
            record.input_fingerprint = Some(fingerprint);
            record.detail = detail;
        }
        transaction.note_attempt(stage, status, None, finished);
        state::save_state(repo, &transaction)?;
        let record = transaction.stage_record(stage);
        write_stage_log(
            context,
            &transaction,
            stage,
            &format!(
                "status: {}\nseconds: {:.1}\nreused: {}\n",
                status.label(),
                record.seconds,
                record.reused
            ),
            &record.detail.join("\n"),
        );
        let note = format!(
            "stage {stage} {}: {}{} in {}",
            state::stage_name(stage),
            status.label(),
            if reused { " (dogfood cache)" } else { "" },
            crate::clock::human_duration(finished - started)
        );
        println!("{note}");
        summary.push(note);
        // The stage's own findings belong in what the command returns, not only
        // in the report: the operator reads the command, not the file.
        for line in &transaction.stage_record(stage).detail {
            summary.push(format!("  {line}"));
        }
    }

    if let Some(reason) = stop_reason {
        transaction.status = Status::Pending;
        state::save_state(repo, &transaction)?;
        let paths = report::write(context, &transaction)?;
        let mut lines = vec![
            format!("release: {}", transaction.release_id),
            format!("candidate: {}", transaction.candidate_sha),
            reason,
        ];
        lines.extend(summary);
        lines.push(format!("report: {}", paths.markdown.display()));
        lines.push(format!(
            "next:   ./dev publish --resume  (continues at stage {})",
            first_undecided(&transaction)
        ));
        return Ok(Report::ok(lines.join("\n")));
    }

    transaction.status = Status::Pass;
    transaction.finished_at = Some(crate::clock::now());
    transaction.stage = STAGE_PUBLISH;
    state::save_state(repo, &transaction)?;
    let paths = report::write(context, &transaction)?;
    let mut lines = vec![
        format!("release:      {}", transaction.release_id),
        format!("version:      {}", transaction.version),
        format!("candidate:    {}", transaction.candidate_sha),
        format!("status:       {}", transaction.status.label()),
        format!(
            "elapsed:      {}",
            crate::clock::human_duration(transaction.duration())
        ),
        format!(
            "tag:          {}",
            transaction.remote.tag.clone().unwrap_or_default()
        ),
        format!(
            "release:      {}",
            transaction.remote.release_url.clone().unwrap_or_default()
        ),
        String::new(),
    ];
    lines.extend(summary);
    lines.push(String::new());
    lines.push(format!("report: {}", paths.markdown.display()));
    lines.push(format!("data:   {}", paths.json.display()));
    if let Some(archived) = &paths.archived {
        lines.push(format!("archived: {}", archived.display()));
    }
    for warning in &paths.warnings {
        lines.push(format!("WARNING: {warning}"));
    }
    lines.push("READY_TO_PUBLISH: PASS".to_string());
    Ok(Report::ok(lines.join("\n")))
}

fn failure_text(
    repo: &Path,
    transaction: &Transaction,
    stage: u8,
    err: &Fail,
    paths: &report::Paths,
) -> String {
    let mut lines = vec![
        format!("release:      {}", transaction.release_id),
        format!("candidate:    {}", transaction.candidate_sha),
        format!("FAILED at stage {stage}: {}", state::stage_name(stage)),
        format!("reason:       {}", err.code),
        err.detail.trim_end().to_string(),
        String::new(),
        "stages:".to_string(),
    ];
    for record in &transaction.stages {
        lines.push(format!(
            "  {} {:<16} {}",
            record.stage,
            state::stage_name(record.stage),
            record.status.label()
        ));
    }
    lines.push(String::new());
    lines.push(format!(
        "the version commit {} exists locally and was NOT tagged, pushed, or reset",
        transaction.candidate_sha
    ));
    lines.push(format!(
        "report: {}\nstate:  {}",
        paths.markdown.display(),
        state::state_path(repo, &transaction.release_id).display()
    ));
    lines.push(format!(
        "next:   fix the cause, then ./dev publish --resume  (keeps {})",
        transaction.release_id
    ));
    lines.join("\n")
}

fn first_undecided(transaction: &Transaction) -> u8 {
    for stage in state::STAGES {
        if !transaction.stage_record(stage).status.is_decided_pass() {
            return stage;
        }
    }
    STAGE_PUBLISH
}

/// A recorded pass is only reusable while the evidence it was computed from is
/// still the evidence on disk. The digest recorded with the verdict is what
/// makes that checkable instead of assumed.
fn evidence_still_matches(
    context: &Context,
    transaction: &Transaction,
    record: &state::StageRecord,
) -> Result<(), String> {
    let Some(recorded) = record.evidence_hash.as_deref() else {
        return Err("the recorded verdict carries no evidence digest".to_string());
    };
    let layers = match record.stage {
        STAGE_BUILD => "L0",
        STAGE_TESTS => "L1,L2",
        STAGE_DOGFOOD => "L3,L4",
        _ => {
            // Stage 4's evidence is the remote facts; a completed release is the
            // proof that they are still true.
            return match transaction.remote.release_url.as_deref() {
                Some(_) => Ok(()),
                None => Err("no release URL was recorded for the published release".to_string()),
            };
        }
    };
    let layout = evidence_layout(context, transaction, layers, record.stage == STAGE_DOGFOOD)
        .map_err(|err| err.detail)?;
    if layout.hash == recorded {
        Ok(())
    } else {
        Err(format!(
            "the {layers} evidence on disk no longer hashes to the digest recorded with the verdict"
        ))
    }
}

fn report_tail(paths: &report::Paths) -> String {
    let mut tail = String::new();
    if let Some(archived) = &paths.archived {
        tail.push_str(&format!("\narchived: {}", archived.display()));
    }
    for warning in &paths.warnings {
        tail.push_str(&format!("\nWARNING: {warning}"));
    }
    tail
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or("").trim().to_string()
}

/// Append a stage's header and raw output to its own log, so a failure leaves
/// both the error location and everything the command said behind it.
fn write_stage_log(
    context: &Context,
    transaction: &Transaction,
    stage: u8,
    header: &str,
    body: &str,
) {
    let dir = state::log_dir(&context.repo, &transaction.release_id);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join(format!("stage-{stage}.log"));
    let mut text = String::new();
    text.push_str(&format!(
        "=== stage {stage} {} at {} ===\n",
        state::stage_name(stage),
        crate::clock::iso8601(crate::clock::now())
    ));
    text.push_str(header);
    text.push_str(body);
    if !text.ends_with('\n') {
        text.push('\n');
    }
    use std::io::Write as _;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = file.write_all(text.as_bytes());
    }
}

// ── fingerprints ────────────────────────────────────────────────────────────

fn stage_fingerprint(context: &Context, transaction: &Transaction, stage: u8) -> String {
    let mut parts = vec![
        format!("stage={stage}"),
        format!("candidate={}", transaction.candidate_sha),
        format!("version={}", transaction.version),
        format!("rustc={}", context.rustc),
        format!("scope={}", context.opts.release_scope),
        format!("build_inputs={}", build_inputs_hash(&context.repo)),
        format!("definition={}", context.definition_hash),
    ];
    match stage {
        STAGE_DOGFOOD => {
            parts.push(format!("model={}", context.lab.model));
            parts.push(format!("baseline={}", context.baseline_sha));
        }
        STAGE_PUBLISH => {
            parts.push(format!("branch={}", context.branch));
            parts.push(format!("slug={}", context.slug));
            parts.push(format!(
                "release_workflow={}",
                sha256_file(&context.repo.join(github::RELEASE_WORKFLOW_FILE)).unwrap_or_default()
            ));
        }
        _ => {}
    }
    crate::hash::sha256_text(&parts.join("\n"))
}

fn build_inputs_hash(repo: &Path) -> String {
    let mut digest = crate::hash::Sha256::new();
    for relative in ["Cargo.toml", "Cargo.lock", ".cargo/config.toml"] {
        digest.update(relative.as_bytes());
        digest.update(b"=");
        digest.update(
            sha256_file(&repo.join(relative))
                .unwrap_or_else(|| "MISSING".to_string())
                .as_bytes(),
        );
        digest.update(b"\n");
    }
    digest.finish()
}

// ── stage implementations ───────────────────────────────────────────────────

fn dogfood_cmd(context: &Context, sha: &str, layers: &str, run_id: Option<&str>) -> CommandSpec {
    let mut args = vec![
        "eval/scripts/release_check.py".to_string(),
        "--codeleveler-sha".to_string(),
        sha.to_string(),
        "--baseline-sha".to_string(),
        context.baseline_sha.clone(),
        "--model".to_string(),
        context.lab.model.clone(),
        "--release-scope".to_string(),
        context.opts.release_scope.clone(),
        "--layers".to_string(),
        layers.to_string(),
    ];
    if let Some(run_id) = run_id {
        args.push("--run-id".to_string());
        args.push(run_id.to_string());
    }
    // The machine-busy preflight protects a manually started run from a host
    // that cannot give it a fair measurement. This pipeline runs its own stages
    // back to back, so the load it observes is its own; `--force` says "the host
    // load is a statement about the host, not a product verdict".
    args.push("--force".to_string());
    CommandSpec::new("python3", args, &context.lab.root)
        .env("DOGFOOD_ROOT", context.lab.root.display().to_string())
}

/// Run one or more dogfood layers and require every check the stage exists for.
fn run_layers(
    context: &Context,
    transaction: &mut Transaction,
    stage: u8,
    layers_requested: &str,
    layers_required: &[&str],
) -> Result<(Vec<String>, String), Fail> {
    let spec = dogfood_cmd(
        context,
        &transaction.candidate_sha,
        layers_requested,
        transaction.dogfood_run_id.as_deref(),
    );
    let out = context.host.run(&spec)?;
    let text = format!("{}{}", out.stdout, out.stderr);
    write_stage_log(
        context,
        transaction,
        stage,
        &format!(
            "cmd: python3 {} (cwd {})\npid: {}\nexit: {}\n\n",
            spec.args.join(" "),
            context.lab.root.display(),
            std::process::id(),
            out.status
        ),
        &text,
    );
    let runs = qualify::find_release_runs(&text);
    let run_id = match runs.len() {
        0 => {
            return Err(Fail::new(
                "RELEASE_RUN_NOT_FOUND",
                format!(
                    "the dogfood gate named no eval/release/<timestamp>-<sha> directory\nexit {}\n{}",
                    out.status,
                    exec::tail(&text, 40)
                ),
            ));
        }
        1 => runs[0].clone(),
        _ => {
            return Err(Fail::new(
                "RELEASE_RUN_AMBIGUOUS",
                format!("the dogfood gate named several runs: {}", runs.join(", ")),
            ));
        }
    };
    // The transaction stores the bare run id; the lab-relative path is derived
    // wherever it is needed, so there is one representation of one fact.
    let run_id = state::bare_run_id(&run_id);
    transaction.dogfood_run_id = Some(run_id.clone());
    let mut detail = Vec::new();
    let mut reused_any = false;
    for layer in layers_required {
        let path = state::lab_run_dir(&context.lab.root, state::RELEASE_RUN_KIND, &run_id)
            .join(format!("{layer}.json"));
        let layer_text = std::fs::read_to_string(&path).map_err(|_| {
            Fail::new(
                "GATE_REPORT_MISMATCH",
                format!("{layer}.json is missing from {run_id}; the layer did not complete"),
            )
        })?;
        let doc = Json::parse(&layer_text).map_err(|err| {
            Fail::new(
                "GATE_REPORT_MISMATCH",
                format!("{layer}.json is not valid JSON: {err}"),
            )
        })?;
        let status = doc.at("status").as_str().unwrap_or("UNKNOWN");
        let reused = doc.get("reused").is_some() && !doc.at("reused").is_null();
        reused_any |= reused;
        if status != "PASS" && status != "WARN" {
            let failures = doc
                .at("checks")
                .as_arr()
                .unwrap_or(&[])
                .iter()
                .filter(|check| {
                    matches!(
                        check.at("status").as_str().unwrap_or(""),
                        "FAIL" | "TIMEOUT"
                    )
                })
                .map(|check| {
                    format!(
                        "{} ({})",
                        check.at("id").as_str().unwrap_or("check"),
                        check.at("detail").as_str().unwrap_or("no detail")
                    )
                })
                .collect::<Vec<_>>()
                .join("; ");
            return Err(Fail::new(
                "LAYER_FAILED",
                format!(
                    "{layer} status is {status}{}\nrun: {run_id}",
                    if failures.is_empty() {
                        String::new()
                    } else {
                        format!(": {failures}")
                    }
                ),
            ));
        }
        for required in required_checks(layer) {
            // A required check has to have run. A check that is present but
            // SKIPPED or UNPROVEN is the same absence with a more polite name.
            let found = doc
                .at("checks")
                .as_arr()
                .unwrap_or(&[])
                .iter()
                .find(|entry| entry.at("id").as_str() == Some(*required))
                .map(|entry| entry.at("status").as_str().unwrap_or("MISSING").to_string());
            match found.as_deref() {
                Some("PASS") | Some("WARN") => {}
                Some(status) => {
                    return Err(Fail::new(
                        "REQUIRED_CHECK_MISSING",
                        format!(
                            "{layer} reports `{required}` as {status}; this stage exists to obtain that check"
                        ),
                    ));
                }
                None => {
                    return Err(Fail::new(
                        "REQUIRED_CHECK_MISSING",
                        format!(
                            "{layer} has no `{required}` check; the layer did not run what this stage exists to obtain"
                        ),
                    ));
                }
            }
        }
        let coverage = feature_coverage_line(context, &run_id, layer);
        detail.push(format!(
            "{layer}: {status}{}{}",
            if reused { " (REUSED)" } else { "" },
            coverage
        ));
    }
    if reused_any {
        transaction.stage_record_mut(stage).reused = true;
    }
    Ok((detail, run_id))
}

/// The feature coverage verdict for this layer, when the gate recorded one.
fn feature_coverage_line(context: &Context, run_id: &str, layer: &str) -> String {
    let path = context
        .lab
        .root
        .join(run_id)
        .join(format!("feature_coverage.{layer}.json"));
    let Some(doc) = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| Json::parse(&text).ok())
    else {
        return String::new();
    };
    let status = doc.at("status").as_str().unwrap_or("UNKNOWN");
    let summary = doc.at("summary");
    format!(
        " | feature coverage {status} ({}/{} core covered, {} uncovered)",
        summary.at("core_covered").as_f64().unwrap_or(0.0) as i64,
        summary.at("core_total").as_f64().unwrap_or(0.0) as i64,
        summary.at("core_uncovered").as_f64().unwrap_or(0.0) as i64
    )
}

/// The checks each stage exists to obtain. Listing them here is what makes
/// "the layer passed" mean "the work happened" rather than "the file exists".
fn required_checks(layer: &str) -> &'static [&'static str] {
    match layer {
        "L0" => &["fmt", "clippy", "release_build"],
        // `feature_coverage_inventory` is the preflight: it proves this stage
        // checked that every feature is registered and mapped before the later
        // stages have to produce the evidence.
        "L1" => &[
            "workspace_tests",
            "prefix_cache_stability",
            "feature_coverage_inventory",
        ],
        "L2" => &["tui_tests"],
        _ => &[],
    }
}

fn stage_build(context: &Context, transaction: &mut Transaction) -> Result<Vec<String>, Fail> {
    println!("stage 1: build & static — fmt, clippy, release build, supply chain, binary identity");
    let (detail, run_id) = run_layers(context, transaction, STAGE_BUILD, "L0", &["L0"])?;
    let layout = evidence_layout(context, transaction, "L0", false)?;
    {
        let record = transaction.stage_record_mut(STAGE_BUILD);
        record.run_id = Some(run_id);
        record.evidence_hash = Some(layout.hash);
    }
    let mut out = detail;
    out.extend(layout.lines);
    Ok(out)
}

fn stage_tests(context: &Context, transaction: &mut Transaction) -> Result<Vec<String>, Fail> {
    println!(
        "stage 2: automated tests — workspace, contracts, permission, runtime, storage, protocol, session, cancellation, background, TUI, PTY"
    );
    let (detail, run_id) = run_layers(context, transaction, STAGE_TESTS, "L1,L2", &["L1", "L2"])?;
    let layout = evidence_layout(context, transaction, "L1,L2", false)?;
    {
        let record = transaction.stage_record_mut(STAGE_TESTS);
        record.run_id = Some(run_id);
        record.evidence_hash = Some(layout.hash);
    }
    let mut out = detail;
    out.extend(layout.lines);
    Ok(out)
}

fn stage_dogfood(context: &Context, transaction: &mut Transaction) -> Result<Vec<String>, Fail> {
    println!("stage 3: real dogfood — model-driven cases, performance evidence, RC qualification");
    let (detail, run_id) = run_layers(context, transaction, STAGE_DOGFOOD, "L3,L4", &["L3"])?;
    let mut out = detail;
    let l3_path =
        state::lab_run_dir(&context.lab.root, state::RELEASE_RUN_KIND, &run_id).join("L3.json");
    if let Some(l3) = std::fs::read_to_string(l3_path)
        .ok()
        .and_then(|text| Json::parse(&text).ok())
        && let Some(cases) = Cases::from_json_l3(&l3)
    {
        transaction.stage_record_mut(STAGE_DOGFOOD).cases = Some(cases.clone());
        out.push(format!(
            "L3 cases: {} total, {} pass, {} fail, {} warn, {} unproven, {} unstable",
            cases.total, cases.passed, cases.failed, cases.warn, cases.unproven, cases.unstable
        ));
    }
    out.extend(final_dogfood_verdict(context, &run_id)?.lines);
    out.extend(run_rc(context, transaction, &run_id)?);
    let layout = evidence_layout(context, transaction, "L3,L4", true)?;
    transaction.stage_record_mut(STAGE_DOGFOOD).evidence_hash = Some(layout.hash);
    out.extend(layout.lines);
    Ok(out)
}

struct Verdict {
    lines: Vec<String>,
}

fn final_dogfood_verdict(context: &Context, run_id: &str) -> Result<Verdict, Fail> {
    let run_dir = state::lab_run_dir(&context.lab.root, state::RELEASE_RUN_KIND, run_id);
    let out = context.host.run(&CommandSpec::new(
        "python3",
        [
            "eval/scripts/release_check.py",
            "--from-artifacts",
            &format!("{}/{run_id}", state::RELEASE_RUN_KIND),
            "--force",
        ],
        &context.lab.root,
    ))?;
    let text = format!("{}{}", out.stdout, out.stderr);
    let doc = std::fs::read_to_string(run_dir.join("release.json"))
        .ok()
        .and_then(|text| Json::parse(&text).ok())
        .ok_or_else(|| {
            Fail::new(
                "GATE_REPORT_MISMATCH",
                format!(
                    "{run_id}/release.json is missing after the final decision\n{}",
                    exec::tail(&text, 30)
                ),
            )
        })?;
    let decision = doc.at("decision");
    let verdict = decision.at("decision").as_str().unwrap_or("UNKNOWN");
    let complete = decision
        .at("qualification_complete")
        .as_bool()
        .unwrap_or(false);
    let exit = decision.at("exit_code").as_f64().unwrap_or(1.0) as i32;
    if exit != 0 && exit != 2 {
        let reason = decision
            .at("partial_reason")
            .as_str()
            .map(str::to_string)
            .unwrap_or_default();
        let mut detail = format!(
            "the release gate decided {verdict} (exit {exit}){}",
            if reason.is_empty() {
                String::new()
            } else {
                format!(": {reason}")
            }
        );
        let missing = layers_missing(doc.at("layers"));
        if !missing.is_empty() {
            detail.push_str(&format!("\nlayers not decided: {}", missing.join(", ")));
        }
        if !complete {
            detail
                .push_str("\nthe qualification is not complete, so there is no verdict to publish");
        }
        let blockers = decision
            .at("blockers")
            .as_arr()
            .unwrap_or(&[])
            .iter()
            .map(|blocker| {
                format!(
                    "{}: {}",
                    blocker.at("id").as_str().unwrap_or("blocker"),
                    blocker.at("detail").as_str().unwrap_or("")
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        if !blockers.is_empty() {
            detail.push_str(&format!("\nblockers: {blockers}"));
        }
        return Err(Fail::new("RELEASE_GATE_FAILED", detail));
    }
    let mut lines = vec![format!(
        "release gate: {verdict} (exit {exit}, complete={complete})"
    )];
    if exit == 2 {
        lines.push(
            "the gate returned CONDITIONAL PASS: the release proceeds and the report records why"
                .to_string(),
        );
    }
    Ok(Verdict { lines })
}

fn layers_missing(layers: &Json) -> Vec<String> {
    let mut missing = Vec::new();
    for layer in ["L0", "L1", "L2", "L3", "L4"] {
        let status = layers.at(layer).at("status").as_str().unwrap_or("MISSING");
        if status == "MISSING" || status == "SKIPPED" {
            missing.push(format!("{layer}={status}"));
        }
    }
    missing
}

fn run_rc(
    context: &Context,
    transaction: &mut Transaction,
    release_run: &str,
) -> Result<Vec<String>, Fail> {
    let reuse_from = if context.opts.reuse_prior_rc {
        find_prior_rc_run(context, &transaction.candidate_sha)
    } else {
        None
    };
    let mut args = vec![
        "eval/scripts/rc_check.py".to_string(),
        "--candidate-sha".to_string(),
        transaction.candidate_sha.clone(),
        "--release-run".to_string(),
        format!("{}/{release_run}", state::RELEASE_RUN_KIND),
        "--model".to_string(),
        context.lab.model.clone(),
        "--force".to_string(),
    ];
    if let Some(reuse_from) = &reuse_from {
        args.push("--reuse-from".to_string());
        args.push(reuse_from.clone());
    }
    let out = context.host.run(
        &CommandSpec::new("python3", args, &context.lab.root)
            .env("DOGFOOD_ROOT", context.lab.root.display().to_string()),
    )?;
    let text = format!("{}{}", out.stdout, out.stderr);
    let runs = rc_runs(&text);
    let run_id = match runs.len() {
        0 => {
            return Err(Fail::new(
                "RC_RUN_NOT_FOUND",
                format!(
                    "rc_check named no eval/rc/<timestamp>-<sha> directory\nexit {}\n{}",
                    out.status,
                    exec::tail(&text, 40)
                ),
            ));
        }
        1 => runs[0].clone(),
        _ => {
            return Err(Fail::new(
                "RC_RUN_AMBIGUOUS",
                format!("rc_check named several runs: {}", runs.join(", ")),
            ));
        }
    };
    let run_id = state::bare_run_id(&run_id);
    transaction.rc_run_id = Some(run_id.clone());
    let doc = std::fs::read_to_string(
        state::lab_run_dir(&context.lab.root, state::RC_RUN_KIND, &run_id).join("rc.json"),
    )
    .ok()
    .and_then(|text| Json::parse(&text).ok())
    .ok_or_else(|| {
        Fail::new(
            "RC_REPORT_MISSING",
            format!(
                "{run_id}/rc.json is missing; the RC qualification did not finish\n{}",
                exec::tail(&text, 30)
            ),
        )
    })?;
    let decision = doc.get("decision").unwrap_or(&doc);
    let verdict = decision
        .at("decision")
        .as_str()
        .unwrap_or("UNKNOWN")
        .to_string();
    let exit = decision.at("exit_code").as_f64().unwrap_or(1.0) as i32;
    if exit != 0 && exit != 2 {
        let blockers = decision
            .at("blockers")
            .as_arr()
            .unwrap_or(&[])
            .iter()
            .map(|blocker| {
                format!(
                    "{}: {}",
                    blocker.at("id").as_str().unwrap_or("blocker"),
                    blocker.at("detail").as_str().unwrap_or("")
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        return Err(Fail::new(
            "RC_QUALIFICATION_FAILED",
            format!("RC qualification decided {verdict} (exit {exit})\nrun: {run_id}\n{blockers}"),
        ));
    }
    let mut lines = vec![format!("RC qualification: {verdict} (exit {exit})")];
    if let Some(reuse) = doc.at("reuse").as_arr()
        && !reuse.is_empty()
    {
        lines.push(format!(
            "RC suites reused: {}{}",
            reuse.len(),
            if reuse_from.is_some() {
                " (from a prior run whose identity matched)"
            } else {
                ""
            }
        ));
    }
    Ok(lines)
}

fn rc_runs(text: &str) -> Vec<String> {
    let marker = "eval/rc/";
    let mut out = Vec::new();
    for (index, _) in text.match_indices(marker) {
        let rest = &text[index + marker.len()..];
        let id: String = rest
            .chars()
            .take_while(|ch| ch.is_ascii_digit() || *ch == '-' || ch.is_ascii_hexdigit())
            .collect();
        if id.len() < 15 {
            continue;
        }
        let full = format!("eval/rc/{id}");
        if !out.contains(&full) {
            out.push(full);
        }
    }
    out
}

/// The newest completed RC run for this candidate. `rc_check` re-checks every
/// identity field before it reuses anything, so *finding* a candidate run is not
/// the same as accepting its evidence.
fn find_prior_rc_run(context: &Context, sha: &str) -> Option<String> {
    let root = context.lab.root.join("eval/rc");
    let short = state::short_sha(sha);
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(&root)
        .ok()?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.ends_with(&short) && path.join("rc.json").is_file())
                .unwrap_or(false)
        })
        .collect();
    candidates.sort();
    candidates
        .pop()
        .map(|path| state::bare_run_id(&path.display().to_string()))
}

struct Layout {
    hash: String,
    lines: Vec<String>,
}

/// Digest of exactly the evidence a stage produced, so a resumed transaction can
/// prove the evidence it accepted is still the evidence on disk.
///
/// `include_rc` is only set by the stage that runs the RC qualification: an
/// artifact produced by a later stage must not change an earlier stage's digest,
/// or that digest could never be re-checked.
fn evidence_layout(
    context: &Context,
    transaction: &Transaction,
    layers: &str,
    include_rc: bool,
) -> Result<Layout, Fail> {
    let run_id = transaction
        .dogfood_run_id
        .clone()
        .ok_or_else(|| Fail::new("RELEASE_RUN_NOT_FOUND", "no dogfood run to hash"))?;
    let run_dir = state::lab_run_dir(&context.lab.root, state::RELEASE_RUN_KIND, &run_id);
    let mut digest = crate::hash::Sha256::new();
    let mut lines = Vec::new();
    for layer in layers.split(',') {
        let path = run_dir.join(format!("{layer}.json"));
        digest.update(layer.as_bytes());
        digest.update(b"=");
        match sha256_file(&path) {
            Some(hash) => {
                digest.update(hash.as_bytes());
                lines.push(format!("{layer}.json sha256={}", &hash[..16]));
            }
            None => {
                digest.update(b"MISSING");
                lines.push(format!("{layer}.json sha256=MISSING"));
            }
        }
        digest.update(b"\n");
    }
    if include_rc
        && let Some(rc) = &transaction.rc_run_id
        && let Some(hash) = sha256_file(
            &state::lab_run_dir(&context.lab.root, state::RC_RUN_KIND, rc).join("rc.json"),
        )
    {
        digest.update(b"rc.json=");
        digest.update(hash.as_bytes());
        lines.push(format!("rc.json sha256={}", &hash[..16]));
    }
    Ok(Layout {
        hash: digest.finish(),
        lines,
    })
}

// ── stage 4: CI/CD & publish ────────────────────────────────────────────────

fn stage_publish(context: &Context, transaction: &mut Transaction) -> Result<Vec<String>, Fail> {
    println!("stage 4: CI/CD & publish");
    let mut detail = Vec::new();
    let sha = transaction.candidate_sha.clone();
    let tag = format!("v{}", transaction.version);
    transaction.remote.tag = Some(tag.clone());

    let snap = git::snapshot(context.host, &context.repo)?;
    if snap.head != sha {
        return Err(Fail::new(
            "CANDIDATE_CHANGED",
            format!(
                "the candidate moved from {sha} to {}; the frozen commit is what gets published",
                snap.head
            ),
        ));
    }
    if !snap.porcelain.is_empty() {
        return Err(Fail::new(
            "WORKTREE_DIRTY",
            "the worktree changed after qualification; commit it and run ./dev publish --resume",
        ));
    }

    // 1-2. Re-verify version/source consistency on the frozen commit.
    let local = version::read_local(&context.repo)?;
    let declared = version::require_local(&local)?;
    if declared.to_string() != transaction.version {
        return Err(Fail::new(
            "VERSION_MISMATCH",
            format!(
                "the candidate declares {declared} but this release is {}",
                transaction.version
            ),
        ));
    }
    detail.push(format!(
        "version guard: {declared} matches the frozen commit"
    ));

    // 3. Push the frozen commit unless origin already has exactly it.
    let remote_head = remote_branch_sha(context, &context.branch)?;
    if remote_head.as_deref() == Some(sha.as_str()) {
        detail.push(format!(
            "push: origin/{} already at {}",
            context.branch,
            state::short_sha(&sha)
        ));
    } else {
        // The push names the frozen SHA as its source, not HEAD: a worktree that
        // moves under a running publish must not change what gets released.
        let refspec = format!("{sha}:refs/heads/{}", context.branch);
        let out = context
            .host
            .run(&exec::git(&context.repo, &["push", "origin", &refspec]))?;
        if out.status != 0 {
            return Err(Fail::new(
                "PUSH_FAILED",
                format!(
                    "git push origin HEAD exited {}\n{}\n\
                     the release commit {sha} is intact locally; retry with ./dev publish --resume",
                    out.status,
                    exec::tail(&out.stderr, 30)
                ),
            ));
        }
        detail.push(format!(
            "push: origin/{} -> {}",
            context.branch,
            state::short_sha(&sha)
        ));
    }
    transaction.remote.pushed_sha = Some(sha.clone());
    state::save_state(&context.repo, transaction)?;

    // 4. Wait for the workflow this repository requires, bound to this SHA.
    // The conclusion is recorded even when the wait ends in a failure: "CI said
    // no" is a fact the report has to carry.
    let ci = wait_for_ci(context, transaction, &sha)?;
    detail.extend(ci.lines);

    // 5-6. Dispatch (or adopt) the release workflow and follow it to a terminal
    // state. A dispatch is not a result.
    let run = ensure_release_run(context, transaction, &sha, &tag)?;
    transaction.remote.workflow_run_id = Some(run.id.clone());
    transaction.remote.workflow_run_status = Some(run.status.clone());
    transaction.remote.workflow_run_conclusion = run.conclusion.clone();
    state::save_state(&context.repo, transaction)?;
    detail.push(format!(
        "release workflow run {}: {}/{}",
        run.id,
        run.status,
        run.conclusion.clone().unwrap_or_else(|| "none".to_string())
    ));
    let jobs = github::run_jobs(context.host, &context.repo, &context.slug, &run.id)?;
    let not_green: Vec<String> = jobs
        .iter()
        .filter(|job| {
            !matches!(
                job.conclusion.as_deref(),
                Some("success") | Some("skipped") | None
            )
        })
        .map(|job| {
            format!(
                "{}={}",
                job.name,
                job.conclusion.clone().unwrap_or_default()
            )
        })
        .collect();
    if !not_green.is_empty() {
        detail.push(format!("failed jobs: {}", not_green.join(", ")));
    }

    // 7. Tag and release facts.
    let Some(tag_sha) = github::remote_tag_sha(context.host, &context.repo, &tag)? else {
        return Err(Fail::new(
            "TAG_MISSING",
            format!(
                "the release workflow finished without creating {tag}\n\
                 the tag and the release are created by the workflow after every artifact verifies"
            ),
        ));
    };
    if tag_sha != sha {
        return Err(Fail::new(
            "TAG_SHA_MISMATCH",
            format!("{tag} points at {tag_sha}, not at the qualified commit {sha}"),
        ));
    }
    transaction.remote.tag_sha = Some(tag_sha);
    detail.push(format!("tag: {tag} -> {sha}"));

    let Some(release) = github::release_view(context.host, &context.repo, &context.slug, &tag)?
    else {
        return Err(Fail::new(
            "RELEASE_MISSING",
            format!("{tag} exists but has no GitHub release"),
        ));
    };
    if release.is_draft {
        return Err(Fail::new(
            "RELEASE_STILL_DRAFT",
            format!("{tag} is still a draft release; it was not published"),
        ));
    }
    transaction.remote.release_state = Some(if release.is_prerelease {
        "prerelease".to_string()
    } else {
        "published".to_string()
    });
    transaction.remote.release_url = release.url.clone();
    transaction.remote.assets = release
        .assets
        .iter()
        .map(|asset| asset.name.clone())
        .collect();
    detail.push(format!(
        "release: {} ({} assets)",
        release.url.clone().unwrap_or_else(|| release.tag.clone()),
        release.assets.len()
    ));

    transaction.stage_record_mut(STAGE_PUBLISH).evidence_hash = Some(crate::hash::sha256_text(
        &transaction.remote.to_json().to_pretty(),
    ));
    // 8-11. Verify the published artifacts, not just their listing.
    if context.opts.verify_artifacts {
        let verify = verify_artifacts(context, &tag, &release, &sha)?;
        transaction.remote.verified_assets = verify.verified;
        detail.extend(verify.lines);
    } else {
        detail.push(
            "artifact verification: SKIPPED by --no-artifact-verify; the published checksums were NOT re-computed"
                .to_string(),
        );
    }
    Ok(detail)
}

fn remote_branch_sha(context: &Context, branch: &str) -> Result<Option<String>, Fail> {
    let out = context.host.run(&exec::git(
        &context.repo,
        &["ls-remote", "origin", &format!("refs/heads/{branch}")],
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
    Ok(out
        .stdout
        .split_whitespace()
        .next()
        .map(str::to_string)
        .filter(|sha| !sha.is_empty()))
}

struct CiWait {
    lines: Vec<String>,
}

fn wait_for_ci(
    context: &Context,
    transaction: &mut Transaction,
    sha: &str,
) -> Result<CiWait, Fail> {
    let workflow = github::workflow_name(&context.repo, github::CI_WORKFLOW_FILE)?;
    if !github::triggers_on_push(&context.repo, github::CI_WORKFLOW_FILE)? {
        return Err(Fail::new(
            "CI_WORKFLOW_NOT_PUSH_TRIGGERED",
            format!("{workflow} does not run on push, so it can never qualify this commit"),
        ));
    }
    let deadline = crate::clock::now() + context.opts.ci_timeout_seconds as f64;
    let mut lines = Vec::new();
    let mut last = String::new();
    loop {
        let runs = github::runs_for_commit(context.host, &context.repo, &context.slug, sha)?;
        let ours: Vec<&github::RunInfo> =
            runs.iter().filter(|run| run.workflow == workflow).collect();
        let state = latest_state(&ours);
        if state.label() != last {
            lines.push(format!(
                "CI `{workflow}` for {}: {}",
                state::short_sha(sha),
                state.label()
            ));
            last = state.label();
        }
        transaction.remote.ci_conclusion = Some(state.label());
        if state.is_terminal_pass() {
            let others = other_runs_summary(&runs, &workflow);
            if !others.is_empty() {
                lines.push(format!("other runs on this commit: {others}"));
            }
            return Ok(CiWait { lines });
        }
        if state.is_terminal() {
            return Err(Fail::new(
                "CI_FAILED",
                format!(
                    "CI `{workflow}` concluded {} for {}\n{}\n\
                     a commit whose required CI did not pass is never released",
                    state.label(),
                    sha,
                    ours.first()
                        .and_then(|run| run.url.clone())
                        .unwrap_or_default()
                ),
            ));
        }
        if crate::clock::now() >= deadline {
            return Err(Fail::new(
                "CI_TIMEOUT",
                format!(
                    "CI `{workflow}` did not reach a terminal state within {}s for {sha}\n\
                     last observed: {}\n\
                     rerun ./dev publish --resume to keep waiting on the same commit",
                    context.opts.ci_timeout_seconds,
                    state.label()
                ),
            ));
        }
        sleep(context.opts.poll_seconds);
    }
}

/// The newest run decides: a re-run supersedes the run it replaced.
fn latest_state(runs: &[&github::RunInfo]) -> github::RunState {
    runs.last()
        .map(|run| run.state())
        .unwrap_or(github::RunState::Queued)
}

fn other_runs_summary(runs: &[github::RunInfo], required: &str) -> String {
    runs.iter()
        .filter(|run| run.workflow != required)
        .map(|run| format!("{}={}", run.workflow, run.state().label()))
        .collect::<Vec<_>>()
        .join(", ")
}

fn ensure_release_run(
    context: &Context,
    transaction: &mut Transaction,
    sha: &str,
    tag: &str,
) -> Result<github::RunInfo, Fail> {
    let workflow = github::workflow_name(&context.repo, github::RELEASE_WORKFLOW_FILE)?;
    let existing: Vec<github::RunInfo> =
        github::runs_for_commit(context.host, &context.repo, &context.slug, sha)?
            .into_iter()
            .filter(|run| run.workflow == workflow)
            .collect();
    if let Some(in_flight) = existing.iter().find(|run| !run.state().is_terminal()) {
        println!(
            "adopting the release run already in flight for {}: {}",
            state::short_sha(sha),
            in_flight.id
        );
        return wait_for_run(context, transaction, &in_flight.id.clone(), tag);
    }
    if let Some(success) = existing.iter().find(|run| run.state().is_terminal_pass()) {
        println!(
            "the release workflow already succeeded for {}: run {}",
            state::short_sha(sha),
            success.id
        );
        return Ok(success.clone());
    }
    if let Some(failed) = existing.iter().find(|run| run.state().is_terminal()) {
        let state = failed.state();
        if !context.opts.retry_remote {
            return Err(Fail::new(
                "RELEASE_RUN_FAILED",
                format!(
                    "the release workflow for {} already concluded {}\nrun: {}\n\
                     ./dev will not silently dispatch a second release for the same commit;\n\
                     re-run the workflow in GitHub, or pass --retry-remote to dispatch a new attempt",
                    state::short_sha(sha),
                    state.label(),
                    failed.url.clone().unwrap_or_default()
                ),
            ));
        }
        println!(
            "previous release run {} concluded {}; --retry-remote asks for a new attempt",
            failed.id,
            state.label()
        );
    }
    // Decide the dispatch from remote facts: a published release is immutable,
    // an existing tag or draft at this commit is resumable.
    if let Some(release) = github::release_view(context.host, &context.repo, &context.slug, tag)?
        && !release.is_draft
    {
        return Err(Fail::new(
            "RELEASE_EXISTS",
            format!(
                "{tag} is already published at {}; a published version is immutable",
                release.url.clone().unwrap_or_else(|| release.tag.clone())
            ),
        ));
    }
    if let Some(existing_tag) = github::remote_tag_sha(context.host, &context.repo, tag)?
        && existing_tag != sha
    {
        return Err(Fail::new(
            "TAG_EXISTS",
            format!("{tag} exists at {existing_tag}, not at the qualified commit {sha}"),
        ));
    }
    let version = transaction.version.clone();
    github::dispatch_release(
        context.host,
        &context.repo,
        &context.slug,
        &context.branch,
        &version,
        sha,
    )?;
    println!(
        "dispatched release.yml version={} commit={}",
        transaction.version, sha
    );
    // A dispatch does not return a run id, so the run for this exact commit is
    // discovered before it is followed: an unnamed poll would be a guess.
    let deadline = crate::clock::now() + context.opts.ci_timeout_seconds as f64;
    loop {
        let runs = github::runs_for_commit(context.host, &context.repo, &context.slug, sha)?;
        let ours: Vec<&github::RunInfo> = runs
            .iter()
            .filter(|run| run.workflow == workflow && run.event == "workflow_dispatch")
            .collect();
        if let Some(run) = ours.last() {
            return wait_for_run(context, transaction, &run.id.clone(), tag);
        }
        if crate::clock::now() >= deadline {
            return Err(Fail::new(
                "RELEASE_RUN_NOT_FOUND",
                format!(
                    "dispatched release.yml for {sha} but no run appeared within {}s\n\
                     check the workflow's trigger conditions before retrying",
                    context.opts.ci_timeout_seconds
                ),
            ));
        }
        sleep(context.opts.poll_seconds);
    }
}

fn wait_for_run(
    context: &Context,
    transaction: &mut Transaction,
    id: &str,
    tag: &str,
) -> Result<github::RunInfo, Fail> {
    let deadline = crate::clock::now() + context.opts.ci_timeout_seconds as f64;
    let mut last = String::new();
    loop {
        let run = github::run_view(context.host, &context.repo, &context.slug, id)?;
        let state = run.state();
        transaction.remote.workflow_run_status = Some(run.status.clone());
        transaction.remote.workflow_run_conclusion = run.conclusion.clone();
        if state.label() != last {
            println!("release workflow {id}: {}", state.label());
            last = state.label();
        }
        if state.is_terminal_pass() {
            return Ok(run);
        }
        if state.is_terminal() {
            let failed = github::run_jobs(context.host, &context.repo, &context.slug, id)
                .unwrap_or_default()
                .into_iter()
                .filter(|job| {
                    !matches!(
                        job.conclusion.as_deref(),
                        Some("success") | Some("skipped") | None
                    )
                })
                .map(|job| {
                    format!(
                        "{}={}",
                        job.name,
                        job.conclusion.clone().unwrap_or_default()
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            return Err(Fail::new(
                "RELEASE_RUN_FAILED",
                format!(
                    "the release workflow concluded {}\nrun: {}\nfailed jobs: {}\n\
                     {tag} has no published release; nothing was released",
                    state.label(),
                    run.url.clone().unwrap_or_default(),
                    if failed.is_empty() {
                        "none reported".to_string()
                    } else {
                        failed
                    }
                ),
            ));
        }
        if crate::clock::now() >= deadline {
            return Err(Fail::new(
                "RELEASE_RUN_TIMEOUT",
                format!(
                    "the release workflow did not reach a terminal state within {}s (last: {})\nrun: {}\n\
                     rerun ./dev publish --resume to keep following the same run",
                    context.opts.ci_timeout_seconds,
                    state.label(),
                    run.url.clone().unwrap_or_default()
                ),
            ));
        }
        sleep(context.opts.poll_seconds);
    }
}

/// Every artifact the release workflow is expected to publish, read from the
/// workflow's own build matrix instead of a list copied into this file.
pub fn expected_artifacts(repo: &Path, version: &str) -> Result<Vec<String>, Fail> {
    let path = repo.join(github::RELEASE_WORKFLOW_FILE);
    let text = std::fs::read_to_string(&path).map_err(|err| {
        Fail::new(
            "WORKFLOW_MISSING",
            format!("could not read {}: {err}", path.display()),
        )
    })?;
    let mut targets = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("target:") {
            let target = rest.trim().trim_matches('"').trim_matches('\'').to_string();
            if !target.is_empty()
                && !target.contains("matrix")
                && !target.contains("${{")
                && !targets.contains(&target)
            {
                targets.push(target);
            }
        }
    }
    if targets.is_empty() {
        return Err(Fail::new(
            "WORKFLOW_MATRIX_UNREADABLE",
            format!(
                "{} declares no build targets, so the published artifacts cannot be enumerated",
                github::RELEASE_WORKFLOW_FILE
            ),
        ));
    }
    let mut names = Vec::new();
    for target in targets {
        let suffix = if target.contains("windows") {
            "zip"
        } else {
            "tar.gz"
        };
        let name = format!("leveler-v{version}-{target}.{suffix}");
        names.push(name.clone());
        names.push(format!("{name}.sha256"));
    }
    names.sort();
    Ok(names)
}

struct Verify {
    verified: Vec<String>,
    lines: Vec<String>,
}

fn verify_artifacts(
    context: &Context,
    tag: &str,
    release: &github::ReleaseInfo,
    sha: &str,
) -> Result<Verify, Fail> {
    let version = tag.trim_start_matches('v').to_string();
    let expected = expected_artifacts(&context.repo, &version)?;
    let published: Vec<&str> = release
        .assets
        .iter()
        .map(|asset| asset.name.as_str())
        .collect();
    let missing: Vec<&String> = expected
        .iter()
        .filter(|name| !published.contains(&name.as_str()))
        .collect();
    if !missing.is_empty() {
        return Err(Fail::new(
            "RELEASE_ASSET_MISSING",
            format!(
                "{tag} is missing {} of {} expected artifact(s): {}",
                missing.len(),
                expected.len(),
                missing
                    .iter()
                    .map(|name| name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
    }
    let unfinished: Vec<&str> = release
        .assets
        .iter()
        .filter(|asset| asset.state != "uploaded")
        .map(|asset| asset.name.as_str())
        .collect();
    if !unfinished.is_empty() {
        return Err(Fail::new(
            "RELEASE_ASSET_INCOMPLETE",
            format!(
                "{} asset(s) are not fully uploaded: {}",
                unfinished.len(),
                unfinished.join(", ")
            ),
        ));
    }
    let mut lines = vec![format!(
        "artifacts: {} expected, {} published, all present",
        expected.len(),
        release.assets.len()
    )];
    let dir = context
        .repo
        .join(".dev/release")
        .join(format!("verified-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    github::download_assets(
        context.host,
        &context.repo,
        &context.slug,
        tag,
        &dir,
        &format!("leveler-v{version}-*"),
    )?;
    let mut verified = Vec::new();
    for name in &expected {
        if name.ends_with(".sha256") {
            continue;
        }
        let path = dir.join(name);
        if !path.is_file() {
            return Err(Fail::new(
                "RELEASE_ASSET_DOWNLOAD_FAILED",
                format!("{name} was not downloaded from {tag}"),
            ));
        }
        let expected_digest = read_checksum(&dir.join(format!("{name}.sha256")))?;
        let actual = sha256_file(&path).ok_or_else(|| {
            Fail::new(
                "RELEASE_ASSET_DOWNLOAD_FAILED",
                format!("could not hash {}", path.display()),
            )
        })?;
        if expected_digest != actual {
            return Err(Fail::new(
                "RELEASE_CHECKSUM_MISMATCH",
                format!(
                    "{name}: the published sha256 is {expected_digest} but the downloaded bytes hash to {actual}"
                ),
            ));
        }
        lines.push(format!("{name}: sha256 {actual} verified"));
        verified.push(name.clone());
    }
    // The artifacts must be installable and runnable, not merely downloadable.
    match runnable_artifact(&expected) {
        Some(name) => {
            let path = dir.join(&name);
            let unpack = dir.join("unpacked");
            let _ = std::fs::remove_dir_all(&unpack);
            std::fs::create_dir_all(&unpack).map_err(|err| {
                Fail::new(
                    "RELEASE_STATE_IO",
                    format!("could not create {}: {err}", unpack.display()),
                )
            })?;
            unpack_archive(context.host, &context.repo, &path, &name, &unpack)?;
            let binary = find_binary(&unpack)?;
            let out = context.host.run(&CommandSpec::new(
                binary.display().to_string(),
                ["--version"],
                &unpack,
            ))?;
            let text = format!("{}{}", out.stdout, out.stderr);
            if out.status != 0 || !text.contains(&version) {
                return Err(Fail::new(
                    "RELEASE_ARTIFACT_NOT_RUNNABLE",
                    format!(
                        "{} --version exited {} and said:\n{}\nexpected it to report version {version}",
                        binary.display(),
                        out.status,
                        exec::first_lines(&text, 10)
                    ),
                ));
            }
            lines.push(format!(
                "install check: {name} runs and reports {}",
                first_line(&text)
            ));
        }
        None => lines.push(
            "install check: SKIPPED — no published artifact matches this host's platform; the checksums were still verified"
                .to_string(),
        ),
    }
    lines.push(format!(
        "verified {} artifact(s) against {tag} at {}",
        verified.len(),
        state::short_sha(sha)
    ));
    Ok(Verify { verified, lines })
}

fn read_checksum(path: &Path) -> Result<String, Fail> {
    let text = std::fs::read_to_string(path).map_err(|err| {
        Fail::new(
            "RELEASE_CHECKSUM_MISSING",
            format!("could not read {}: {err}", path.display()),
        )
    })?;
    let digest = text
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase();
    if digest.len() != 64 || !digest.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(Fail::new(
            "RELEASE_CHECKSUM_MISSING",
            format!("{} does not hold a sha256 digest", path.display()),
        ));
    }
    Ok(digest)
}

fn runnable_artifact(expected: &[String]) -> Option<String> {
    let (want_arch, want_os) = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => ("aarch64-apple-darwin", "apple-darwin"),
        ("macos", "x86_64") => ("x86_64-apple-darwin", "apple-darwin"),
        ("linux", "x86_64") => ("x86_64-unknown-linux-gnu", "linux-gnu"),
        ("windows", "x86_64") => ("x86_64-pc-windows-msvc", "windows-msvc"),
        _ => return None,
    };
    let candidates: Vec<&String> = expected
        .iter()
        .filter(|name| !name.ends_with(".sha256"))
        .collect();
    candidates
        .iter()
        .find(|name| name.contains(want_arch))
        .or_else(|| candidates.iter().find(|name| name.contains(want_os)))
        .map(|name| (*name).clone())
}

fn unpack_archive(
    host: &dyn Host,
    repo: &Path,
    path: &Path,
    name: &str,
    into: &Path,
) -> Result<(), Fail> {
    let out = if name.ends_with(".zip") {
        host.run(&CommandSpec::new(
            "unzip",
            [
                "-q",
                "-o",
                &path.display().to_string(),
                "-d",
                &into.display().to_string(),
            ],
            repo,
        ))?
    } else {
        host.run(&CommandSpec::new(
            "tar",
            [
                "-xzf",
                &path.display().to_string(),
                "-C",
                &into.display().to_string(),
            ],
            repo,
        ))?
    };
    if out.status != 0 {
        return Err(Fail::new(
            "RELEASE_ARTIFACT_NOT_INSTALLABLE",
            format!(
                "could not unpack {name}\n{}",
                exec::first_lines(&out.stderr, 10)
            ),
        ));
    }
    Ok(())
}

fn find_binary(root: &Path) -> Result<PathBuf, Fail> {
    let mut stack = vec![root.to_path_buf()];
    let mut found: Option<PathBuf> = None;
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            if name == "leveler" || name == "leveler.exe" {
                found = Some(path);
            }
        }
    }
    found.ok_or_else(|| {
        Fail::new(
            "RELEASE_ARTIFACT_NOT_INSTALLABLE",
            format!(
                "the artifact holds no leveler binary under {}",
                root.display()
            ),
        )
    })
}

fn sleep(seconds: u64) {
    std::thread::sleep(std::time::Duration::from_secs(seconds.max(1)));
}
