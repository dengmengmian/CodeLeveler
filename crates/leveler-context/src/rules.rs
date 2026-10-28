//! Project rule loading and merging (spec §39): AGENTS.md, .leveler/instructions.md,
//! and .leveler/rules/*.md. Each instruction records its source.
//!
//! # The delivery contract
//!
//! Which project rules reach the model is decided here, deterministically, from
//! host data — never by the model and never by a retrieval round trip:
//!
//! - **Mandatory** sections are the ones the host names in
//!   `.leveler/config.yaml` (`rules.always_delivered`). They are what the task
//!   cannot be done correctly without, so they get the budget first.
//! - **Optional** sections fill the rest of the budget in document order.
//! - One request has ONE budget covering every rules document it delivers. It
//!   is measured in UTF-8 bytes of the exact prompt text, not in tokens; token
//!   counts are an estimate ([`leveler_model::estimate_text`]) and the two are
//!   never mixed in one figure.
//! - Anything not delivered is named (source, section id, heading) in the
//!   prompt itself, so the model knows what it does not have. A mandatory
//!   section that does not fit is reported loudly, never dropped silently.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Request-wide rule byte budget (selection, not truncation).
///
/// This is the ceiling for everything this module delivers in one request:
/// mandatory sections, optional sections and nothing else. It is deliberately
/// bounded: dumping an unbounded rules corpus into every request pays for prose
/// the current task does not need and pushes the turn's real context out of the
/// window. It is also the DEFAULT only — a project that needs more states it in
/// `.leveler/config.yaml`.
///
/// The unit is UTF-8 bytes of prompt text. Tokens are estimated separately and
/// are never confused with this number.
pub const MAX_RULE_BYTES: usize = 65_536;

/// The tool that returns a rule section verbatim. Defined here (rather than in
/// the tool crate) because the prompt text that points at it and the tool that
/// answers it must name the same thing.
///
/// It is an OPTIONAL query path. Nothing mandatory depends on the model
/// choosing to call it.
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

/// How a project selects its rules for delivery.
///
/// `always` names the MANDATORY sections (heading or id substrings,
/// case-insensitive): the rules whose absence changes whether the task is done
/// correctly. Everything else is optional and fills what is left of the one
/// request budget in document order. This is host data from
/// `.leveler/config.yaml`; a model never chooses it, and no fact about which
/// rules bind the task depends on a model deciding to look something up.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuleDeliveryPolicy {
    pub always: Vec<String>,
    /// Request-wide delivery budget. `None` uses [`MAX_RULE_BYTES`].
    pub budget_bytes: Option<usize>,
}

impl RuleDeliveryPolicy {
    pub fn budget(&self) -> usize {
        self.budget_bytes.unwrap_or(MAX_RULE_BYTES)
    }

    /// Whether the host declared this section mandatory.
    pub fn is_mandatory(&self, section: &RuleSection) -> bool {
        let id = section.id.to_lowercase();
        let heading = section.heading.to_lowercase();
        self.always.iter().any(|needle| {
            let needle = needle.trim().to_lowercase();
            !needle.is_empty() && (id.contains(&needle) || heading.contains(&needle))
        })
    }
}

/// One request's rule delivery: prompt-ready documents plus what it cost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleDeliveryPlan {
    pub documents: Vec<ProjectInstruction>,
    pub report: RuleDeliveryReport,
}

/// The byte accounting for one request's rule delivery.
///
/// Every figure is UTF-8 bytes of prompt text. `mandatory_bytes +
/// optional_bytes` never exceeds `budget_bytes`; `index_bytes` is the omission
/// notice, charged outside that budget so a rules file can still use all of it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuleDeliveryReport {
    pub budget_bytes: usize,
    pub mandatory_bytes: usize,
    pub optional_bytes: usize,
    pub index_bytes: usize,
    pub sections_delivered: usize,
    pub sections_omitted: usize,
    /// Mandatory sections that did not fit, as `source#id`.
    pub undelivered_mandatory: Vec<String>,
    /// Mandatory sections cut at the budget, as `source#id`.
    pub clipped_mandatory: Vec<String>,
    /// `always_delivered` entries that matched no section at all.
    pub unmatched_mandatory: Vec<String>,
}

/// What one document contributes, by class.
#[derive(Debug, Default)]
struct DocumentSelection {
    mandatory: Vec<RuleSection>,
    optional: Vec<RuleSection>,
    omitted: Vec<RuleSection>,
    undelivered_mandatory: Vec<RuleSection>,
    clipped_mandatory: Vec<RuleSection>,
}

/// Plan the delivery of every rule document under `root`.
pub fn plan_rule_delivery(root: &Path, policy: &RuleDeliveryPolicy) -> RuleDeliveryPlan {
    plan_instructions(load_rules(root), policy)
}

/// Plan delivery across already-loaded documents.
///
/// Two passes, one budget: all mandatory sections first (document order), then
/// optional sections (document order). A model, a task and a tool call cannot
/// change the order, so what a mandatory rule costs is predictable and an
/// optional section can never take its bytes.
pub fn plan_instructions(
    instructions: Vec<ProjectInstruction>,
    policy: &RuleDeliveryPolicy,
) -> RuleDeliveryPlan {
    let budget = policy.budget();
    let split: Vec<Vec<RuleSection>> = instructions
        .iter()
        .map(|instruction| split_rule_sections(&instruction.content))
        .collect();
    let mut selections: Vec<DocumentSelection> = (0..instructions.len())
        .map(|_| DocumentSelection::default())
        .collect();
    let mut report = RuleDeliveryReport {
        budget_bytes: budget,
        ..RuleDeliveryReport::default()
    };
    let mut used = 0usize;

    for (doc, sections) in split.iter().enumerate() {
        for section in sections
            .iter()
            .filter(|section| policy.is_mandatory(section))
        {
            if used + section.bytes <= budget {
                used += section.bytes;
                report.mandatory_bytes += section.bytes;
                report.sections_delivered += 1;
                selections[doc].mandatory.push(section.clone());
                continue;
            }
            // Mandatory is never dropped silently. A section that fits only
            // partly is delivered clipped and named; one that does not fit at
            // all is named as undelivered.
            match clip_to(section, budget.saturating_sub(used)) {
                Some(clipped) => {
                    used += clipped.bytes;
                    report.mandatory_bytes += clipped.bytes;
                    report.sections_delivered += 1;
                    report
                        .clipped_mandatory
                        .push(format!("{}#{}", instructions[doc].source, section.id));
                    selections[doc].clipped_mandatory.push(section.clone());
                    selections[doc].mandatory.push(clipped);
                }
                None => {
                    report.sections_omitted += 1;
                    report
                        .undelivered_mandatory
                        .push(format!("{}#{}", instructions[doc].source, section.id));
                    selections[doc].undelivered_mandatory.push(section.clone());
                }
            }
        }
    }

    for (doc, sections) in split.iter().enumerate() {
        for section in sections.iter().filter(|s| !policy.is_mandatory(s)) {
            if used + section.bytes <= budget {
                used += section.bytes;
                report.optional_bytes += section.bytes;
                report.sections_delivered += 1;
                selections[doc].optional.push(section.clone());
            } else {
                report.sections_omitted += 1;
                selections[doc].omitted.push(section.clone());
            }
        }
    }

    // A configured entry that matches nothing silently drops an
    // authority-bearing section, so it is reported instead of tolerated.
    for needle in &policy.always {
        let needle = needle.trim();
        if needle.is_empty() {
            continue;
        }
        let matched = split.iter().flatten().any(|section| {
            section.id.to_lowercase().contains(&needle.to_lowercase())
                || section
                    .heading
                    .to_lowercase()
                    .contains(&needle.to_lowercase())
        });
        if !matched {
            report.unmatched_mandatory.push(needle.to_string());
        }
    }

    let mut documents = Vec::new();
    let mut index_bytes = 0usize;
    for (instruction, selection) in instructions.iter().zip(&selections) {
        let (text, rendered_notices) = render_selection(instruction, selection, &report);
        index_bytes += rendered_notices;
        if text.trim().is_empty() {
            continue;
        }
        documents.push(ProjectInstruction {
            source: instruction.source.clone(),
            content: text,
        });
    }
    report.index_bytes = index_bytes;

    RuleDeliveryPlan { documents, report }
}

/// Clip one section to at most `remaining` bytes, or `None` when what would be
/// left is not worth reading. Clipping is always marked.
fn clip_to(section: &RuleSection, remaining: usize) -> Option<RuleSection> {
    const MIN_CLIP: usize = 512;
    if remaining < MIN_CLIP {
        return None;
    }
    let clipped = leveler_core::truncate_head_bytes(
        &section.content,
        remaining,
        "\n… [mandatory rule section truncated at the rule budget]",
    );
    Some(RuleSection {
        content: clipped.clone(),
        bytes: clipped.len(),
        ..section.clone()
    })
}

/// Render one document's delivered sections and its notices.
///
/// Returns the body (the caller adds `--- from <source> ---`) and the number of
/// bytes that belong to notices rather than rules.
fn render_selection(
    instruction: &ProjectInstruction,
    selection: &DocumentSelection,
    report: &RuleDeliveryReport,
) -> (String, usize) {
    let mut body = String::new();
    for section in selection.mandatory.iter().chain(&selection.optional) {
        body.push_str(&section.content);
        if !body.ends_with('\n') {
            body.push('\n');
        }
    }
    let mut notice = String::new();
    if !selection.omitted.is_empty() {
        notice.push_str(&format!(
            "\n--- {} of `{}` not delivered (over the {}-byte rule budget) ---\n",
            plural(
                selection.omitted.len(),
                "optional section",
                "optional sections"
            ),
            instruction.source,
            report.budget_bytes,
        ));
        for section in &selection.omitted {
            notice.push_str(&format!("  [{}] {}\n", section.id, section.heading));
        }
        // Not an invitation to fetch: the host declared these optional, and
        // every mandatory rule is already above this notice. Advertising each
        // one as a rule to go and read made the model spend 2-3 rounds on
        // background prose (measured: 4-6 retrieval calls on one task).
        notice.push_str(&format!(
            "They are optional here, not requirements of this task. `{}` returns any one \
             verbatim (source `{}`, section id above) if the work touches what it describes.\n",
            READ_PROJECT_RULES_TOOL, instruction.source,
        ));
    }
    if !selection.undelivered_mandatory.is_empty() {
        notice.push_str(&format!(
            "\n--- MANDATORY RULES NOT DELIVERED from `{}` ---\n",
            instruction.source
        ));
        for section in &selection.undelivered_mandatory {
            notice.push_str(&format!(
                "  [{}] {} ({} bytes)\n",
                section.id, section.heading, section.bytes
            ));
        }
        notice.push_str(&format!(
            "The {}-byte rule budget is smaller than the host's mandatory rule set, so these \
             binding sections are NOT in your context. Read them with `{}` before acting on \
             this repository.\n",
            report.budget_bytes, READ_PROJECT_RULES_TOOL,
        ));
    }
    if !selection.clipped_mandatory.is_empty() {
        notice.push_str(&format!(
            "\n--- MANDATORY RULE TEXT TRUNCATED (`{}`) ---\n",
            instruction.source
        ));
        for section in &selection.clipped_mandatory {
            notice.push_str(&format!(
                "  [{}] {} was cut at the rule budget; read it with `{}`\
                 (source `{}`, section `{}`) before relying on it.\n",
                section.id,
                section.heading,
                READ_PROJECT_RULES_TOOL,
                instruction.source,
                section.id
            ));
        }
    }
    let index_bytes = notice.len();
    body.push_str(&notice);
    (body, index_bytes)
}

fn plural(count: usize, one: &str, many: &str) -> String {
    if count == 1 {
        format!("{count} {one}")
    } else {
        format!("{count} {many}")
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

/// Every root rule document under `root`, in priority order.
pub fn load_rules(root: &Path) -> Vec<ProjectInstruction> {
    let mut out = Vec::new();
    for candidate in ["AGENTS.md", ".leveler/instructions.md"] {
        if let Some(rule) = read_rule_source(root, candidate) {
            out.push(rule);
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
            if let Some(rel) = path.strip_prefix(root).ok().and_then(|p| p.to_str())
                && let Some(rule) = read_rule_source(root, rel)
            {
                out.push(rule);
            }
        }
    }
    out
}

/// The ONE reader for a rule document, used by the prompt loader, the scoped
/// loader and the `read_project_rules` tool. One reader means a delivered
/// section and a retrieved section are the same bytes.
///
/// `rel` must be a workspace-relative rule document: absolute paths, `..`,
/// and any file that is not a rules candidate are refused. That keeps a
/// retrieval tool from laundering arbitrary file content into project
/// authority.
pub fn read_rule_source(root: &Path, rel: &str) -> Option<ProjectInstruction> {
    if !is_rule_document(rel) {
        return None;
    }
    let content = std::fs::read_to_string(root.join(rel)).ok()?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Stored in full. Selection happens at render time, by whole section, so
    // nothing is lost before the delivery decision is made.
    Some(ProjectInstruction {
        source: rel.to_string(),
        content: trimmed.to_string(),
    })
}

/// Whether `rel` is a rule document this product loads: an `AGENTS.md` in any
/// directory, `.leveler/instructions.md`, or `.leveler/rules/*.md`.
pub fn is_rule_document(rel: &str) -> bool {
    let path = Path::new(rel);
    if path.is_absolute() {
        return false;
    }
    let mut parts: Vec<String> = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    let Some(name) = parts.last() else {
        return false;
    };
    if name == "AGENTS.md" {
        return true;
    }
    match parts
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [".leveler", "instructions.md"] => true,
        [".leveler", "rules", file] => file.ends_with(".md"),
        _ => false,
    }
}

/// The nested rule documents that scope over `paths`, shallowest first.
///
/// This is discovery only: the caller reconciles the set it currently has
/// against this one, so a changed file is replaced and a deleted one is
/// dropped. Root `AGENTS.md` is never returned — it belongs to the request
/// prefix, not to a path.
pub fn scoped_rule_sources(root: &Path, paths: &[String]) -> Vec<ProjectInstruction> {
    let mut out: Vec<ProjectInstruction> = Vec::new();
    for path in paths {
        for rel in scoped_document_paths(path) {
            if rel == "AGENTS.md" || out.iter().any(|rule| rule.source == rel) {
                continue;
            }
            if let Some(rule) = read_rule_source(root, &rel) {
                out.push(rule);
            }
        }
    }
    out
}

fn scoped_document_paths(path: &str) -> Vec<String> {
    let path = Path::new(path);
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

    ancestors
        .into_iter()
        .map(|ancestor| format!("{}/AGENTS.md", ancestor.display()))
        .collect()
}

/// Render instructions into a prompt block.
///
/// `content` is expected to be prompt-ready: either a full document (small
/// enough that selection is a no-op) or the output of [`plan_rule_delivery`].
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

    fn instruction(source: &str, content: &str) -> ProjectInstruction {
        ProjectInstruction {
            source: source.to_string(),
            content: content.to_string(),
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("leveler-rules-{}-{tag}", std::process::id() as u64));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A document shaped like the real failure: a short requirement first,
    /// ordinary prose in the middle, one late requirement at the end.
    fn over_budget_document() -> String {
        let filler: String = (1..=7)
            .map(|i| {
                format!(
                    "# Background note {i}\n{}\n",
                    "ordinary prose. ".repeat(200)
                )
            })
            .collect();
        format!(
            "# Project rules\n\n## Answer artifact\nWrite `answers/answer.txt`.\n\n{filler}\n\
             ## Release marker\nWrite the token `LEVELER-LATE-7Q4` into `markers/late.txt`.\n"
        )
    }

    #[test]
    fn configured_rules_deliver_tdd_and_minimum_verification() {
        let root = temp_dir("configured-rules");
        std::fs::create_dir_all(root.join(".leveler")).unwrap();
        std::fs::write(
            root.join("AGENTS.md"),
            "# Project rules\n\n## TDD\n先写一个最小测试。\n\n## Verification\n最小充分验证。\n",
        )
        .unwrap();
        std::fs::write(
            root.join(".leveler/config.yaml"),
            "rules:\n  always_delivered: [TDD, Verification]\n  budget_bytes: 16000\n",
        )
        .unwrap();
        let config = leveler_project::config::ProjectConfig::load(&root)
            .expect("fixture verification config");
        let policy = RuleDeliveryPolicy {
            always: config.rules.always_delivered,
            budget_bytes: config.rules.budget_bytes,
        };
        let plan = plan_rule_delivery(&root, &policy);
        let delivered = plan
            .documents
            .iter()
            .map(|rule| rule.content.as_str())
            .collect::<String>();
        assert!(
            delivered.contains("先写一个最小"),
            "TDD behavior must be delivered"
        );
        assert!(
            delivered.contains("最小充分验证"),
            "verification policy must be delivered"
        );
        assert_eq!(
            plan.report.sections_omitted, 0,
            "repository rules must fit the budget"
        );
        assert!(plan.report.undelivered_mandatory.is_empty());
        assert!(
            plan.report.unmatched_mandatory.is_empty(),
            "every configured entry must match a section: {:?}",
            plan.report.unmatched_mandatory
        );
        for needle in &policy.always {
            assert!(
                delivered.to_lowercase().contains(&needle.to_lowercase()),
                "configured mandatory section was not delivered: {needle}"
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mandatory_sections_get_the_budget_before_optional_ones() {
        let doc = over_budget_document();
        let policy = RuleDeliveryPolicy {
            always: vec!["Release marker".to_string()],
            budget_bytes: Some(16_000),
        };
        let plan = plan_instructions(vec![instruction("AGENTS.md", &doc)], &policy);
        let body = &plan.documents[0].content;
        assert!(
            body.starts_with("## Release marker"),
            "the mandatory section leads the delivered block: {body:.200}"
        );
        assert!(body.contains("LEVELER-LATE-7Q4"));
        assert!(
            body.contains("Write `answers/answer.txt`"),
            "optional sections still fill the rest of the budget"
        );
        assert!(plan.report.mandatory_bytes > 0);
        assert!(plan.report.optional_bytes > 0);
        assert!(plan.report.mandatory_bytes + plan.report.optional_bytes <= 16_000);
        assert_eq!(plan.report.undelivered_mandatory, Vec::<String>::new());
    }

    #[test]
    fn an_optional_section_can_never_take_the_bytes_of_a_mandatory_one() {
        // The mandatory section is last, and the budget holds only it.
        let doc = format!(
            "# Big\n{}\n# Release marker\n{}\n",
            "x".repeat(4_000),
            "LEVELER-LATE-7Q4\n"
        );
        let policy = RuleDeliveryPolicy {
            always: vec!["Release marker".to_string()],
            budget_bytes: Some(600),
        };
        let plan = plan_instructions(vec![instruction("AGENTS.md", &doc)], &policy);
        let body = &plan.documents[0].content;
        assert!(
            body.starts_with("## Release marker") || body.starts_with("# Release marker"),
            "{body:.200}"
        );
        assert!(body.contains("LEVELER-LATE-7Q4"));
        assert_eq!(plan.report.optional_bytes, 0);
    }

    #[test]
    fn one_budget_covers_every_document_in_the_request() {
        let first = format!("# A\n{}\n", "a".repeat(8_000));
        let second = format!("# B\n{}\n", "b".repeat(8_000));
        let policy = RuleDeliveryPolicy {
            always: Vec::new(),
            budget_bytes: Some(10_000),
        };
        let plan = plan_instructions(
            vec![
                instruction("AGENTS.md", &first),
                instruction(".leveler/rules/b.md", &second),
            ],
            &policy,
        );
        assert_eq!(
            plan.report.mandatory_bytes + plan.report.optional_bytes <= 10_000,
            true,
            "the ceiling is the request, not each document: {plan:?}"
        );
        // The first document spends the budget; the second is indexed.
        assert_eq!(plan.documents.len(), 2);
        assert!(plan.documents[1].content.contains("not delivered"));
        assert!(
            plan.documents[1].content.contains("[b] B"),
            "the index names the section id and title: {}",
            plan.documents[1].content
        );
    }

    #[test]
    fn a_mandatory_section_in_a_later_document_outranks_earlier_optional_prose() {
        let first = format!("# A\n{}\n", "a".repeat(8_000));
        let second = "# B\n\n## Release marker\nLEVELER-LATE-7Q4\n".to_string();
        let policy = RuleDeliveryPolicy {
            always: vec!["Release marker".to_string()],
            budget_bytes: Some(9_000),
        };
        let plan = plan_instructions(
            vec![
                instruction("AGENTS.md", &first),
                instruction(".leveler/rules/b.md", &second),
            ],
            &policy,
        );
        let delivered = plan
            .documents
            .iter()
            .map(|rule| rule.content.as_str())
            .collect::<String>();
        assert!(
            delivered.contains("LEVELER-LATE-7Q4"),
            "mandatory is allocated across documents first: {delivered:.300}"
        );
        assert!(plan.report.undelivered_mandatory.is_empty());
    }

    #[test]
    fn an_omitted_section_is_named_by_id_and_title() {
        let doc = over_budget_document();
        let policy = RuleDeliveryPolicy {
            always: Vec::new(),
            budget_bytes: Some(1_500),
        };
        let plan = plan_instructions(vec![instruction("AGENTS.md", &doc)], &policy);
        let body = &plan.documents[0].content;
        assert!(body.contains("of `AGENTS.md` not delivered"), "{body:.400}");
        assert!(
            body.contains("[background-note-1] Background note 1"),
            "{body:.2000}"
        );
        assert!(body.contains(READ_PROJECT_RULES_TOOL));
        assert!(plan.report.sections_omitted > 0);
        assert!(plan.report.index_bytes > 0);
    }

    #[test]
    fn an_undelivered_mandatory_section_is_reported_loudly_not_dropped() {
        // 2 KiB of mandatory rules under a 1 KiB budget: the host asked for
        // more than it allows. The plan must say so, in the prompt and in the
        // report.
        let doc = format!("# Release marker\n{}\n", "LEVELER-LATE-7Q4 ".repeat(150));
        let policy = RuleDeliveryPolicy {
            always: vec!["Release marker".to_string()],
            budget_bytes: Some(1_000),
        };
        let plan = plan_instructions(vec![instruction("AGENTS.md", &doc)], &policy);
        let body = &plan.documents[0].content;
        assert!(
            plan.report.undelivered_mandatory.is_empty()
                || !plan.report.clipped_mandatory.is_empty(),
            "either clipped-and-named or undelivered-and-named: {:?}",
            plan.report
        );
        assert!(
            !plan.report.undelivered_mandatory.is_empty()
                || !plan.report.clipped_mandatory.is_empty(),
            "{:?}",
            plan.report
        );
        assert!(
            body.contains("MANDATORY RULE") || body.contains("MANDATORY RULES NOT DELIVERED"),
            "{body:.300}"
        );
    }

    #[test]
    fn a_mandatory_entry_that_matches_nothing_is_reported() {
        let policy = RuleDeliveryPolicy {
            always: vec!["Verification Policy".to_string()],
            budget_bytes: None,
        };
        let plan = plan_instructions(
            vec![instruction("AGENTS.md", "# Notes\nnothing here\n")],
            &policy,
        );
        assert_eq!(
            plan.report.unmatched_mandatory,
            vec!["Verification Policy".to_string()]
        );
    }

    #[test]
    fn a_document_that_fits_is_delivered_whole_and_unchanged() {
        let doc = "# Alpha\na\n# Beta\nb\n";
        let plan = plan_instructions(
            vec![instruction("AGENTS.md", doc)],
            &RuleDeliveryPolicy::default(),
        );
        assert_eq!(plan.documents[0].content, doc);
        assert_eq!(plan.report.sections_delivered, 2);
        assert_eq!(plan.report.sections_omitted, 0);
        assert_eq!(plan.report.index_bytes, 0);
    }

    #[test]
    fn loads_agents_md_and_rules() {
        let dir = temp_dir("load");
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
    fn scoped_rule_sources_scope_nested_files_and_never_repeat_the_root() {
        let dir = temp_dir("scoped");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("AGENTS.md"), "Root rule.").unwrap();
        std::fs::write(dir.join("src/AGENTS.md"), "Src rule v1.").unwrap();

        let rules = scoped_rule_sources(&dir, &["src/lib.rs".to_string()]);
        assert_eq!(rules.len(), 1, "{rules:?}");
        assert_eq!(rules[0].source, "src/AGENTS.md");
        assert!(rules[0].content.contains("Src rule v1"));

        // A changed file is a changed source: the same call must see v2.
        std::fs::write(dir.join("src/AGENTS.md"), "Src rule v2.").unwrap();
        let rules = scoped_rule_sources(&dir, &["src/lib.rs".to_string()]);
        assert!(rules[0].content.contains("Src rule v2"));

        // A deleted file is no longer a source at all.
        std::fs::remove_file(dir.join("src/AGENTS.md")).unwrap();
        assert!(scoped_rule_sources(&dir, &["src/lib.rs".to_string()]).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn only_rule_documents_are_readable_as_rules() {
        let dir = temp_dir("read-source");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "pub fn x() {}").unwrap();
        std::fs::write(dir.join("src/AGENTS.md"), "Src rule.").unwrap();
        assert!(read_rule_source(&dir, "src/lib.rs").is_none());
        assert!(read_rule_source(&dir, "../AGENTS.md").is_none());
        assert!(read_rule_source(&dir, "/etc/passwd").is_none());
        assert!(
            read_rule_source(&dir, "src/AGENTS.md")
                .expect("nested rules")
                .content
                .contains("Src rule")
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn empty_when_no_rules() {
        let dir = temp_dir("none");
        assert!(load_rules(&dir).is_empty());
        assert!(
            plan_rule_delivery(&dir, &RuleDeliveryPolicy::default())
                .documents
                .is_empty()
        );
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
    fn render_instructions_formats_sources() {
        let instructions = vec![
            instruction("AGENTS.md", "be kind"),
            instruction("rules/style.md", "no panic"),
        ];
        let rendered = render_instructions(&instructions);
        assert!(rendered.contains("--- from AGENTS.md ---"));
        assert!(rendered.contains("be kind"));
        assert!(rendered.contains("--- from rules/style.md ---"));
        assert!(rendered.contains("no panic"));
    }
}
