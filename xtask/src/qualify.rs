//! Facts the release gate publishes that the harness must read without guessing.
//!
//! These are the lab-facing primitives shared by every qualification entry
//! (`./dev pre-release`, `./dev release`, `./dev dogfood`, `./dev publish`):
//! where the baseline commit lives, whether the candidate is in the lab clone,
//! and which run directory the gate actually wrote. The driving logic is *not*
//! here — it is one stage machine in [`crate::pipeline`], so a local
//! qualification and a publish cannot disagree about what a stage means.

use std::path::Path;

use crate::error::Fail;
use crate::exec::{self, Host};

/// Run directories the gate WROTE, preferring them over ones it merely named.
///
/// `release_check` ends with a `written:` section listing exactly the files this
/// invocation produced. Every other run path in the output belongs to somebody
/// else's run — a reuse note, a precondition naming the release run — and those
/// contain `eval/release/` too, in absolute paths from paths this run does not
/// own. Only the written section says which run this is. A gate that died before
/// writing one still falls back to the named runs, which is what the early-exit
/// paths report.
pub(crate) fn find_release_runs(text: &str) -> Vec<String> {
    let written = collect_runs(text, Scope::Written);
    if !written.is_empty() {
        return written;
    }
    collect_runs(text, Scope::Everything)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    /// Only the paths under `written:`.
    Written,
    /// Every run path in the text, written or merely named.
    Everything,
}

fn collect_runs(text: &str, scope: Scope) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_written = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("written:") {
            in_written = true;
            push_runs(rest, &mut out);
            continue;
        }
        if in_written && trimmed.is_empty() {
            in_written = false;
            continue;
        }
        if scope == Scope::Written && !in_written {
            continue;
        }
        push_runs(line, &mut out);
    }
    out
}

fn push_runs(line: &str, out: &mut Vec<String>) {
    let marker = "eval/release/";
    for (index, _) in line.match_indices(marker) {
        let rest = &line[index + marker.len()..];
        let Some(id) = take_run_id(rest) else {
            continue;
        };
        let full = format!("{marker}{id}");
        if !out.contains(&full) {
            out.push(full);
        }
    }
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

pub(crate) fn ensure_in_lab(
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

pub(crate) fn read_baseline(lab: &Path) -> Result<String, Fail> {
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
