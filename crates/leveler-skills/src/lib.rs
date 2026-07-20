//! Agent Skills: discover, load, and author `SKILL.md` procedural-knowledge
//! packs. Only each skill's `name` + `description` are injected into context
//! by default; the full body is loaded on demand (progressive disclosure) or
//! turn-injected when the user names `$skill` / selects `/skill`.
#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Errors from authoring a skill.
#[derive(Debug, thiserror::Error)]
pub enum SkillError {
    #[error("invalid skill name `{0}` (use letters, digits, '-' or '_')")]
    InvalidName(String),
    #[error("skill `{0}` already exists")]
    Exists(String),
    #[error("io error: {0}")]
    Io(String),
}

/// Where a skill came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillSource {
    /// `<repo>/.leveler/skills/<name>/`
    Project,
    /// `~/.leveler/skills/<name>/`
    Global,
}

impl SkillSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Global => "global",
        }
    }
}

/// The lightweight index entry loaded into context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillSummary {
    pub name: String,
    pub description: String,
    pub source: SkillSource,
}

/// A skill's full content, loaded on demand or turn-injected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDetail {
    pub name: String,
    pub description: String,
    /// The Markdown body after the frontmatter.
    pub body: String,
    pub source: SkillSource,
    /// Absolute skill package directory (contains `SKILL.md`).
    pub dir: PathBuf,
    /// Paths relative to [`Self::dir`] under `scripts/` (and nested).
    pub scripts: Vec<String>,
    /// Paths relative to [`Self::dir`] under `references/` (and nested).
    pub references: Vec<String>,
    /// Other bundled files (not SKILL.md, not under scripts/ or references/).
    pub other_files: Vec<String>,
}

/// Result of resolving `$name` mentions in a user message.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SkillMentionResolution {
    /// Skills successfully loaded for this turn (order of first mention).
    pub loaded: Vec<SkillDetail>,
    /// `$name` tokens that did not match any discovered skill.
    pub unknown: Vec<String>,
}

impl SkillMentionResolution {
    pub fn is_empty(&self) -> bool {
        self.loaded.is_empty() && self.unknown.is_empty()
    }
}

#[derive(Debug, Default, Deserialize)]
struct Frontmatter {
    name: Option<String>,
    description: Option<String>,
}

/// The project skills directory for a repo root.
pub fn project_skills_dir(root: &Path) -> PathBuf {
    root.join(".leveler").join("skills")
}

/// The user-global skills directory (`<leveler-home>/skills`), if a home is
/// known. Shares the home-resolution order via [`leveler_core::leveler_home_dir`].
pub fn global_skills_dir() -> Option<PathBuf> {
    leveler_core::leveler_home_dir(leveler_core::environment()).map(|home| home.join("skills"))
}

/// Discover skills from the project and global directories. On a name clash the
/// project skill wins (shadowing global).
pub fn discover(root: &Path) -> Vec<SkillSummary> {
    let mut out: Vec<SkillSummary> = Vec::new();
    let mut seen: Vec<String> = Vec::new();

    // Project first so it wins clashes.
    let mut sources = vec![(project_skills_dir(root), SkillSource::Project)];
    if let Some(global) = global_skills_dir() {
        sources.push((global, SkillSource::Global));
    }
    for (dir, source) in sources {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut found: Vec<SkillSummary> = entries
            .flatten()
            .filter_map(|e| summary_at(&e.path(), source))
            .collect();
        found.sort_by(|a, b| a.name.cmp(&b.name));
        for s in found {
            if seen.contains(&s.name) {
                continue;
            }
            seen.push(s.name.clone());
            out.push(s);
        }
    }
    out
}

fn summary_at(dir: &Path, source: SkillSource) -> Option<SkillSummary> {
    if !dir.is_dir() {
        return None;
    }
    let content = std::fs::read_to_string(dir.join("SKILL.md")).ok()?;
    let (fm, _) = split_frontmatter(&content);
    let name = fm
        .name
        .filter(|n| !n.is_empty())
        .or_else(|| dir.file_name().map(|n| n.to_string_lossy().into_owned()))?;
    Some(SkillSummary {
        name,
        description: fm.description.unwrap_or_default(),
        source,
    })
}

/// Load a skill's full body and structured bundled files by name.
pub fn load(root: &Path, name: &str) -> Option<SkillDetail> {
    let mut candidates = vec![(project_skills_dir(root).join(name), SkillSource::Project)];
    if let Some(global) = global_skills_dir() {
        candidates.push((global.join(name), SkillSource::Global));
    }
    for (dir, source) in candidates {
        let Ok(content) = std::fs::read_to_string(dir.join("SKILL.md")) else {
            continue;
        };
        let (fm, body) = split_frontmatter(&content);
        let (scripts, references, other_files) = classify_bundled_files(&dir);
        return Some(SkillDetail {
            name: fm
                .name
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| name.to_string()),
            description: fm.description.unwrap_or_default(),
            body,
            source,
            dir,
            scripts,
            references,
            other_files,
        });
    }
    None
}

/// Walk a skill package directory and classify relative paths.
fn classify_bundled_files(dir: &Path) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut scripts = Vec::new();
    let mut references = Vec::new();
    let mut other_files = Vec::new();
    walk_bundled(dir, dir, &mut scripts, &mut references, &mut other_files);
    scripts.sort();
    references.sort();
    other_files.sort();
    (scripts, references, other_files)
}

fn walk_bundled(
    root: &Path,
    current: &Path,
    scripts: &mut Vec<String>,
    references: &mut Vec<String>,
    other_files: &mut Vec<String>,
) {
    let Ok(entries) = std::fs::read_dir(current) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == "SKILL.md" {
            continue;
        }
        if path.is_dir() {
            walk_bundled(root, &path, scripts, references, other_files);
            continue;
        }
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let rel_s = rel.to_string_lossy().replace('\\', "/");
        if rel_s.starts_with("scripts/") || rel_s == "scripts" {
            scripts.push(rel_s);
        } else if rel_s.starts_with("references/") || rel_s == "references" {
            references.push(rel_s);
        } else {
            other_files.push(rel_s);
        }
    }
}

/// Extract `$skill-name` tokens from user text.
///
/// Names use letters, digits, `-`, and `_`. Boundaries: `$` start; end at the
/// first non-name character. Does not match `$$` or bare `$`.
pub fn parse_skill_mentions(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'$' {
            i += 1;
            continue;
        }
        // Skip `$$` escapes / noise.
        if i + 1 < bytes.len() && bytes[i + 1] == b'$' {
            i += 2;
            continue;
        }
        let start = i + 1;
        let mut end = start;
        while end < bytes.len() {
            let c = bytes[end] as char;
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                end += 1;
            } else {
                break;
            }
        }
        if end > start {
            let name = text[start..end].to_string();
            if !out.contains(&name) {
                out.push(name);
            }
            i = end;
        } else {
            i += 1;
        }
    }
    out
}

/// Resolve `$name` mentions against discovered skills under `root`.
pub fn resolve_mentions(root: &Path, text: &str) -> SkillMentionResolution {
    let mut loaded = Vec::new();
    let mut unknown = Vec::new();
    let mut seen = Vec::new();
    for name in parse_skill_mentions(text) {
        if seen.contains(&name) {
            continue;
        }
        seen.push(name.clone());
        match load(root, &name) {
            Some(detail) => loaded.push(detail),
            None => unknown.push(name),
        }
    }
    SkillMentionResolution { loaded, unknown }
}

/// Format a full skill package for the model (`load_skill` or turn inject).
pub fn render_skill_package(detail: &SkillDetail) -> String {
    let mut s = format!(
        "# Skill: {}\n{}\n\nsource: {}\ndir: {}\n\n",
        detail.name,
        detail.description,
        detail.source.as_str(),
        detail.dir.display()
    );
    s.push_str("## Instructions\n\n");
    s.push_str(detail.body.trim_end());
    s.push('\n');

    if !detail.scripts.is_empty() {
        s.push_str(
            "\n## Scripts\nPrefer `shell_command` / `run_command` on these paths \
             (resolve relative to `dir` above). Do not retype large script bodies.\n",
        );
        for p in &detail.scripts {
            let abs = detail.dir.join(p);
            s.push_str(&format!("- `{p}` → `{}`\n", abs.display()));
        }
    }
    if !detail.references.is_empty() {
        s.push_str("\n## References\nRead with `read_file` using the absolute path:\n");
        for p in &detail.references {
            let abs = detail.dir.join(p);
            s.push_str(&format!("- `{p}` → `{}`\n", abs.display()));
        }
    }
    if !detail.other_files.is_empty() {
        s.push_str("\n## Other bundled files\n");
        for p in &detail.other_files {
            let abs = detail.dir.join(p);
            s.push_str(&format!("- `{p}` → `{}`\n", abs.display()));
        }
    }
    s
}

/// Build the system-side turn injection for resolved mentions.
///
/// Returns `None` when there are no mentions at all.
pub fn render_turn_injection(resolution: &SkillMentionResolution) -> Option<String> {
    if resolution.is_empty() {
        return None;
    }
    let mut s = String::from(
        "SKILL TURN INJECTION — the user named skill(s) for this turn. \
         Follow each loaded skill's instructions completely before other task \
         actions. Do not skip a named skill; do not re-delegate reading these \
         instructions to a sub-agent.\n",
    );
    for detail in &resolution.loaded {
        s.push('\n');
        s.push_str(&render_skill_package(detail));
        s.push('\n');
    }
    if !resolution.unknown.is_empty() {
        s.push_str(
            "\nUnknown skill mentions (not in project or global index; continue without them):\n",
        );
        for name in &resolution.unknown {
            s.push_str(&format!("- `${name}`\n"));
        }
    }
    Some(s)
}

/// Progressive-disclosure usage rules appended to the skill index.
pub const SKILLS_HOW_TO_USE: &str = "\
How to use skills (progressive disclosure):\n\
- Discovery: the list above is name + description only. Full instructions live \
  in each skill's `SKILL.md` and are loaded with `load_skill`, or injected \
  automatically when the user names `$skill-name` or selects `/skill`.\n\
- Trigger: if the user names a skill (`$name` or `/skill name`) OR the task \
  clearly matches a listed description, you MUST use that skill for the turn — \
  read its full instructions before other task actions. Multiple mentions mean \
  use them all. Do not carry skills across turns unless re-mentioned.\n\
- Missing: if a named skill is not in the list, say so briefly and continue.\n\
- Paths: resolve `scripts/…` and `references/…` relative to the skill `dir` \
  returned by load/injection. Prefer running provided scripts over retyping \
  large code blocks. Read required reference files yourself; do not delegate \
  reading skill instructions to a sub-agent.\n\
- Hygiene: load only what the task needs; do not open every bundled file by default.\n";

/// Render the skills index for injection into context (empty if none).
pub fn render_index(skills: &[SkillSummary]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut s = String::from(
        "Available skills — reusable procedures for specific tasks. Call \
         `load_skill` with a name to read full instructions before related work, \
         unless the user already named `$skill` / `/skill` (then follow the \
         turn injection):\n",
    );
    for skill in skills {
        s.push_str(&format!(
            "- {} [{}]: {}\n",
            skill.name,
            skill.source.as_str(),
            skill.description
        ));
    }
    s.push('\n');
    s.push_str(SKILLS_HOW_TO_USE);
    s
}

/// Create a new project skill, writing `.leveler/skills/<name>/SKILL.md`.
pub fn create(
    root: &Path,
    name: &str,
    description: &str,
    body: &str,
) -> Result<PathBuf, SkillError> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(SkillError::InvalidName(name.to_string()));
    }
    let dir = project_skills_dir(root).join(name);
    let file = dir.join("SKILL.md");
    if file.exists() {
        return Err(SkillError::Exists(name.to_string()));
    }
    std::fs::create_dir_all(&dir).map_err(|e| SkillError::Io(e.to_string()))?;
    let content = format!(
        "---\nname: {name}\ndescription: {}\n---\n\n{}\n",
        description.replace('\n', " ").trim(),
        body.trim_end(),
    );
    std::fs::write(&file, content).map_err(|e| SkillError::Io(e.to_string()))?;
    Ok(dir)
}

/// Split leading `---`-delimited YAML frontmatter from the Markdown body.
fn split_frontmatter(content: &str) -> (Frontmatter, String) {
    let normalized = content.replace("\r\n", "\n");
    if let Some(rest) = normalized.strip_prefix("---\n")
        && let Some(end) = rest.find("\n---")
    {
        let yaml = &rest[..end];
        let after = &rest[end + 4..]; // skip "\n---"
        let body = after.trim_start_matches(['\n', ' ']).to_string();
        let fm = serde_yaml::from_str::<Frontmatter>(yaml).unwrap_or_default();
        return (fm, body);
    }
    (Frontmatter::default(), normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!(
            "leveler-skills-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write_skill(root: &Path, name: &str, fm_name: &str, desc: &str, body: &str) {
        let dir = project_skills_dir(root).join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!(
                "---\nname: {fm_name}\ndescription: {desc}\nmetadata:\n  x: y\n---\n\n{body}\n"
            ),
        )
        .unwrap();
    }

    #[test]
    fn discovers_and_loads_a_skill() {
        let root = tmp("disc");
        write_skill(
            &root,
            "deploy",
            "deploy",
            "Deploy the service safely.",
            "# Deploy\n\nRun make deploy.",
        );
        std::fs::write(
            project_skills_dir(&root)
                .join("deploy")
                .join("checklist.md"),
            "1. x",
        )
        .unwrap();

        let index = discover(&root);
        assert_eq!(index.len(), 1);
        assert_eq!(index[0].name, "deploy");
        assert_eq!(index[0].description, "Deploy the service safely.");
        assert_eq!(index[0].source, SkillSource::Project);

        let detail = load(&root, "deploy").unwrap();
        assert!(detail.body.contains("Run make deploy."));
        assert!(
            !detail.body.contains("---"),
            "frontmatter stripped: {:?}",
            detail.body
        );
        assert_eq!(detail.other_files, vec!["checklist.md".to_string()]);
        assert!(detail.scripts.is_empty());
        assert!(detail.references.is_empty());
    }

    #[test]
    fn create_then_discover_roundtrip() {
        let root = tmp("create");
        let dir = create(
            &root,
            "run-migrations",
            "Apply DB migrations.",
            "# Migrations\n\nUse sqlx.",
        )
        .unwrap();
        assert!(dir.join("SKILL.md").is_file());

        let index = discover(&root);
        assert_eq!(index.len(), 1);
        assert_eq!(index[0].description, "Apply DB migrations.");
        let detail = load(&root, "run-migrations").unwrap();
        assert!(detail.body.contains("Use sqlx."));

        // Duplicate + invalid name are rejected.
        assert!(matches!(
            create(&root, "run-migrations", "x", "y"),
            Err(SkillError::Exists(_))
        ));
        assert!(matches!(
            create(&root, "bad name!", "x", "y"),
            Err(SkillError::InvalidName(_))
        ));
    }

    #[test]
    fn render_index_is_empty_without_skills() {
        assert_eq!(render_index(&[]), "");
    }

    #[test]
    fn parse_skill_mentions_extracts_dollar_names() {
        assert_eq!(
            parse_skill_mentions("use $demo and $other-skill please"),
            vec!["demo".to_string(), "other-skill".to_string()]
        );
        assert_eq!(
            parse_skill_mentions("no mentions here"),
            Vec::<String>::new()
        );
        assert_eq!(parse_skill_mentions("$alone"), vec!["alone".to_string()]);
        // Boundary: longer prefix does not count as shorter name.
        assert_eq!(
            parse_skill_mentions("$notion-research-docs"),
            vec!["notion-research-docs".to_string()]
        );
        assert!(!parse_skill_mentions("$notion-research-docs").contains(&"notion".to_string()));
    }

    #[test]
    fn resolve_mentions_loads_known_and_lists_unknown() {
        let root = tmp("mention");
        write_skill(
            &root,
            "demo",
            "demo",
            "Demo skill.",
            "UNIQUE_BODY_TOKEN_XYZ_42",
        );
        let res = resolve_mentions(&root, "please run $demo and $no_such_skill");
        assert_eq!(res.loaded.len(), 1);
        assert_eq!(res.loaded[0].name, "demo");
        assert!(res.loaded[0].body.contains("UNIQUE_BODY_TOKEN_XYZ_42"));
        assert_eq!(res.unknown, vec!["no_such_skill".to_string()]);

        let inj = render_turn_injection(&res).expect("injection");
        assert!(inj.contains("UNIQUE_BODY_TOKEN_XYZ_42"));
        assert!(inj.contains("`$no_such_skill`") || inj.contains("$no_such_skill"));
        assert!(inj.contains("SKILL TURN INJECTION"));
    }

    #[test]
    fn unknown_only_injection_has_no_fake_body() {
        let root = tmp("unknown");
        let res = resolve_mentions(&root, "try $ghost_skill");
        assert!(res.loaded.is_empty());
        assert_eq!(res.unknown, vec!["ghost_skill".to_string()]);
        let inj = render_turn_injection(&res).unwrap();
        assert!(!inj.contains("## Instructions\n\n\n# "));
        assert!(inj.contains("Unknown skill"));
    }

    #[test]
    fn structured_scripts_and_references_in_package() {
        let root = tmp("struct");
        write_skill(&root, "pack", "pack", "Pack skill.", "Do the pack thing.");
        let dir = project_skills_dir(&root).join("pack");
        std::fs::create_dir_all(dir.join("scripts")).unwrap();
        std::fs::create_dir_all(dir.join("references")).unwrap();
        std::fs::write(dir.join("scripts/foo.sh"), "#!/bin/sh\necho hi\n").unwrap();
        std::fs::write(dir.join("references/note.md"), "note").unwrap();
        std::fs::write(dir.join("extra.txt"), "x").unwrap();

        let detail = load(&root, "pack").unwrap();
        assert_eq!(detail.scripts, vec!["scripts/foo.sh".to_string()]);
        assert_eq!(detail.references, vec!["references/note.md".to_string()]);
        assert_eq!(detail.other_files, vec!["extra.txt".to_string()]);

        let rendered = render_skill_package(&detail);
        assert!(rendered.contains("Do the pack thing."));
        assert!(rendered.contains("scripts/foo.sh"));
        assert!(rendered.contains("references/note.md"));
        assert!(rendered.contains(detail.dir.to_string_lossy().as_ref()));
        // Absolute path under skill dir (not a hard-coded project-only prefix).
        assert!(rendered.contains(&format!("{}", detail.dir.join("scripts/foo.sh").display())));
    }

    #[test]
    fn render_index_includes_how_to_use_rules() {
        let skills = vec![SkillSummary {
            name: "x".into(),
            description: "does x".into(),
            source: SkillSource::Project,
        }];
        let idx = render_index(&skills);
        assert!(idx.contains("progressive disclosure") || idx.contains("How to use skills"));
        assert!(idx.contains("$"));
        assert!(idx.contains("scripts/"));
        assert!(idx.contains("sub-agent") || idx.contains("subagent"));
        assert!(idx.contains("load_skill"));
    }
}
