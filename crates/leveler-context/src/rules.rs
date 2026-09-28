//! Project rule loading and merging (spec §39): AGENTS.md, .leveler/instructions.md,
//! and .leveler/rules/*.md. Each instruction records its source.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Per-document delivery budget. This is a **selection** budget, not a
/// truncation point: the rules are split at markdown section boundaries and
/// whole sections are delivered until this many bytes are used. Anything left
/// over is still a project rule. The prompt says how many sections were left
/// out and names `read_project_rules`, which lists and returns them. A large
/// rules file loses no authority — only verbatim presence.
///
/// It is deliberately not unbounded. Dumping a 46 KB rules file into every
/// request pays for prose the current task does not need and pushes the
/// turn's real context out of the window.
pub const MAX_RULE_BYTES: usize = 16_000;

/// The tool that returns an undelivered rule section verbatim. Defined here
/// (rather than in the tool crate) because the prompt text that points at it
/// and the tool that answers it must name the same thing.
pub const READ_PROJECT_RULES_TOOL: &str = "read_project_rules";

/// One markdown section of a rules document, split deterministically at ATX
/// headings. `content` is the exact original text of the section, heading line
/// included — never a rewrite, so a delivered or retrieved section carries the
/// author's own words.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleSection {
    /// Stable, deterministic id within its document (heading slug, with an
    /// ordinal suffix on collision).
    pub id: String,
    /// The heading text (or `(preamble)` for text before the first heading).
    pub heading: String,
    /// Exact original section text.
    pub content: String,
    /// Byte length of `content`.
    pub bytes: usize,
}

/// Split a rules document into deterministic sections at markdown ATX
/// headings. Text before the first heading is the `preamble` section; a
/// document with no headings is one `document` section. Byte-for-byte, the
/// concatenation of every section's `content` is the input.
pub fn split_rule_sections(text: &str) -> Vec<RuleSection> {
    fn is_heading(line: &str) -> bool {
        let trimmed = line.trim_start();
        trimmed.starts_with('#') && trimmed[1..].starts_with(['#', ' ', '\t'])
    }
    fn slug(heading: &str) -> String {
        let mut out = String::new();
        for ch in heading.trim_start_matches('#').trim().chars() {
            if ch.is_ascii_alphanumeric() {
                out.push(ch.to_ascii_lowercase());
            } else if ch.is_alphanumeric() {
                out.push(ch);
            } else if !out.ends_with('-') && !out.is_empty() {
                out.push('-');
            }
        }
        let out = out.trim_matches('-').to_string();
        if out.is_empty() {
            "section".to_string()
        } else {
            out
        }
    }

    let mut sections: Vec<RuleSection> = Vec::new();
    let mut current: Option<(String, String)> = None; // (id base, heading)
    let mut body = String::new();
    for line in text.split_inclusive('\n') {
        if is_heading(line) {
            if let Some((base, heading)) = current.take() {
                push_section(&mut sections, base, heading, &body);
                body.clear();
            } else if !body.is_empty() {
                push_section(
                    &mut sections,
                    "preamble".to_string(),
                    "(preamble)".to_string(),
                    &body,
                );
                body.clear();
            }
            current = Some((
                slug(line),
                line.trim().trim_start_matches('#').trim().to_string(),
            ));
            body.push_str(line);
        } else {
            body.push_str(line);
        }
    }
    match current {
        Some((base, heading)) => push_section(&mut sections, base, heading, &body),
        None if !body.is_empty() => push_section(
            &mut sections,
            "document".to_string(),
            "(document)".to_string(),
            &body,
        ),
        None => {}
    }
    sections
}

fn push_section(sections: &mut Vec<RuleSection>, base: String, heading: String, body: &str) {
    if body.trim().is_empty() {
        return;
    }
    let mut id = base.clone();
    let mut n = 2u32;
    while sections.iter().any(|s| s.id == id) {
        id = format!("{base}-{n}");
        n += 1;
    }
    sections.push(RuleSection {
        id,
        heading,
        content: body.to_string(),
        bytes: body.len(),
    });
}

/// How a rule document is selected for delivery.
///
/// `always` names sections (heading or id substrings, case-insensitive) that
/// must reach the model in full every turn — architecture gates, safety
/// boundaries, verification policy, commit rules. The rest fills the budget in
/// document order and, when it does not fit, is indexed and retrievable
/// verbatim. The policy is host-defined data; a model never chooses it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuleDeliveryPolicy {
    pub always: Vec<String>,
    /// Per-document budget. `None` uses [`MAX_RULE_BYTES`].
    pub budget_bytes: Option<usize>,
}

impl RuleDeliveryPolicy {
    pub fn budget(&self) -> usize {
        self.budget_bytes.unwrap_or(MAX_RULE_BYTES)
    }

    fn is_always(&self, section: &RuleSection) -> bool {
        let id = section.id.to_lowercase();
        let heading = section.heading.to_lowercase();
        self.always.iter().any(|needle| {
            let needle = needle.trim().to_lowercase();
            !needle.is_empty() && (id.contains(&needle) || heading.contains(&needle))
        })
    }
}

/// What one rules document's delivery contains, and what it does not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleDelivery {
    pub source: String,
    /// Whole sections delivered verbatim, in document order.
    pub delivered: Vec<RuleSection>,
    /// Sections that exist but were not delivered (their text is available
    /// through [`READ_PROJECT_RULES_TOOL`]).
    pub omitted: Vec<RuleSection>,
    /// The first delivered section was longer than the whole budget and was
    /// cut once, at the byte limit, with an explicit marker.
    pub first_section_clipped: bool,
}

/// Select whole sections up to `budget` bytes. A first section larger than the
/// budget is delivered clipped, once, with a marker — never silently.
pub fn deliver_sections(source: &str, content: &str, budget: usize) -> RuleDelivery {
    deliver_sections_with_policy(
        source,
        content,
        &RuleDeliveryPolicy {
            always: Vec::new(),
            budget_bytes: Some(budget),
        },
    )
}

/// Select whole sections under `policy`: always-on sections first (in document
/// order), then the rest in document order until the budget is used. Anything
/// left over is returned as omitted for indexing.
pub fn deliver_sections_with_policy(
    source: &str,
    content: &str,
    policy: &RuleDeliveryPolicy,
) -> RuleDelivery {
    let budget = policy.budget();
    let mut sections = split_rule_sections(content);
    // Deterministic order: always-on sections first, each group in document
    // order. `sort_by_key` is stable, so document order is preserved inside a
    // group.
    sections.sort_by_key(|section| !policy.is_always(section));
    let mut delivered = Vec::new();
    let mut used = 0usize;
    let mut remaining = sections.into_iter();
    let mut first_section_clipped = false;
    while let Some(section) = remaining.next() {
        if used + section.bytes <= budget {
            used += section.bytes;
            delivered.push(section);
            continue;
        }
        if delivered.is_empty() && section.bytes > budget {
            // Nothing fits yet: deliver this one section clipped so the
            // document is not represented by an index alone.
            let clipped = leveler_core::truncate_head_bytes(
                &section.content,
                budget,
                "\n… [rule section truncated]",
            );
            first_section_clipped = true;
            used += clipped.len();
            delivered.push(RuleSection {
                content: clipped,
                bytes: section.bytes,
                ..section
            });
            continue;
        }
        // This section did not fit; it and everything after it is omitted.
        let mut omitted = vec![section];
        omitted.extend(remaining);
        return RuleDelivery {
            source: source.to_string(),
            delivered,
            omitted,
            first_section_clipped,
        };
    }
    RuleDelivery {
        source: source.to_string(),
        delivered,
        omitted: Vec::new(),
        first_section_clipped,
    }
}

/// A single project instruction with provenance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectInstruction {
    /// Workspace-relative source path.
    pub source: String,
    /// The instruction text (possibly truncated).
    pub content: String,
}

/// Load project rules from `root`, in priority order (root AGENTS.md
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
        // Stored in full. Selection happens at render time, by whole section,
        // so nothing is lost before the delivery decision is made.
        out.push(ProjectInstruction {
            source: rel.to_string(),
            content: trimmed.to_string(),
        });
    }
}

/// Load project rules already selected for delivery under `policy`.
///
/// The returned `content` is the prompt-ready body: the selected whole
/// sections, followed by the explicit index of what was not delivered. The
/// loader is the same one the retrieval tool uses, so a delivered section and
/// a retrieved section are the same bytes.
pub fn load_rules_for_delivery(
    root: &Path,
    policy: &RuleDeliveryPolicy,
) -> Vec<ProjectInstruction> {
    load_rules(root)
        .into_iter()
        .map(|instruction| ProjectInstruction {
            content: deliver_document(&instruction.source, &instruction.content, policy),
            source: instruction.source,
        })
        .collect()
}

/// Select and render one document's delivery body (no `--- from` header).
pub fn deliver_document(source: &str, content: &str, policy: &RuleDeliveryPolicy) -> String {
    let delivery = deliver_sections_with_policy(source, content, policy);
    let mut body = String::new();
    for section in &delivery.delivered {
        body.push_str(&section.content);
    }
    if !body.is_empty() && !body.ends_with('\n') {
        body.push('\n');
    }
    if !delivery.omitted.is_empty() {
        let count = delivery.omitted.len();
        let noun = if count == 1 { "section" } else { "sections" };
        let verb = if count == 1 { "was" } else { "were" };
        body.push_str(&format!(
            "--- {count} {noun} of `{source}` {verb} not included ---\n\
             They remain project rules. `{READ_PROJECT_RULES_TOOL}` with no \
             arguments lists every source and section id. The same tool with \
             `source` and `section` returns that section verbatim.\n",
        ));
    }
    body
}

/// Render instructions into a prompt block.
///
/// `content` is expected to be prompt-ready: either a full document (small
/// enough that selection is a no-op) or the output of [`deliver_document`].
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

/// Render instructions with an explicit delivery policy applied per document.
pub fn render_instructions_with(
    instructions: &[ProjectInstruction],
    policy: &RuleDeliveryPolicy,
) -> String {
    let selected: Vec<ProjectInstruction> = instructions
        .iter()
        .map(|instruction| ProjectInstruction {
            content: deliver_document(&instruction.source, &instruction.content, policy),
            source: instruction.source.clone(),
        })
        .collect();
    render_instructions(&selected)
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
    fn split_keeps_every_byte_and_splits_at_headings() {
        let doc = "intro line\n# Alpha\nA body\n## Beta\nB body\n";
        let sections = split_rule_sections(doc);
        assert_eq!(
            sections
                .iter()
                .map(|s| s.content.as_str())
                .collect::<String>(),
            doc,
            "splitting must not rewrite or drop bytes"
        );
        let ids: Vec<&str> = sections.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["preamble", "alpha", "beta"]);
        assert_eq!(sections[0].heading, "(preamble)");
        assert_eq!(sections[1].heading, "Alpha");
    }

    #[test]
    fn a_document_without_headings_is_one_section() {
        let sections = split_rule_sections("just prose\n");
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].id, "document");
    }

    #[test]
    fn duplicate_headings_get_deterministic_distinct_ids() {
        let doc = "# Notes\na\n# Notes\nb\n";
        let sections = split_rule_sections(doc);
        assert_eq!(
            sections.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            vec!["notes", "notes-2"]
        );
    }

    #[test]
    fn delivery_names_whole_omitted_sections_instead_of_cutting_the_tail() {
        let big = format!("# Keep\n{}\n# Late\n{}", "a".repeat(200), "b".repeat(200));
        let delivery = deliver_sections("AGENTS.md", &big, 300);
        assert_eq!(delivery.delivered.len(), 1);
        assert_eq!(delivery.omitted.len(), 1);
        assert_eq!(delivery.omitted[0].id, "late");
        // The delivered part is whole, not a byte cut.
        assert!(!delivery.delivered[0].content.contains("truncated"));
    }

    #[test]
    fn an_always_delivered_section_jumps_ahead_of_document_order() {
        let doc = format!(
            "# Big\n{}\n# Architecture Gates\nshort\n# Tail\n{}",
            "x".repeat(400),
            "y".repeat(400)
        );
        let policy = RuleDeliveryPolicy {
            always: vec!["architecture gates".to_string()],
            budget_bytes: Some(60),
        };
        let delivery = deliver_sections_with_policy("AGENTS.md", &doc, &policy);
        // The named section is delivered first even though it is not first in
        // the document; the huge leading section is not delivered whole.
        assert_eq!(delivery.delivered[0].id, "architecture-gates");
    }

    #[test]
    fn delivery_output_names_the_source_and_the_retrieval_tool() {
        let doc = format!("# Keep\nshort\n# Late\n{}", "z".repeat(MAX_RULE_BYTES));
        let body = deliver_document("AGENTS.md", &doc, &RuleDeliveryPolicy::default());
        assert!(body.contains("1 section of `AGENTS.md` was not included"));
        assert!(body.contains(READ_PROJECT_RULES_TOOL));
        assert!(!body.contains("[late]"));
    }

    #[test]
    fn a_first_section_larger_than_the_budget_is_clipped_once_and_marked() {
        let doc = format!("# Huge\n{}", "x".repeat(1_000));
        let delivery = deliver_sections("AGENTS.md", &doc, 200);
        assert!(delivery.first_section_clipped);
        assert!(delivery.delivered[0].content.ends_with("truncated]"));
    }

    #[test]
    fn render_instructions_points_at_the_retrieval_tool_for_omitted_sections() {
        let instruction = ProjectInstruction {
            source: "AGENTS.md".to_string(),
            content: format!("# Keep\nshort\n# Late\n{}", "z".repeat(MAX_RULE_BYTES)),
        };
        let rendered = render_instructions_with(&[instruction], &RuleDeliveryPolicy::default());
        assert!(rendered.contains("--- from AGENTS.md ---"));
        assert!(rendered.contains("1 section of `AGENTS.md` was not included"));
        assert!(rendered.contains(READ_PROJECT_RULES_TOOL));
        assert!(!rendered.contains("[late]"));
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
