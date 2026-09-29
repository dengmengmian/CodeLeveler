use std::fmt;
use std::path::Path;

use crate::error::Fail;

pub const DEFAULT_MODEL: &str = "deepseek/deepseek-flash";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SemVer {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl fmt::Display for SemVer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Patch,
    Minor,
    Major,
}

pub fn parse_stable(raw: &str) -> Result<SemVer, Fail> {
    let text = raw.trim();
    let text = text.strip_prefix('v').unwrap_or(text);
    if text.is_empty()
        || text.contains('-')
        || text.contains('+')
        || !text.chars().all(|c| c.is_ascii_digit() || c == '.')
    {
        return Err(malformed(raw));
    }
    let parts: Vec<&str> = text.split('.').collect();
    if parts.len() != 3 || parts.iter().any(|part| part.is_empty()) {
        return Err(malformed(raw));
    }
    let parse = |part: &str| part.parse::<u64>().map_err(|_| malformed(raw));
    Ok(SemVer {
        major: parse(parts[0])?,
        minor: parse(parts[1])?,
        patch: parse(parts[2])?,
    })
}

fn malformed(raw: &str) -> Fail {
    Fail::new(
        "MALFORMED_VERSION",
        format!("expected MAJOR.MINOR.PATCH, got {raw}"),
    )
}

pub fn bump(version: SemVer, level: Level) -> Result<SemVer, Fail> {
    let next = match level {
        Level::Patch => version
            .patch
            .checked_add(1)
            .map(|patch| SemVer { patch, ..version }),
        Level::Minor => version.minor.checked_add(1).map(|minor| SemVer {
            minor,
            patch: 0,
            ..version
        }),
        Level::Major => version.major.checked_add(1).map(|major| SemVer {
            major,
            minor: 0,
            patch: 0,
        }),
    };
    next.ok_or_else(|| Fail::new("MALFORMED_VERSION", "version overflow"))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalVersions {
    pub workspace: SemVer,
    pub pins: Vec<(String, SemVer)>,
    pub release_en: SemVer,
    pub release_zh: SemVer,
}

pub fn read_local(repo: &Path) -> Result<LocalVersions, Fail> {
    let manifest = read_repo_file(repo, "Cargo.toml")?;
    let workspace = workspace_version(&manifest)?;
    let pins = path_pins(&manifest)?;
    let release_en = heading_version(&read_repo_file(repo, "docs/RELEASE.md")?, "docs/RELEASE.md")?;
    let release_zh = heading_version(
        &read_repo_file(repo, "docs/RELEASE.zh-CN.md")?,
        "docs/RELEASE.zh-CN.md",
    )?;
    Ok(LocalVersions {
        workspace,
        pins,
        release_en,
        release_zh,
    })
}

pub fn require_local(local: &LocalVersions) -> Result<SemVer, Fail> {
    let mut rows = vec![
        ("docs/RELEASE.md".to_string(), local.release_en),
        ("docs/RELEASE.zh-CN.md".to_string(), local.release_zh),
    ];
    for (name, version) in &local.pins {
        rows.push((format!("pin {name}"), *version));
    }
    disagree(local.workspace, &rows)
}

pub fn require_publish(local: &LocalVersions, tags: &str, releases: &str) -> Result<SemVer, Fail> {
    require_local(local)?;
    let highest_tag = highest_stable(tags).ok_or_else(|| {
        Fail::new(
            "NO_STABLE_TAG",
            "git tag --list has no stable vMAJOR.MINOR.PATCH tag",
        )
    })?;
    let highest_release = highest_stable(releases).ok_or_else(|| {
        Fail::new(
            "NO_STABLE_TAG",
            "gh release list returned no stable release",
        )
    })?;
    let rows = vec![
        ("highest stable git tag".to_string(), highest_tag),
        ("highest stable GitHub release".to_string(), highest_release),
    ];
    disagree(local.workspace, &rows)
}

fn disagree(workspace: SemVer, rows: &[(String, SemVer)]) -> Result<SemVer, Fail> {
    let mismatches: Vec<_> = rows
        .iter()
        .filter(|(_, version)| *version != workspace)
        .map(|(name, version)| format!("{name} = {version}"))
        .collect();
    if mismatches.is_empty() {
        return Ok(workspace);
    }
    Err(Fail::new(
        "VERSION_MISMATCH",
        format!(
            "workspace.package.version = {workspace}\n{}",
            mismatches.join("\n")
        ),
    ))
}

pub fn render_bumped_manifest(manifest: &str, next: SemVer) -> Result<String, Fail> {
    let current = workspace_version(manifest)?;
    let old = current.to_string();
    let new = next.to_string();
    let mut section = String::new();
    let mut replaced_workspace = false;
    let mut out = String::new();
    for (index, line) in manifest.lines().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            section = trimmed.to_string();
        }
        let in_workspace = section == "[workspace.package]";
        let is_pin = line.contains("path =") && line.contains("version =");
        if (in_workspace || is_pin) && line.contains(&format!("version = \"{old}\"")) {
            if in_workspace && !is_pin {
                replaced_workspace = true;
            }
            out.push_str(&line.replace(
                &format!("version = \"{old}\""),
                &format!("version = \"{new}\""),
            ));
        } else {
            out.push_str(line);
        }
    }
    if manifest.ends_with('\n') {
        out.push('\n');
    }
    if !replaced_workspace {
        return Err(Fail::new(
            "VERSION_SOURCE_MISSING",
            "Cargo.toml has no [workspace.package] version to bump",
        ));
    }
    Ok(out)
}

pub fn bump_heading(text: &str, next: SemVer, label: &str) -> Result<String, Fail> {
    let mut replaced = false;
    let mut out = String::new();
    for (index, line) in text.lines().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        if !replaced && line.starts_with("# CodeLeveler ") {
            let current = line.trim_start_matches("# CodeLeveler ").trim();
            parse_stable(current).map_err(|err| {
                Fail::new(
                    "MALFORMED_VERSION",
                    format!("{label} heading: {}", err.detail),
                )
            })?;
            out.push_str("# CodeLeveler ");
            out.push_str(&next.to_string());
            replaced = true;
        } else {
            out.push_str(line);
        }
    }
    if text.ends_with('\n') {
        out.push('\n');
    }
    if !replaced {
        return Err(Fail::new(
            "VERSION_SOURCE_MISSING",
            format!("{label} has no `# CodeLeveler X.Y.Z` heading"),
        ));
    }
    Ok(out)
}

pub fn github_slug(manifest: &str) -> Result<String, Fail> {
    let mut section = String::new();
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            section = trimmed.to_string();
            continue;
        }
        if section != "[workspace.package]" {
            continue;
        }
        let Some(value) = assignment(trimmed, "repository") else {
            continue;
        };
        return slug_from_url(&value);
    }
    Err(Fail::new(
        "VERSION_SOURCE_MISSING",
        "Cargo.toml has no workspace.package.repository",
    ))
}

fn slug_from_url(url: &str) -> Result<String, Fail> {
    let url = url.trim().trim_end_matches(".git");
    let rest = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("git@github.com:"))
        .ok_or_else(|| {
            Fail::new(
                "GITHUB_RELEASE_UNKNOWN",
                format!("repository is not a GitHub URL: {url}"),
            )
        })?;
    let mut parts = rest.split('/');
    match (parts.next(), parts.next()) {
        (Some(owner), Some(name))
            if !owner.is_empty() && !name.is_empty() && parts.next().is_none() =>
        {
            Ok(format!("{owner}/{name}"))
        }
        _ => Err(Fail::new(
            "GITHUB_RELEASE_UNKNOWN",
            format!("repository URL has no owner/name: {url}"),
        )),
    }
}

pub fn parse_tag_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(|line| {
            let line = line.trim();
            let field = line.split('\t').next().unwrap_or(line);
            field.split_whitespace().next().unwrap_or("").trim()
        })
        .filter(|tag| !tag.is_empty())
        .map(|tag| tag.trim_end_matches("^{}").to_string())
        .collect()
}

pub fn parse_stable_tags(text: &str) -> Vec<SemVer> {
    parse_tag_lines(text)
        .iter()
        .filter_map(|tag| parse_stable(tag).ok())
        .collect()
}

pub fn highest_stable(text: &str) -> Option<SemVer> {
    parse_stable_tags(text).into_iter().max()
}

pub fn tag_listed(text: &str, tag: &str) -> bool {
    let needle = format!("refs/tags/{tag}");
    parse_tag_lines(text).iter().any(|line| line == tag)
        || text.lines().any(|line| line.contains(&needle))
}

fn workspace_version(manifest: &str) -> Result<SemVer, Fail> {
    let mut section = String::new();
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            section = trimmed.to_string();
            continue;
        }
        if section == "[workspace.package]" {
            if let Some(value) = assignment(trimmed, "version") {
                return parse_stable(&value);
            }
        }
    }
    Err(Fail::new(
        "VERSION_SOURCE_MISSING",
        "Cargo.toml has no [workspace.package] version",
    ))
}

fn path_pins(manifest: &str) -> Result<Vec<(String, SemVer)>, Fail> {
    let mut pins = Vec::new();
    for line in manifest.lines() {
        if !line.contains("path =") || !line.contains("version =") {
            continue;
        }
        let name = line
            .split('=')
            .next()
            .unwrap_or("")
            .trim()
            .trim_matches('"')
            .to_string();
        let Some(version) = inline_version(line) else {
            return Err(Fail::new(
                "VERSION_SOURCE_MISSING",
                format!("path dependency has no version: {line}"),
            ));
        };
        pins.push((name, parse_stable(&version)?));
    }
    Ok(pins)
}

fn inline_version(line: &str) -> Option<String> {
    let marker = "version = \"";
    let start = line.find(marker)? + marker.len();
    let rest = &line[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn assignment(line: &str, key: &str) -> Option<String> {
    let rest = line.trim().strip_prefix(key)?;
    let rest = rest.trim_start().strip_prefix('=')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn heading_version(text: &str, label: &str) -> Result<SemVer, Fail> {
    for line in text.lines() {
        if let Some(rest) = line.trim().strip_prefix("# CodeLeveler ") {
            return parse_stable(rest.trim()).map_err(|err| {
                Fail::new(
                    "MALFORMED_VERSION",
                    format!("{label} heading: {}", err.detail),
                )
            });
        }
    }
    Err(Fail::new(
        "VERSION_SOURCE_MISSING",
        format!("{label} has no `# CodeLeveler X.Y.Z` heading"),
    ))
}

fn read_repo_file(repo: &Path, rel: &str) -> Result<String, Fail> {
    let path = repo.join(rel);
    std::fs::read_to_string(&path).map_err(|err| {
        Fail::new(
            "VERSION_SOURCE_MISSING",
            format!("could not read {}: {err}", path.display()),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bump_levels() {
        let current = parse_stable("1.0.9").unwrap();
        assert_eq!(bump(current, Level::Patch).unwrap().to_string(), "1.0.10");
        assert_eq!(bump(current, Level::Minor).unwrap().to_string(), "1.1.0");
        assert_eq!(bump(current, Level::Major).unwrap().to_string(), "2.0.0");
    }

    #[test]
    fn malformed_and_prerelease_rejected() {
        assert_eq!(parse_stable("1.0").unwrap_err().code, "MALFORMED_VERSION");
        assert_eq!(
            parse_stable("1.0.0-rc.1").unwrap_err().code,
            "MALFORMED_VERSION"
        );
        assert_eq!(parse_stable("v1.2.3").unwrap().to_string(), "1.2.3");
    }

    #[test]
    fn manifest_bump_changes_workspace_and_path_pins_only() {
        let manifest = "\
[workspace.package]
version = \"1.0.8\"

[workspace.dependencies]
leveler-core = { version = \"1.0.8\", path = \"crates/leveler-core\" }
tokio = { version = \"1.0.8\" }
";
        let next = render_bumped_manifest(manifest, parse_stable("1.0.9").unwrap()).unwrap();
        assert!(next.contains("[workspace.package]\nversion = \"1.0.9\""));
        assert!(
            next.contains("leveler-core = { version = \"1.0.9\", path = \"crates/leveler-core\" }")
        );
        assert!(next.contains("tokio = { version = \"1.0.8\" }"));
    }

    #[test]
    fn heading_bump_keeps_the_body() {
        let text = "# CodeLeveler 1.0.8\n\nStill mentions 1.0.8 in the notes.\n";
        let next = bump_heading(text, parse_stable("1.0.9").unwrap(), "docs/RELEASE.md").unwrap();
        assert!(next.starts_with("# CodeLeveler 1.0.9\n"));
        assert!(next.contains("Still mentions 1.0.8"));
    }

    #[test]
    fn highest_stable_ignores_prerelease() {
        let tags = "v0.1.0-beta.1\nv1.0.0\nv1.0.8\n";
        assert_eq!(highest_stable(tags).unwrap().to_string(), "1.0.8");
    }
}
