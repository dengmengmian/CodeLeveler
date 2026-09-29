use std::path::Path;

use crate::error::{Fail, Report};
use crate::exec::{self, CommandSpec, Host, ProcessOut};
use crate::git::{self, Snapshot};
use crate::root::{self, Env, GateOpts, Lab};
use crate::version;

#[derive(Debug)]
pub(crate) struct Qualification {
    pub exit_code: i32,
    pub release_exit: i32,
    pub rc_exit: Option<i32>,
    pub run: Option<String>,
    pub baseline: String,
    pub notes: Vec<String>,
}

pub fn run_pre_release(
    host: &dyn Host,
    repo: &Path,
    env: &Env,
    opts: &GateOpts,
) -> Result<Report, Fail> {
    let snap = git::snapshot(host, repo)?;
    git::require_clean(&snap)?;
    run_pre_release_locked(host, repo, env, opts, &snap)
}

pub fn run_dogfood(
    host: &dyn Host,
    repo: &Path,
    env: &Env,
    opts: &GateOpts,
) -> Result<Report, Fail> {
    let snap = git::snapshot(host, repo)?;
    git::require_clean(&snap)?;
    let qual = run_dogfood_locked(host, repo, env, opts, &snap)?;
    Ok(format_dogfood(&snap.head, &qual))
}

pub fn run_release(
    host: &dyn Host,
    repo: &Path,
    env: &Env,
    opts: &GateOpts,
) -> Result<Report, Fail> {
    let snap = git::snapshot(host, repo)?;
    git::require_clean(&snap)?;
    run_release_locked(host, repo, env, opts, &snap)
}

pub(crate) fn run_pre_release_locked(
    host: &dyn Host,
    repo: &Path,
    env: &Env,
    opts: &GateOpts,
    locked: &Snapshot,
) -> Result<Report, Fail> {
    git::confirm(host, repo, locked)?;
    let local = version::read_local(repo)?;
    version::require_local(&local)?;
    let lab = root::resolve_lab(repo, opts, env)?;
    let baseline = read_baseline(&lab.root)?;
    let mut notes = Vec::new();
    if let Some(note) = ensure_in_lab(host, repo, &lab.root, &locked.head)? {
        notes.push(note);
    }
    let out = host.run(&release_cmd(
        &lab,
        &locked.head,
        &baseline,
        Some("L0,L1,L2"),
    ))?;
    git::confirm(host, repo, locked)?;
    let body = judge_pre_release(&lab.root, &combined(&out), out.status)?;
    let mut lines = vec![
        format!("Candidate: {}", locked.head),
        "Pre-release: PASS".to_string(),
        body,
    ];
    lines.extend(notes);
    Ok(Report::ok(lines.join("\n")))
}

pub(crate) fn run_dogfood_locked(
    host: &dyn Host,
    repo: &Path,
    env: &Env,
    opts: &GateOpts,
    locked: &Snapshot,
) -> Result<Qualification, Fail> {
    git::confirm(host, repo, locked)?;
    let local = version::read_local(repo)?;
    version::require_local(&local)?;
    let lab = root::resolve_lab(repo, opts, env)?;
    let baseline = read_baseline(&lab.root)?;
    let mut notes = Vec::new();
    if let Some(note) = ensure_in_lab(host, repo, &lab.root, &locked.head)? {
        notes.push(note);
    }
    let out = host.run(&release_cmd(&lab, &locked.head, &baseline, None))?;
    git::confirm(host, repo, locked)?;
    let runs = find_release_runs(&combined(&out));
    let release_exit = normalize_exit(out.status);
    if release_exit != 0 && release_exit != 2 {
        return Ok(Qualification {
            exit_code: dogfood_exit(release_exit, None),
            release_exit,
            rc_exit: None,
            run: optional_run(&runs)?,
            baseline,
            notes,
        });
    }
    let run = require_run(&runs)?;
    let rc = host.run(&rc_cmd(&lab, &locked.head, &run))?;
    git::confirm(host, repo, locked)?;
    let rc_exit = normalize_exit(rc.status);
    Ok(Qualification {
        exit_code: dogfood_exit(release_exit, Some(rc_exit)),
        release_exit,
        rc_exit: Some(rc_exit),
        run: Some(run),
        baseline,
        notes,
    })
}

pub(crate) fn run_release_locked(
    host: &dyn Host,
    repo: &Path,
    env: &Env,
    opts: &GateOpts,
    locked: &Snapshot,
) -> Result<Report, Fail> {
    let pre = run_pre_release_locked(host, repo, env, opts, locked)?;
    let qual = run_dogfood_locked(host, repo, env, opts, locked)?;
    Ok(format_release(&locked.head, &pre, &qual))
}

pub(crate) fn dogfood_exit(release_exit: i32, rc_exit: Option<i32>) -> i32 {
    if release_exit != 0 && release_exit != 2 {
        return if release_exit > 0 { release_exit } else { 1 };
    }
    match rc_exit {
        Some(0) if release_exit == 0 => 0,
        Some(0) | Some(2) => 2,
        Some(code) if code > 0 => code,
        _ => 1,
    }
}

pub(crate) fn verdict(code: i32) -> &'static str {
    match code {
        0 => "PASS",
        2 => "CONDITIONAL PASS",
        3 => "NOT_STARTED",
        4 => "RC_INCOMPLETE",
        _ => "FAIL",
    }
}

pub(crate) fn judge_pre_release(lab: &Path, output: &str, status: i32) -> Result<String, Fail> {
    if status == 3 {
        return Err(Fail::new("NOT_STARTED", "release gate returned NOT_STARTED (exit 3)").exit(3));
    }
    let run = require_run(&find_release_runs(output))?;
    let mut lines = Vec::new();
    for layer in ["L0", "L1", "L2"] {
        let path = lab.join(&run).join(format!("{layer}.json"));
        let json = std::fs::read_to_string(&path).map_err(|_| {
            Fail::new(
                "PRE_RELEASE_FAILED",
                format!("{layer}.json is missing from {run}"),
            )
        })?;
        let layer_status = first_status(&json).ok_or_else(|| {
            Fail::new(
                "GATE_REPORT_MISMATCH",
                format!("{layer}.json has no layer status"),
            )
        })?;
        if layer == "L0" {
            judge_fmt(&json)?;
        }
        if layer_status != "PASS" && layer_status != "WARN" {
            return Err(Fail::new(
                "PRE_RELEASE_FAILED",
                format!("{layer} status is {layer_status}"),
            ));
        }
        lines.push(format!("{layer}: {layer_status}"));
    }
    lines.push(format!("Release run: {run}"));
    Ok(lines.join("\n"))
}

fn judge_fmt(json: &str) -> Result<(), Fail> {
    match check_status(json, "fmt") {
        Some(status) if status == "PASS" || status == "WARN" => Ok(()),
        Some(_) => Err(Fail::new(
            "FMT_CHECK_FAIL",
            "L0 fmt check failed; ./dev does not run rustfmt on a release candidate",
        )),
        None => Err(Fail::new(
            "GATE_REPORT_MISMATCH",
            "L0.json has no fmt check",
        )),
    }
}

pub(crate) fn find_release_runs(text: &str) -> Vec<String> {
    let marker = "eval/release/";
    let mut out = Vec::new();
    for (index, _) in text.match_indices(marker) {
        let rest = &text[index + marker.len()..];
        let Some(id) = take_run_id(rest) else {
            continue;
        };
        let full = format!("eval/release/{id}");
        if !out.contains(&full) {
            out.push(full);
        }
    }
    out
}

fn take_run_id(rest: &str) -> Option<&str> {
    let bytes = rest.as_bytes();
    if bytes.len() < 28 {
        return None;
    }
    let date = &bytes[..8];
    let time = &bytes[9..15];
    let sha = &bytes[16..28];
    if date.iter().all(|c| c.is_ascii_digit())
        && bytes[8] == b'-'
        && time.iter().all(|c| c.is_ascii_digit())
        && bytes[15] == b'-'
        && sha.iter().all(|c| c.is_ascii_hexdigit())
    {
        Some(&rest[..28])
    } else {
        None
    }
}

fn first_status(json: &str) -> Option<String> {
    json_string_field(json, "status")
}

fn check_status(json: &str, id: &str) -> Option<String> {
    let want = format!("\"{id}\"");
    let mut rest = json;
    while let Some(index) = rest.find("\"id\"") {
        let tail = &rest[index + 4..];
        let trimmed = trim_ws_colon(tail);
        if let Some(after) = trimmed.strip_prefix(&want) {
            let end = after.find("\"id\"").unwrap_or(after.len());
            return json_string_field(&after[..end], "status");
        }
        rest = tail;
    }
    None
}

fn json_string_field(text: &str, key: &str) -> Option<String> {
    let marker = format!("\"{key}\"");
    let index = text.find(&marker)?;
    let rest = trim_ws_colon(&text[index + marker.len()..]);
    let rest = rest.strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
            continue;
        }
        if ch == '"' {
            return Some(out);
        }
        out.push(ch);
    }
    None
}

fn trim_ws_colon(text: &str) -> &str {
    let text = text.trim_start();
    text.strip_prefix(':').map(str::trim_start).unwrap_or(text)
}

fn release_cmd(lab: &Lab, sha: &str, baseline: &str, layers: Option<&str>) -> CommandSpec {
    let mut args = vec![
        "eval/scripts/release_check.py".to_string(),
        "--codeleveler-sha".to_string(),
        sha.to_string(),
        "--baseline-sha".to_string(),
        baseline.to_string(),
        "--model".to_string(),
        lab.model.clone(),
    ];
    if let Some(layers) = layers {
        args.push("--layers".to_string());
        args.push(layers.to_string());
        args.push("--no-pty".to_string());
    }
    CommandSpec::new("python3", args, &lab.root).env("DOGFOOD_ROOT", lab.root.display().to_string())
}

fn rc_cmd(lab: &Lab, sha: &str, run: &str) -> CommandSpec {
    CommandSpec::new(
        "python3",
        [
            "eval/scripts/rc_check.py",
            "--candidate-sha",
            sha,
            "--release-run",
            run,
            "--model",
            lab.model.as_str(),
        ],
        &lab.root,
    )
    .env("DOGFOOD_ROOT", lab.root.display().to_string())
}

fn ensure_in_lab(
    host: &dyn Host,
    repo: &Path,
    lab: &Path,
    sha: &str,
) -> Result<Option<String>, Fail> {
    let clone = lab.join("repos/codeleveler");
    if !clone.is_dir() {
        return Err(Fail::new(
            "DOGFOOD_CLONE_MISSING",
            format!(
                "dogfood lab has no repos/codeleveler checkout: {}",
                clone.display()
            ),
        ));
    }
    if object_exists(host, &clone, sha)? {
        return Ok(None);
    }
    let fetched = host.run(&exec::git(
        &clone,
        &["fetch", &repo.display().to_string(), sha],
    ))?;
    if fetched.status == 0 && object_exists(host, &clone, sha)? {
        return Ok(Some(format!(
            "fetched {sha} into dogfood repos/codeleveler"
        )));
    }
    Err(Fail::new(
        "CANDIDATE_NOT_IN_DOGFOOD",
        format!(
            "{sha} is not in {}\n{}",
            clone.display(),
            exec::first_lines(&fetched.stderr, 20)
        ),
    ))
}

fn object_exists(host: &dyn Host, clone: &Path, sha: &str) -> Result<bool, Fail> {
    let spec = exec::git(clone, &["cat-file", "-e", &format!("{sha}^{{commit}}")]);
    Ok(host.run(&spec)?.status == 0)
}

fn read_baseline(lab: &Path) -> Result<String, Fail> {
    let path = lab.join("eval/release/BASELINE");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(Fail::new(
                "BASELINE_MISSING",
                format!("missing {}", path.display()),
            ));
        }
        Err(err) => return Err(Fail::new("BASELINE_MISSING", err.to_string())),
    };
    let line = text.trim();
    if line.is_empty() {
        return Err(Fail::new(
            "BASELINE_MISSING",
            format!("{} is empty", path.display()),
        ));
    }
    if !(7..=40).contains(&line.len()) || !line.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err(Fail::new(
            "BASELINE_INVALID",
            format!("baseline is not a commit sha: {line}"),
        ));
    }
    Ok(line.to_string())
}

fn optional_run(runs: &[String]) -> Result<Option<String>, Fail> {
    match runs.len() {
        0 => Ok(None),
        1 => Ok(Some(runs[0].clone())),
        _ => Err(ambiguous(runs)),
    }
}

fn require_run(runs: &[String]) -> Result<String, Fail> {
    match runs.len() {
        0 => Err(Fail::new(
            "RELEASE_RUN_NOT_FOUND",
            "release gate output has no eval/release/<timestamp>-<sha> directory",
        )),
        1 => Ok(runs[0].clone()),
        _ => Err(ambiguous(runs)),
    }
}

fn ambiguous(runs: &[String]) -> Fail {
    Fail::new(
        "RELEASE_RUN_AMBIGUOUS",
        format!("release gate named multiple runs: {}", runs.join(", ")),
    )
}

fn combined(out: &ProcessOut) -> String {
    format!("{}{}", out.stdout, out.stderr)
}

fn normalize_exit(status: i32) -> i32 {
    if status < 0 { 1 } else { status }
}

fn format_dogfood(sha: &str, qual: &Qualification) -> Report {
    let rc = qual.rc_exit.map(verdict).unwrap_or("NOT_STARTED");
    let mut lines = vec![
        format!("Candidate: {sha}"),
        format!("Baseline: {}", qual.baseline),
        format!("Release qualification: {}", verdict(qual.release_exit)),
        format!(
            "Release run: {}",
            qual.run.as_deref().unwrap_or("not found")
        ),
        format!("RC Qualification: {rc}"),
        format!("Dogfood: {}", verdict(qual.exit_code)),
    ];
    lines.extend(qual.notes.iter().cloned());
    Report::with_code(qual.exit_code, lines.join("\n"))
}

fn format_release(sha: &str, pre: &Report, qual: &Qualification) -> Report {
    let detail = pre
        .text
        .lines()
        .filter(|line| {
            line.starts_with("L0:")
                || line.starts_with("L1:")
                || line.starts_with("L2:")
                || line.starts_with("Release run:")
                || line.starts_with("fetched ")
        })
        .collect::<Vec<_>>()
        .join("\n");
    let ready = match qual.exit_code {
        0 => "YES",
        2 => "CONDITIONAL",
        _ => "NO",
    };
    let rc = qual.rc_exit.map(verdict).unwrap_or("NOT_STARTED");
    let mut lines = vec![
        format!("Candidate: {sha}"),
        "Release Gate: PASS".to_string(),
        detail,
        format!("Dogfood: {}", verdict(qual.release_exit)),
        format!("RC Qualification: {rc}"),
        format!("Ready to release: {ready}"),
    ];
    lines.extend(qual.notes.iter().cloned());
    Report::with_code(qual.exit_code, lines.join("\n"))
}
