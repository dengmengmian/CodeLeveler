use std::path::{Component, Path, PathBuf};

use crate::error::Fail;
use crate::exec::{self, Host};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Affected {
    Workspace { reason: String },
    Packages(Vec<String>),
}

pub fn owned_rust_files(
    host: &dyn Host,
    repo: &Path,
    explicit: &[String],
    env_owned: Option<&str>,
) -> Result<Vec<PathBuf>, Fail> {
    let repo = canonicalize_repo(repo)?;
    if !explicit.is_empty() {
        return explicit_files(&repo, explicit);
    }
    let mut names = staged_rust(host, &repo)?;
    if let Some(raw) = env_owned {
        names.extend(split_owned(raw));
    }
    names.extend(file_owned(&repo)?);
    let mut out = Vec::new();
    for name in names {
        let path = resolve_inside(&repo, &name)?;
        if !path.is_file() {
            return Err(Fail::new(
                "OWNED_FILE_MISSING",
                format!("owned Rust file does not exist: {name}"),
            ));
        }
        if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
            return Err(Fail::new(
                "FMT_SCOPE_INVALID",
                format!("owned path is not a Rust file: {name}"),
            ));
        }
        if !out.contains(&path) {
            out.push(path);
        }
    }
    Ok(out)
}

pub fn affected(repo: &Path, files: &[PathBuf]) -> Result<Affected, Fail> {
    let repo = canonicalize_repo(repo)?;
    let mut reasons = Vec::new();
    for file in files {
        if let Some(reason) = workspace_reason(&repo, file) {
            if !reasons.contains(&reason) {
                reasons.push(reason);
            }
        }
    }
    if !reasons.is_empty() {
        return Ok(Affected::Workspace {
            reason: reasons.join("; "),
        });
    }
    let mut packages = Vec::new();
    for file in files {
        match package_name_for(&repo, file)? {
            Some(name) => {
                if !packages.contains(&name) {
                    packages.push(name);
                }
            }
            None => {
                return Ok(Affected::Workspace {
                    reason: format!("{} is outside a package", display_rel(&repo, file)),
                });
            }
        }
    }
    packages.sort();
    Ok(Affected::Packages(packages))
}

fn explicit_files(repo: &Path, explicit: &[String]) -> Result<Vec<PathBuf>, Fail> {
    let mut out = Vec::new();
    for raw in explicit {
        if raw.is_empty() {
            return Err(Fail::new("FMT_SCOPE_INVALID", "empty path"));
        }
        let path = resolve_inside(repo, raw)?;
        if path.is_dir() {
            collect_rust(&path, &mut out)?;
            continue;
        }
        if !path.is_file() {
            return Err(Fail::new(
                "OWNED_FILE_MISSING",
                format!("path does not exist: {raw}"),
            ));
        }
        if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
            return Err(Fail::new(
                "FMT_SCOPE_INVALID",
                format!("not a Rust file: {raw}"),
            ));
        }
        if !out.contains(&path) {
            out.push(path);
        }
    }
    if out.is_empty() {
        return Err(Fail::new(
            "FMT_SCOPE_INVALID",
            "explicit path contains no Rust files",
        ));
    }
    Ok(out)
}

fn collect_rust(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), Fail> {
    let entries = std::fs::read_dir(dir)
        .map_err(|err| Fail::new("FMT_SCOPE_INVALID", format!("{}: {err}", dir.display())))?;
    let mut children: Vec<_> = entries
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| Fail::new("FMT_SCOPE_INVALID", err.to_string()))?;
    children.sort_by_key(|entry| entry.file_name());
    for entry in children {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if matches!(
            name.as_ref(),
            "target" | ".git" | "node_modules" | ".codeleveler-target"
        ) {
            continue;
        }
        let path = entry.path();
        let meta = std::fs::symlink_metadata(&path)
            .map_err(|err| Fail::new("FMT_SCOPE_INVALID", format!("{}: {err}", path.display())))?;
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            collect_rust(&path, out)?;
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs")
            && !out.contains(&path)
        {
            out.push(path);
        }
    }
    Ok(())
}

fn staged_rust(host: &dyn Host, repo: &Path) -> Result<Vec<String>, Fail> {
    Ok(staged_names(host, repo)?
        .into_iter()
        .filter(|name| name.ends_with(".rs"))
        .collect())
}

/// Check/test scope: Rust files plus workspace triggers (root Cargo.toml, Cargo.lock, `.cargo/**`).
pub fn owned_scope(
    host: &dyn Host,
    repo: &Path,
    explicit: &[String],
    env_owned: Option<&str>,
) -> Result<Vec<PathBuf>, Fail> {
    let repo = canonicalize_repo(repo)?;
    if !explicit.is_empty() {
        return explicit_scope(&repo, explicit);
    }
    let mut out = Vec::new();
    for name in staged_names(host, &repo)? {
        push_scope(&repo, &name, false, &mut out)?;
    }
    if let Some(raw) = env_owned {
        for name in split_owned(raw) {
            push_scope(&repo, &name, true, &mut out)?;
        }
    }
    for name in file_owned(&repo)? {
        push_scope(&repo, &name, true, &mut out)?;
    }
    Ok(out)
}

fn push_scope(repo: &Path, name: &str, strict: bool, out: &mut Vec<PathBuf>) -> Result<(), Fail> {
    let path = resolve_inside(repo, name)?;
    if !path.exists() {
        return Err(Fail::new(
            "OWNED_FILE_MISSING",
            format!("owned path does not exist: {name}"),
        ));
    }
    if !is_scope_path(repo, &path) {
        if strict {
            return Err(Fail::new(
                "FMT_SCOPE_INVALID",
                format!("owned path is not a Rust file or workspace manifest: {name}"),
            ));
        }
        return Ok(());
    }
    if !out.contains(&path) {
        out.push(path);
    }
    Ok(())
}

fn explicit_scope(repo: &Path, explicit: &[String]) -> Result<Vec<PathBuf>, Fail> {
    let mut out = Vec::new();
    for raw in explicit {
        if raw.is_empty() {
            return Err(Fail::new("FMT_SCOPE_INVALID", "empty path"));
        }
        let path = resolve_inside(repo, raw)?;
        if path.is_dir() {
            if workspace_reason(repo, &path).is_some() {
                out.push(path);
                continue;
            }
            let before = out.len();
            collect_rust(&path, &mut out)?;
            if out.len() == before {
                return Err(Fail::new(
                    "FMT_SCOPE_INVALID",
                    format!("no Rust files under {raw}"),
                ));
            }
            continue;
        }
        if !path.exists() {
            return Err(Fail::new(
                "OWNED_FILE_MISSING",
                format!("path does not exist: {raw}"),
            ));
        }
        if !is_scope_path(repo, &path) {
            return Err(Fail::new(
                "FMT_SCOPE_INVALID",
                format!("not a Rust file or workspace manifest: {raw}"),
            ));
        }
        if !out.contains(&path) {
            out.push(path);
        }
    }
    if out.is_empty() {
        return Err(Fail::new(
            "FMT_SCOPE_INVALID",
            "explicit path contains no Rust files",
        ));
    }
    Ok(out)
}

fn is_scope_path(repo: &Path, path: &Path) -> bool {
    path.extension().and_then(|ext| ext.to_str()) == Some("rs")
        || workspace_reason(repo, path).is_some()
}

fn staged_names(host: &dyn Host, repo: &Path) -> Result<Vec<String>, Fail> {
    let spec = exec::git(
        repo,
        &[
            "diff",
            "--cached",
            "--name-only",
            "--diff-filter=ACMR",
            "-z",
        ],
    );
    let out = host.run(&spec)?;
    if out.status != 0 {
        return Err(Fail::new(
            "GIT_FAILED",
            format!("git diff --cached failed\n{}", out.stderr.trim()),
        ));
    }
    Ok(out
        .stdout
        .split('\0')
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect())
}

fn split_owned(raw: &str) -> Vec<String> {
    raw.split([',', '\n'])
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

fn file_owned(repo: &Path) -> Result<Vec<String>, Fail> {
    let path = repo.join(".dev/owned");
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(str::to_string)
            .collect()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(Fail::new(
            "OWNED_FILE_MISSING",
            format!("could not read {}: {err}", path.display()),
        )),
    }
}

fn workspace_reason(repo: &Path, file: &Path) -> Option<String> {
    let rel = file.strip_prefix(repo).ok()?;
    if rel == Path::new("Cargo.toml") {
        return Some("workspace Cargo.toml is in scope".to_string());
    }
    if rel == Path::new("Cargo.lock") {
        return Some("Cargo.lock is in scope".to_string());
    }
    if rel.components().next() == Some(Component::Normal(".cargo".as_ref())) {
        return Some(".cargo/ is in scope".to_string());
    }
    None
}

fn package_name_for(repo: &Path, file: &Path) -> Result<Option<String>, Fail> {
    let mut dir = if file.is_dir() {
        file.to_path_buf()
    } else {
        file.parent().unwrap_or(repo).to_path_buf()
    };
    loop {
        let manifest = dir.join("Cargo.toml");
        if manifest.is_file() {
            let text = std::fs::read_to_string(&manifest)
                .map_err(|err| Fail::new("CHECK_FAIL", format!("{}: {err}", manifest.display())))?;
            if let Some(name) = package_name(&text) {
                return Ok(Some(name));
            }
            if dir == *repo {
                return Ok(None);
            }
        }
        if dir == *repo {
            return Ok(None);
        }
        dir = match dir.parent() {
            Some(parent) => parent.to_path_buf(),
            None => return Ok(None),
        };
    }
}

fn package_name(text: &str) -> Option<String> {
    let mut in_package = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_package = trimmed == "[package]";
            continue;
        }
        if in_package {
            if let Some(name) = assignment(trimmed, "name") {
                return Some(name);
            }
        }
    }
    None
}

fn assignment(line: &str, key: &str) -> Option<String> {
    let rest = line.trim().strip_prefix(key)?;
    let rest = rest.trim_start().strip_prefix('=')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

pub fn resolve_inside(repo: &Path, raw: &str) -> Result<PathBuf, Fail> {
    let repo = canonicalize_repo(repo)?;
    let input = Path::new(raw);
    let joined = if input.is_absolute() {
        input.to_path_buf()
    } else {
        repo.join(input)
    };
    let normalized = lexical_normalize(&joined)
        .ok_or_else(|| Fail::new("PATH_OUTSIDE_REPO", format!("{raw} escapes the repository")))?;
    if !normalized.starts_with(&repo) {
        return Err(Fail::new(
            "PATH_OUTSIDE_REPO",
            format!("{raw} is outside the repository"),
        ));
    }
    Ok(normalized)
}

fn canonicalize_repo(repo: &Path) -> Result<PathBuf, Fail> {
    repo.canonicalize()
        .map_err(|err| Fail::new("REPO_NOT_FOUND", format!("{}: {err}", repo.display())))
}

fn lexical_normalize(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            Component::Normal(part) => out.push(part),
        }
    }
    Some(out)
}

fn display_rel(repo: &Path, file: &Path) -> String {
    file.strip_prefix(repo)
        .map(|rel| rel.display().to_string())
        .unwrap_or_else(|_| file.display().to_string())
}
