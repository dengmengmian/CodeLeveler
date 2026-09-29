use std::path::{Path, PathBuf};

use crate::error::{Fail, Report};
use crate::exec::{self, Host};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Default,
    All,
}

pub fn parse_args(args: &[String]) -> Result<Mode, Fail> {
    match args {
        [] => Ok(Mode::Default),
        [flag] if flag == "--all" || flag == "build" => Ok(Mode::All),
        [flag] if flag == "eval" => Err(Fail::new(
            "UNKNOWN_CLEAN_TARGET",
            "eval is not a CodeLeveler clean target\n\
             dogfood lab evidence is reclaimed by eval/scripts/reclaim_builds.py in the dogfood lab",
        )),
        [flag] if flag == "reports" => Err(Fail::new(
            "UNKNOWN_CLEAN_TARGET",
            "reports is not a CodeLeveler clean target",
        )),
        [flag] => Err(Fail::new(
            "UNKNOWN_CLEAN_TARGET",
            format!("{flag} is not a registered clean target"),
        )),
        _ => Err(Fail::new(
            "CLEAN_ARGS",
            "use ./dev clean, ./dev clean --all, or ./dev clean build",
        )),
    }
}

pub fn apply(host: &dyn Host, repo: &Path, mode: Mode) -> Result<Report, Fail> {
    let repo = repo
        .canonicalize()
        .map_err(|err| Fail::new("REPO_NOT_FOUND", format!("{}: {err}", repo.display())))?;
    let mut removed = Vec::new();
    for path in default_paths(&repo)? {
        if remove_registered(&repo, &path)? {
            removed.push(display(&repo, &path));
        }
    }
    if mode == Mode::All {
        for rel in ["apps/leveler-mobile/build", "crates/leveler-web/web/dist"] {
            let path = repo.join(rel);
            if remove_registered(&repo, &path)? {
                removed.push(rel.to_string());
            }
        }
        cargo_clean(host, &repo, &[])?;
        removed.push("cargo clean".to_string());
        let shared = repo.join(".codeleveler-target");
        match std::fs::symlink_metadata(&shared) {
            Ok(meta) if meta.file_type().is_symlink() => {
                if remove_registered(&repo, &shared)? {
                    removed.push(".codeleveler-target".to_string());
                }
            }
            Ok(_) => {
                cargo_clean(host, &repo, &["--target-dir", ".codeleveler-target"])?;
                removed.push("cargo clean --target-dir .codeleveler-target".to_string());
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(Fail::new(
                    "CLEAN_FAILED",
                    format!(".codeleveler-target: {err}"),
                ));
            }
        }
    }
    let text = if removed.is_empty() {
        "clean: nothing to remove".to_string()
    } else {
        let mut lines: Vec<_> = removed
            .into_iter()
            .map(|item| format!("removed: {item}"))
            .collect();
        lines.push("clean: PASS".to_string());
        lines.join("\n")
    };
    Ok(Report::ok(text))
}

fn default_paths(repo: &Path) -> Result<Vec<PathBuf>, Fail> {
    let mut paths = vec![
        repo.join("dist"),
        repo.join("crates/leveler-execution/leveler-tool-cache"),
    ];
    paths.extend(children_with_prefix(
        &repo.join("crates/leveler-execution"),
        "leveler-ac-",
    )?);
    paths.extend(children_with_prefix(repo, ".shots")?);
    Ok(paths)
}

fn children_with_prefix(dir: &Path, prefix: &str) -> Result<Vec<PathBuf>, Fail> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(Fail::new(
                "CLEAN_FAILED",
                format!("{}: {err}", dir.display()),
            ));
        }
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| Fail::new("CLEAN_FAILED", err.to_string()))?;
        let name = entry.file_name();
        if name.to_string_lossy().starts_with(prefix) {
            out.push(entry.path());
        }
    }
    out.sort();
    Ok(out)
}

fn cargo_clean(host: &dyn Host, repo: &Path, extra: &[&str]) -> Result<(), Fail> {
    let mut args = vec!["clean".to_string()];
    args.extend(extra.iter().map(|part| (*part).to_string()));
    let spec = crate::exec::CommandSpec::new("cargo", args, repo);
    let out = host.run(&spec).map_err(|err| {
        if err.code == "TOOL_MISSING" {
            Fail::new("TOOL_MISSING", "cargo is not on PATH")
        } else {
            err
        }
    })?;
    if out.status != 0 {
        return Err(Fail::new(
            "CLEAN_FAILED",
            format!(
                "cargo clean exited {}\n{}",
                out.status,
                exec::first_lines(&out.stderr, 40)
            ),
        ));
    }
    Ok(())
}

/// Remove a registered path. A symlink is unlinked and not followed.
fn remove_registered(repo: &Path, path: &Path) -> Result<bool, Fail> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => {
            return Err(Fail::new(
                "CLEAN_FAILED",
                format!("{}: {err}", path.display()),
            ));
        }
    };
    let normalized = lexical(path).ok_or_else(|| {
        Fail::new(
            "CLEAN_REFUSED",
            format!("{} escapes the repository", path.display()),
        )
    })?;
    if !normalized.starts_with(repo) || normalized == *repo {
        return Err(Fail::new(
            "CLEAN_REFUSED",
            format!("{} is outside the repository", path.display()),
        ));
    }
    if meta.file_type().is_symlink() {
        std::fs::remove_file(path)
            .map_err(|err| Fail::new("CLEAN_FAILED", format!("{}: {err}", path.display())))?;
        return Ok(true);
    }
    let canon = path
        .canonicalize()
        .map_err(|err| Fail::new("CLEAN_FAILED", format!("{}: {err}", path.display())))?;
    if !canon.starts_with(repo) {
        return Err(Fail::new(
            "CLEAN_REFUSED",
            format!("{} resolves outside the repository", path.display()),
        ));
    }
    let result = if meta.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    result.map_err(|err| Fail::new("CLEAN_FAILED", format!("{}: {err}", path.display())))?;
    Ok(true)
}

fn lexical(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    Some(out)
}

fn display(repo: &Path, path: &Path) -> String {
    path.strip_prefix(repo)
        .map(|rel| rel.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}
