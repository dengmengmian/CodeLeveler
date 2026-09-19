//! Rendering skills into model context: the index (progressive disclosure),
//! a loaded package, and the `$name` turn injection.
//!
//! The index carries only names + descriptions + scope; the full body is
//! loaded on demand via `load_skill`, or turn-injected when the user names
//! `$skill`. That split is what keeps a growing skill library from
//! consuming context linearly.

use crate::registry::{SkillDetail, SkillMentionResolution, SkillSummary};

/// Extract `$skill-name` tokens from user text.
///
/// Names use letters, digits, `-`, and `_`. Boundaries: `$` start; end at the
/// first non-name character. Does not match `$$` or a bare `$`.
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

/// Progressive-disclosure usage rules appended to the skills index.
pub const SKILLS_HOW_TO_USE: &str = "\
How to use skills (progressive disclosure):\n\
- Discovery: the list above is name + description only. Full instructions live \
  in each skill's `SKILL.md` and are loaded with `load_skill`, or injected \
  automatically when the user names `$skill-name`.\n\
- Trigger: if the user names a skill (`$name`) OR the task \
  clearly matches a listed description, you MUST use that skill for the turn — \
  read its full instructions before other task actions. Multiple mentions mean \
  use them all. Do not carry skills across turns unless re-mentioned.\n\
- Missing: if a named skill is not in the list, say so briefly and continue.\n\
- Paths: resolve `scripts/…` and `references/…` relative to the skill `dir` \
  returned by load/injection. Prefer running provided scripts over retyping \
  large code blocks. Read required reference files yourself; do not delegate \
  reading skill instructions to a sub-agent.\n\
- Hygiene: load only what the task needs; do not open every bundled file by default.\n";

/// One line of the index: `name [scope]: description`.
fn index_line(skill: &SkillSummary) -> String {
    let description = skill
        .description
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "- {} [{}]: {}\n",
        skill.name,
        skill.scope.as_str(),
        description
    )
}

/// Render the skills index for injection into context (empty if none).
pub fn render_index(skills: &[SkillSummary]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut s = String::from(INDEX_HEADER);
    for skill in skills {
        s.push_str(&index_line(skill));
    }
    s.push('\n');
    s.push_str(SKILLS_HOW_TO_USE);
    s
}

/// Upper bound on the per-turn skill index, in bytes. The index rides on every
/// turn, so a machine with hundreds of skills must not crowd the conversation;
/// `/skills` and `load_skill` remain complete when it truncates.
pub const MAX_SKILL_INDEX_BYTES: usize = 8192;

/// Like [`render_index`], but bounded: it lists whole entries until the cap and
/// names how many were left out, then always appends the how-to-use rules.
pub fn render_capped_index(skills: &[SkillSummary], max_bytes: usize) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let reserve = SKILLS_HOW_TO_USE.len() + 256;
    let mut s = String::from(INDEX_HEADER);
    for (shown, skill) in skills.iter().enumerate() {
        let line = index_line(skill);
        if s.len() + line.len() + reserve > max_bytes {
            s.push_str(&format!(
                "- … and {} more skills not listed; name one to load it.\n",
                skills.len() - shown
            ));
            break;
        }
        s.push_str(&line);
    }
    s.push('\n');
    s.push_str(SKILLS_HOW_TO_USE);
    s
}

const INDEX_HEADER: &str = "Available skills — reusable procedures for specific tasks. Call \
     `load_skill` with a name to read full instructions before related work, \
     unless the user already named `$skill` (then follow the \
     turn injection):\n";

/// Format a full skill package for the model (`load_skill` or turn inject).
pub fn render_skill_package(detail: &SkillDetail) -> String {
    let mut s = format!(
        "# Skill: {}\n{}\n\nscope: {}\nsource: {}\ndir: {}\n\n",
        detail.name,
        detail.description,
        detail.scope.as_str(),
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
        s.push_str("\nUnknown skill mentions (not in the skills index; continue without them):\n");
        for name in &resolution.unknown {
            s.push_str(&format!("- `${name}`\n"));
        }
    }
    if !resolution.invalid.is_empty() {
        s.push_str(
            "\nNamed skills that exist but cannot be used (do not guess at their \
             contents; tell the user which one is broken and why):\n",
        );
        for (name, reason) in &resolution.invalid {
            s.push_str(&format!("- `${name}`: {reason}\n"));
        }
    }
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{SkillScope, SkillSource};

    #[test]
    fn parses_mentions_with_boundaries() {
        assert_eq!(
            parse_skill_mentions("use $deploy then $rust-review!"),
            vec!["deploy", "rust-review"]
        );
        assert!(parse_skill_mentions("cost is $$5 and $ alone").is_empty());
        assert_eq!(parse_skill_mentions("$a $a"), vec!["a"]);
    }

    #[test]
    fn render_index_is_metadata_only() {
        let skills = vec![SkillSummary {
            name: "x".into(),
            description: "does x".into(),
            scope: SkillScope::Project,
            source: SkillSource::Native,
        }];
        let idx = render_index(&skills);
        assert!(idx.contains("x [project]: does x"));
        assert!(idx.contains("How to use skills"));
        assert!(idx.contains("load_skill"));
    }

    #[test]
    fn a_capped_index_stays_bounded_and_keeps_the_rules() {
        let skills: Vec<SkillSummary> = (0..200)
            .map(|i| SkillSummary {
                name: format!("skill-{i:03}"),
                description: "a long description that should stop the listing early".to_string(),
                scope: SkillScope::Project,
                source: SkillSource::Native,
            })
            .collect();
        let capped = render_capped_index(&skills, 2048);
        assert!(capped.len() < 2048 + 256, "not bounded: {}", capped.len());
        assert!(capped.contains("more skills not listed"), "{capped}");
        assert!(capped.contains("How to use skills"), "{capped}");
        assert!(!capped.contains("skill-199"), "{capped}");
    }
}
