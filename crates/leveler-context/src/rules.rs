//! Project rule loading and merging (spec §39): AGENTS.md, .leveler/instructions.md,
//! and .leveler/rules/*.md. Each instruction records its source.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

const MAX_RULE_BYTES: usize = 8000;

/// A single project instruction with provenance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectInstruction {
    /// Workspace-relative source path.
    pub source: String,
    /// The instruction text (possibly truncated).
    pub content: String,
}

/// Load and merge project rules from `root`, in priority order (root AGENTS.md
/// first, then explicit instructions, then rule files).
pub fn load_rules(root: &Path) -> Vec<ProjectInstruction> {
    load_rules_for_paths(root, &[])
}

/// Load project rules plus any nested `AGENTS.md` files that scope over the
/// provided workspace-relative paths. Root rules are loaded first; deeper
/// `AGENTS.md` files are appended later so their instructions can override.
pub fn load_rules_for_paths(root: &Path, paths: &[String]) -> Vec<ProjectInstruction> {
    let mut out = Vec::new();

    for candidate in ["AGENTS.md", ".leveler/instructions.md"] {
        push_if_present(root, candidate, &mut out);
    }

    let mut seen: Vec<String> = out.iter().map(|i| i.source.clone()).collect();
    for path in paths {
        for agents in scoped_agents_paths(path) {
            if seen.contains(&agents) {
                continue;
            }
            push_if_present(root, &agents, &mut out);
            seen.push(agents);
        }
    }

    // .leveler/rules/*.md, sorted for determinism.
    let rules_dir = root.join(".leveler/rules");
    if let Ok(entries) = std::fs::read_dir(&rules_dir) {
        let mut paths: Vec<_> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("md"))
            .collect();
        paths.sort();
        for path in paths {
            if let Some(rel) = path.strip_prefix(root).ok().and_then(|p| p.to_str()) {
                let rel = rel.to_string();
                push_if_present(root, &rel, &mut out);
            }
        }
    }

    out
}

/// Load *only* the nested `AGENTS.md` files scoping over `paths`, excluding the
/// root rules that [`load_rules`] already returns.
///
/// Split out from [`load_rules_for_paths`] because the two have different
/// lifetimes in a turn: root rules are constant and belong in the system prompt,
/// while these appear as the agent touches new directories. Folding a growing
/// set into the system prompt would rewrite the transcript's first message every
/// round and miss the provider's prefix cache on every request.
///
/// `exclude` holds sources already injected; they are skipped. Returns rules in
/// shallowest-first order, so a deeper `AGENTS.md` still overrides.
pub fn load_scoped_rules(
    root: &Path,
    paths: &[String],
    exclude: &[String],
) -> Vec<ProjectInstruction> {
    let mut out: Vec<ProjectInstruction> = Vec::new();
    // Root sources are owned by the system prompt; never re-emit them here.
    let mut seen: Vec<String> = vec!["AGENTS.md".to_string()];
    seen.extend(exclude.iter().cloned());

    for path in paths {
        for agents in scoped_agents_paths(path) {
            if seen.contains(&agents) {
                continue;
            }
            push_if_present(root, &agents, &mut out);
            seen.push(agents);
        }
    }
    out
}

fn scoped_agents_paths(path: &str) -> Vec<String> {
    let path = Path::new(path);
    let mut dirs = Vec::new();
    let mut current = PathBuf::new();

    for component in path.components() {
        match component {
            Component::Normal(part) => current.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return Vec::new(),
        }
    }

    let mut dir = current.parent();
    let mut ancestors = Vec::new();
    while let Some(d) = dir {
        if d.as_os_str().is_empty() {
            break;
        }
        ancestors.push(d.to_path_buf());
        dir = d.parent();
    }
    ancestors.reverse();

    for ancestor in ancestors {
        dirs.push(format!("{}/AGENTS.md", ancestor.display()));
    }
    dirs
}

fn push_if_present(root: &Path, rel: &str, out: &mut Vec<ProjectInstruction>) {
    let path = root.join(rel);
    if let Ok(content) = std::fs::read_to_string(&path) {
        let trimmed = content.trim();
        if trimmed.is_empty() {
            return;
        }
        out.push(ProjectInstruction {
            source: rel.to_string(),
            content: truncate(trimmed),
        });
    }
}

fn truncate(s: &str) -> String {
    if s.len() <= MAX_RULE_BYTES {
        return s.to_string();
    }
    let mut end = MAX_RULE_BYTES;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… [rule truncated]", &s[..end])
}

/// Render instructions into a prompt block.
pub fn render_instructions(instructions: &[ProjectInstruction]) -> String {
    let mut s = String::new();
    for instr in instructions {
        s.push_str(&format!(
            "--- from {} ---\n{}\n\n",
            instr.source, instr.content
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_agents_md_and_rules() {
        let dir =
            std::env::temp_dir().join(format!("leveler-rules-{}", std::process::id() as u64 + 51));
        std::fs::create_dir_all(dir.join(".leveler/rules")).unwrap();
        std::fs::write(dir.join("AGENTS.md"), "Use tabs.").unwrap();
        std::fs::write(dir.join(".leveler/rules/style.md"), "No unwrap.").unwrap();
        let rules = load_rules(&dir);
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].source, "AGENTS.md");
        assert!(rules.iter().any(|r| r.content.contains("No unwrap")));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn loads_scoped_agents_for_candidate_paths() {
        let dir = std::env::temp_dir().join(format!(
            "leveler-scoped-rules-{}",
            std::process::id() as u64 + 53
        ));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("AGENTS.md"), "Root rule.").unwrap();
        std::fs::write(dir.join("src/AGENTS.md"), "Src rule.").unwrap();

        let rules = load_rules_for_paths(&dir, &["src/lib.rs".to_string()]);

        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].source, "AGENTS.md");
        assert_eq!(rules[1].source, "src/AGENTS.md");
        assert!(rules[1].content.contains("Src rule"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn empty_when_no_rules() {
        let dir = std::env::temp_dir().join(format!(
            "leveler-norules-{}",
            std::process::id() as u64 + 52
        ));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(load_rules(&dir).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn truncate_keeps_short_content_intact() {
        let content = "short";
        let out = truncate(content);
        assert_eq!(out, "short");
    }

    #[test]
    fn truncate_adds_marker_when_clipping() {
        let content = "x".repeat(MAX_RULE_BYTES + 100);
        let out = truncate(&content);
        assert!(out.ends_with("\n… [rule truncated]"));
        assert!(out.len() <= MAX_RULE_BYTES + "\n… [rule truncated]".len());
    }

    #[test]
    fn truncate_respects_utf8_boundaries() {
        // A string that ends with a multi-byte char right at the boundary.
        let prefix = "α".repeat(MAX_RULE_BYTES / 2);
        let content = format!("{prefix}βγδ");
        let out = truncate(&content);
        assert!(out.ends_with("\n… [rule truncated]"));
        assert!(out.chars().all(|c| c != '\0'));
    }

    #[test]
    fn render_instructions_formats_sources() {
        let instructions = vec![
            ProjectInstruction {
                source: "AGENTS.md".to_string(),
                content: "be kind".to_string(),
            },
            ProjectInstruction {
                source: "rules/style.md".to_string(),
                content: "no panic".to_string(),
            },
        ];
        let rendered = render_instructions(&instructions);
        assert!(rendered.contains("--- from AGENTS.md ---"));
        assert!(rendered.contains("be kind"));
        assert!(rendered.contains("--- from rules/style.md ---"));
        assert!(rendered.contains("no panic"));
    }
}
