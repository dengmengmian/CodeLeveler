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
    /// Whether this line opens or closes a fenced code block. Only a fence
    /// with three or more backticks/tildes counts, and a closing fence must
    /// use the same character. Anything inside such a block is sample text,
    /// not document structure.
    fn fence(line: &str) -> Option<(char, usize)> {
        let trimmed = line.trim_start_matches(' ');
        if line.len() - trimmed.len() > 3 {
            return None;
        }
        let marker = trimmed.chars().next()?;
        if marker != '`' && marker != '~' {
            return None;
        }
        let run = trimmed.chars().take_while(|c| *c == marker).count();
        (run >= 3).then_some((marker, run))
    }
    fn is_heading(line: &str) -> bool {
        let indent = line.len() - line.trim_start_matches(' ').len();
        if indent > 3 {
            return false;
        }
        let trimmed = line.trim_start_matches(' ');
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
    let mut open_fence: Option<(char, usize)> = None;
    for line in text.split_inclusive('\n') {
        // A heading-looking line inside a fenced code block is sample text.
        // Splitting there invents a section and changes what the host can
        // select as mandatory, so fence state is part of the document shape.
        if let Some((marker, opened)) = open_fence {
            if let Some((close_marker, close_run)) = fence(line)
                && close_marker == marker
                && close_run >= opened
                && line.trim_start_matches(' ')[close_run..].trim().is_empty()
            {
                open_fence = None;
            }
            body.push_str(line);
            continue;
        }
        if let Some((marker, run)) = fence(line) {
            open_fence = Some((marker, run));
            body.push_str(line);
            continue;
        }
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

/// Why a rule document could not be read, or why a policy file could not be
/// parsed. `NotFound` is not a variant: an absent optional rule is not a
/// failure, and callers need that case to stay distinguishable from one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleReadErrorKind {
    /// The path is syntactically not a rule document this product loads
    /// (absolute, `..`, or an unrelated filename).
    NotARuleDocument,
    /// The candidate resolves outside the workspace root, including through a
    /// symlink.
    OutsideWorkspace,
    PermissionDenied,
    InvalidUtf8,
    /// The candidate exists but is not a regular file (a directory, socket…).
    NotAFile,
    /// The rule delivery policy file exists but is not valid YAML/config.
    Parse,
    Io,
}

impl RuleReadErrorKind {
    /// Stable key used in logs and prompt diagnostics; the serde spelling is
    /// the same so a log reader and an event reader agree.
    pub fn as_key(self) -> &'static str {
        match self {
            Self::NotARuleDocument => "not_a_rule_document",
            Self::OutsideWorkspace => "outside_workspace",
            Self::PermissionDenied => "permission_denied",
            Self::InvalidUtf8 => "invalid_utf8",
            Self::NotAFile => "not_a_file",
            Self::Parse => "parse",
            Self::Io => "io",
        }
    }
}

/// One structured read failure. `source` is workspace-relative for rule
/// documents (the policy file is named `.leveler/config.yaml`), so the
/// diagnostic can be logged and persisted without an absolute private path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleReadError {
    pub source: String,
    pub kind: RuleReadErrorKind,
    pub detail: String,
}

impl RuleReadError {
    pub fn new(
        source: impl Into<String>,
        kind: RuleReadErrorKind,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            source: source.into(),
            kind,
            detail: detail.into(),
        }
    }

    fn from_io(source: &str, error: &std::io::Error) -> Self {
        let kind = match error.kind() {
            std::io::ErrorKind::PermissionDenied => RuleReadErrorKind::PermissionDenied,
            std::io::ErrorKind::IsADirectory => RuleReadErrorKind::NotAFile,
            _ => RuleReadErrorKind::Io,
        };
        Self::new(source, kind, error.to_string())
    }
}

/// The result of reading one candidate: loaded text, an existing empty file,
/// or no such file. Only the first is a rule source; the other two must stay
/// distinguishable so an empty or missing file is not mistaken for a failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleSourceRead {
    Loaded(ProjectInstruction),
    Empty,
    Absent,
}

/// Everything discovered under one root: usable documents, empty candidates,
/// and read failures. Discovery never turns a failure into an empty rule set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoadedRuleSet {
    pub documents: Vec<ProjectInstruction>,
    pub empty: Vec<String>,
    pub errors: Vec<RuleReadError>,
}

/// Nested rule sources discovered for the current call paths. Read failures
/// are returned beside the documents so a caller cannot mistake "unreadable"
/// for "no rules here".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScopedRules {
    pub documents: Vec<ProjectInstruction>,
    pub errors: Vec<RuleReadError>,
}

/// Where one rule source ended up in this request's delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleSourceStatus {
    Delivered,
    /// At least one section reached the prompt, but not all of them.
    PartiallyDelivered,
    /// Read successfully, but the budget admitted none of its sections.
    Omitted,
    /// The file exists and contains no non-whitespace text.
    Empty,
    /// The file could not be read; `RuleDeliveryReport::read_errors` says why.
    Unreadable,
}

/// One rule source in the delivery audit. `sha256` is over the exact loaded
/// text (trimmed), so an operator can compare what was selected with what is
/// on disk without persisting the rule body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleSourceAudit {
    pub source: String,
    pub bytes: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    pub status: RuleSourceStatus,
}

/// Whether the delivery policy came from the project file or the built-in
/// defaults.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RulePolicySource {
    #[default]
    Default,
    ProjectConfig,
}

/// Provenance of the delivery policy, including a structured failure when the
/// file exists but could not be read or parsed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RulePolicyAudit {
    pub source: RulePolicySource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RuleReadError>,
}

/// The durable delivery audit for one turn: which sources were read, their
/// content identity, the policy that selected sections, and what the plan
/// delivered. This records selection into the turn's control context; it is
/// not a receipt from the provider.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleDeliveryAudit {
    /// Canonical workspace identity, when a workspace exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// Durable turn identity when the host supplies one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub policy: RulePolicyAudit,
    pub sources: Vec<RuleSourceAudit>,
    pub report: RuleDeliveryReport,
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

/// One request's rule delivery: prompt-ready documents, the per-source audit,
/// and what it cost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleDeliveryPlan {
    pub documents: Vec<ProjectInstruction>,
    pub report: RuleDeliveryReport,
    /// Every discovered source, in delivery order, with its status.
    pub sources: Vec<RuleSourceAudit>,
}

/// The byte accounting for one request's rule delivery.
///
/// Every figure is UTF-8 bytes of prompt text. `mandatory_bytes +
/// optional_bytes` never exceeds `budget_bytes`; `index_bytes` is the omission
/// notice, charged outside that budget so a rules file can still use all of it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleDeliveryReport {
    pub budget_bytes: usize,
    pub mandatory_bytes: usize,
    pub optional_bytes: usize,
    pub index_bytes: usize,
    /// Delivered sections as `source#id`; a clipped mandatory section counts
    /// here because it did reach the prompt.
    pub delivered_sections: Vec<String>,
    /// Sections that reached the prompt in no form, as `source#id`.
    pub omitted_sections: Vec<String>,
    /// Mandatory sections that did not fit, as `source#id`.
    pub undelivered_mandatory: Vec<String>,
    /// Mandatory sections cut at the budget, as `source#id`.
    pub clipped_mandatory: Vec<String>,
    /// `always_delivered` entries that matched no section at all.
    pub unmatched_mandatory: Vec<String>,
    /// Files that existed but could not be read. Empty is not a substitute.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read_errors: Vec<RuleReadError>,
}

/// What one document contributes, by class.
#[derive(Debug, Default)]
struct DocumentSelection {
    mandatory: Vec<RuleSection>,
    optional: Vec<RuleSection>,
    omitted: Vec<RuleSection>,
    undelivered_mandatory: Vec<RuleSection>,
    clipped_mandatory: Vec<RuleSection>,
    delivered: usize,
    total: usize,
}

/// Plan the delivery of every rule document under `root`.
pub fn plan_rule_delivery(root: &Path, policy: &RuleDeliveryPolicy) -> RuleDeliveryPlan {
    let loaded = load_rules(root);
    plan_instructions_with(loaded.documents, policy, loaded.empty, loaded.errors)
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
    plan_instructions_with(instructions, policy, Vec::new(), Vec::new())
}

fn plan_instructions_with(
    instructions: Vec<ProjectInstruction>,
    policy: &RuleDeliveryPolicy,
    empty: Vec<String>,
    read_errors: Vec<RuleReadError>,
) -> RuleDeliveryPlan {
    let budget = policy.budget();
    let split: Vec<Vec<RuleSection>> = instructions
        .iter()
        .map(|instruction| split_rule_sections(&instruction.content))
        .collect();
    let mut selections: Vec<DocumentSelection> = split
        .iter()
        .map(|sections| DocumentSelection {
            total: sections.len(),
            ..DocumentSelection::default()
        })
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
                report
                    .delivered_sections
                    .push(format!("{}#{}", instructions[doc].source, section.id));
                selections[doc].delivered += 1;
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
                    report
                        .delivered_sections
                        .push(format!("{}#{}", instructions[doc].source, section.id));
                    selections[doc].delivered += 1;
                    report
                        .clipped_mandatory
                        .push(format!("{}#{}", instructions[doc].source, section.id));
                    selections[doc].clipped_mandatory.push(section.clone());
                    selections[doc].mandatory.push(clipped);
                }
                None => {
                    report
                        .omitted_sections
                        .push(format!("{}#{}", instructions[doc].source, section.id));
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
                report
                    .delivered_sections
                    .push(format!("{}#{}", instructions[doc].source, section.id));
                selections[doc].delivered += 1;
                selections[doc].optional.push(section.clone());
            } else {
                report
                    .omitted_sections
                    .push(format!("{}#{}", instructions[doc].source, section.id));
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

    let mut sources = Vec::with_capacity(instructions.len() + empty.len() + read_errors.len());
    for (instruction, selection) in instructions.iter().zip(&selections) {
        let status = if selection.delivered == 0 {
            RuleSourceStatus::Omitted
        } else if selection.delivered < selection.total {
            RuleSourceStatus::PartiallyDelivered
        } else {
            RuleSourceStatus::Delivered
        };
        sources.push(RuleSourceAudit {
            source: instruction.source.clone(),
            bytes: instruction.content.len(),
            sha256: Some(content_sha256(&instruction.content)),
            status,
        });
    }
    for source in &empty {
        sources.push(RuleSourceAudit {
            source: source.clone(),
            bytes: 0,
            sha256: None,
            status: RuleSourceStatus::Empty,
        });
    }
    for error in &read_errors {
        sources.push(RuleSourceAudit {
            source: error.source.clone(),
            bytes: 0,
            sha256: None,
            status: RuleSourceStatus::Unreadable,
        });
    }
    report.read_errors = read_errors;
    RuleDeliveryPlan {
        documents,
        report,
        sources,
    }
}

fn content_sha256(text: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    format!("{:x}", hasher.finalize())
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

/// A rule source identity as it is delivered: workspace-relative and joined
/// with `/` on every host. A source is a wire value (prompt provenance,
/// delivery audit, tool results), not a native path, so the same rule must
/// have the same identity everywhere.
fn workspace_relative_identity(path: &Path) -> String {
    let text = path.to_string_lossy();
    if cfg!(windows) {
        text.replace('\\', "/")
    } else {
        text.into_owned()
    }
}

/// Every root rule document under `root`, in priority order.
///
/// An absent optional document is normal; a document that exists but cannot be
/// read is returned in `errors` and never collapsed into "no rules". Root and
/// candidate are resolved to their real paths, so a symlink that leaves the
/// workspace is refused instead of being read.
pub fn load_rules(root: &Path) -> LoadedRuleSet {
    let mut out = LoadedRuleSet::default();
    let canonical_root = match root.canonicalize() {
        Ok(root) => root,
        Err(error) => {
            out.errors.push(RuleReadError::new(
                ".",
                RuleReadErrorKind::Io,
                format!("workspace root: {error}"),
            ));
            return out;
        }
    };
    for candidate in ["AGENTS.md", ".leveler/instructions.md"] {
        match read_rule_source_in(&canonical_root, candidate) {
            Ok(RuleSourceRead::Loaded(rule)) => out.documents.push(rule),
            Ok(RuleSourceRead::Empty) => out.empty.push(candidate.to_string()),
            Ok(RuleSourceRead::Absent) => {}
            Err(error) => out.errors.push(error),
        }
    }
    // .leveler/rules/*.md, sorted for determinism. The directory is contained
    // once so an empty symlinked directory cannot be read from outside.
    let rules_dir = canonical_root.join(".leveler/rules");
    match rules_dir.canonicalize() {
        Ok(dir) => {
            if !dir.starts_with(&canonical_root) {
                out.errors.push(RuleReadError::new(
                    ".leveler/rules",
                    RuleReadErrorKind::OutsideWorkspace,
                    "rule directory resolves outside the workspace root",
                ));
                return out;
            }
            if !dir.is_dir() {
                out.errors.push(RuleReadError::new(
                    ".leveler/rules",
                    RuleReadErrorKind::NotAFile,
                    "rule path is not a directory",
                ));
                return out;
            }
            match std::fs::read_dir(&dir) {
                Ok(entries) => {
                    let mut paths = Vec::new();
                    for entry in entries {
                        match entry {
                            Ok(entry) => {
                                if entry.path().extension().and_then(|e| e.to_str()) == Some("md") {
                                    paths.push(entry.path());
                                }
                            }
                            Err(error) => out
                                .errors
                                .push(RuleReadError::from_io(".leveler/rules", &error)),
                        }
                    }
                    paths.sort();
                    for path in paths {
                        let Some(rel) = path
                            .strip_prefix(&canonical_root)
                            .ok()
                            .and_then(|rel| rel.to_str().map(|_| workspace_relative_identity(rel)))
                        else {
                            // The failure is diagnosable without logging the
                            // absolute location: the source stays workspace-
                            // relative like every other rule source.
                            let source = path
                                .strip_prefix(&canonical_root)
                                .map(workspace_relative_identity)
                                .unwrap_or_else(|_| path.to_string_lossy().into_owned());
                            out.errors.push(RuleReadError::new(
                                source,
                                RuleReadErrorKind::InvalidUtf8,
                                "rule file name is not UTF-8",
                            ));
                            continue;
                        };
                        match read_rule_source_in(&canonical_root, &rel) {
                            Ok(RuleSourceRead::Loaded(rule)) => out.documents.push(rule),
                            Ok(RuleSourceRead::Empty) => out.empty.push(rel),
                            Ok(RuleSourceRead::Absent) => {}
                            Err(error) => out.errors.push(error),
                        }
                    }
                }
                Err(error) => out
                    .errors
                    .push(RuleReadError::from_io(".leveler/rules", &error)),
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => out
            .errors
            .push(RuleReadError::from_io(".leveler/rules", &error)),
    }
    out
}

/// The ONE reader for a rule document, used by the prompt loader, the scoped
/// loader and the `read_project_rules` tool. One reader means a delivered
/// section and a retrieved section are the same bytes.
///
/// `rel` must be a workspace-relative rule document: absolute paths, `..`,
/// and any file that is not a rules candidate are refused. The candidate is
/// resolved with `canonicalize` and checked against the canonical root with
/// component-aware containment, so a symlink cannot launder arbitrary file
/// content into project authority. A missing optional file is `Absent`, not an
/// error; every other failure is returned, never swallowed.
pub fn read_rule_source(root: &Path, rel: &str) -> Result<RuleSourceRead, RuleReadError> {
    if !is_rule_document(rel) {
        return Err(RuleReadError::new(
            rel,
            RuleReadErrorKind::NotARuleDocument,
            "not a project rule document",
        ));
    }
    let canonical_root = root.canonicalize().map_err(|error| {
        RuleReadError::new(
            rel,
            RuleReadErrorKind::Io,
            format!("workspace root: {error}"),
        )
    })?;
    read_rule_source_in(&canonical_root, rel)
}

fn read_rule_source_in(root: &Path, rel: &str) -> Result<RuleSourceRead, RuleReadError> {
    if !is_rule_document(rel) {
        return Err(RuleReadError::new(
            rel,
            RuleReadErrorKind::NotARuleDocument,
            "not a project rule document",
        ));
    }
    let path = root.join(rel);
    // `symlink_metadata` sees the entry itself, so a dangling symlink is
    // present-but-broken, while a genuinely missing candidate stays absent.
    match std::fs::symlink_metadata(&path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(RuleSourceRead::Absent);
        }
        Err(error) => return Err(RuleReadError::from_io(rel, &error)),
    }
    let canonical = match path.canonicalize() {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(RuleReadError::new(
                rel,
                RuleReadErrorKind::Io,
                "dangling symlink",
            ));
        }
        Err(error) => return Err(RuleReadError::from_io(rel, &error)),
    };
    if !workspace_contains(root, &canonical) {
        return Err(RuleReadError::new(
            rel,
            RuleReadErrorKind::OutsideWorkspace,
            "rule file resolves outside the workspace root",
        ));
    }
    let metadata =
        std::fs::metadata(&canonical).map_err(|error| RuleReadError::from_io(rel, &error))?;
    if !metadata.is_file() {
        return Err(RuleReadError::new(
            rel,
            RuleReadErrorKind::NotAFile,
            "rule path is not a regular file",
        ));
    }
    let bytes = std::fs::read(&canonical).map_err(|error| RuleReadError::from_io(rel, &error))?;
    let content = match String::from_utf8(bytes) {
        Ok(content) => content,
        Err(error) => {
            return Err(RuleReadError::new(
                rel,
                RuleReadErrorKind::InvalidUtf8,
                format!("invalid UTF-8 at byte {}", error.utf8_error().valid_up_to()),
            ));
        }
    };
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Ok(RuleSourceRead::Empty);
    }
    // Stored in full. Selection happens at render time, by whole section, so
    // nothing is lost before the delivery decision is made.
    Ok(RuleSourceRead::Loaded(ProjectInstruction {
        source: rel.to_string(),
        content: trimmed.to_string(),
    }))
}

/// Whether `target` is `root` or below it. Both paths must already be
/// canonical, so a symlink cannot vouch for its own containment.
fn workspace_contains(root: &Path, target: &Path) -> bool {
    if target.starts_with(root) {
        return true;
    }
    // Windows paths compare case-insensitively; canonicalize does not
    // normalize the casing of every component (and the drive letter may
    // differ). `starts_with` above is therefore not sufficient there.
    #[cfg(windows)]
    {
        let root = root.to_string_lossy().to_lowercase();
        let target = target.to_string_lossy().to_lowercase();
        target
            .strip_prefix(&root)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('\\') || rest.starts_with('/'))
    }
    #[cfg(not(windows))]
    false
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
/// prefix, not to a path. Read failures are returned beside the documents; a
/// path that is a directory also scopes its own `AGENTS.md`.
pub fn scoped_rule_sources(root: &Path, paths: &[String]) -> ScopedRules {
    let mut out = ScopedRules::default();
    let canonical_root = match root.canonicalize() {
        Ok(root) => root,
        Err(error) => {
            out.errors.push(RuleReadError::new(
                ".",
                RuleReadErrorKind::Io,
                format!("workspace root: {error}"),
            ));
            return out;
        }
    };
    for path in paths {
        let is_dir = canonical_root.join(path).is_dir();
        for rel in scoped_document_paths(path, is_dir) {
            if rel == "AGENTS.md" || out.documents.iter().any(|rule| rule.source == rel) {
                continue;
            }
            match read_rule_source_in(&canonical_root, &rel) {
                Ok(RuleSourceRead::Loaded(rule)) => out.documents.push(rule),
                Ok(RuleSourceRead::Empty) | Ok(RuleSourceRead::Absent) => {}
                Err(error) => {
                    if !out.errors.iter().any(|known| known.source == error.source) {
                        out.errors.push(error);
                    }
                }
            }
        }
    }
    out
}

fn scoped_document_paths(path: &str, is_dir: bool) -> Vec<String> {
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
    // A command CWD can itself be a directory (`run_command` with `cwd:
    // src/npc`), so its own AGENTS.md scopes the command; treating it as a file
    // skipped exactly the rule that governs the directory being worked in.
    if is_dir && !current.as_os_str().is_empty() {
        ancestors.push(current);
    }

    ancestors
        .into_iter()
        .map(|ancestor| format!("{}/AGENTS.md", workspace_relative_identity(&ancestor)))
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
        assert!(
            plan.report.omitted_sections.is_empty(),
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
        assert!(
            plan.report.mandatory_bytes + plan.report.optional_bytes <= 10_000,
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
        assert!(!plan.report.omitted_sections.is_empty());
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
        assert_eq!(
            plan.report.delivered_sections,
            vec!["AGENTS.md#alpha".to_string(), "AGENTS.md#beta".to_string()]
        );
        assert!(plan.report.omitted_sections.is_empty());
        assert_eq!(plan.report.index_bytes, 0);
        assert_eq!(plan.sources.len(), 1);
        assert_eq!(plan.sources[0].status, RuleSourceStatus::Delivered);
        assert_eq!(plan.sources[0].bytes, doc.len());
        assert_eq!(
            plan.sources[0].sha256.as_deref(),
            Some(content_sha256(doc).as_str())
        );
    }

    #[test]
    fn loads_agents_md_and_rules() {
        let dir = temp_dir("load");
        std::fs::create_dir_all(dir.join(".leveler/rules")).unwrap();
        std::fs::write(dir.join("AGENTS.md"), "Use tabs.").unwrap();
        std::fs::write(dir.join(".leveler/rules/style.md"), "No unwrap.").unwrap();
        let rules = load_rules(&dir);
        assert!(rules.errors.is_empty(), "{:?}", rules.errors);
        assert_eq!(rules.documents.len(), 2);
        assert_eq!(rules.documents[0].source, "AGENTS.md");
        assert!(
            rules
                .documents
                .iter()
                .any(|r| r.content.contains("No unwrap"))
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scoped_rule_sources_scope_nested_files_and_never_repeat_the_root() {
        let dir = temp_dir("scoped");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("AGENTS.md"), "Root rule.").unwrap();
        std::fs::write(dir.join("src/AGENTS.md"), "Src rule v1.").unwrap();

        let rules = scoped_rule_sources(&dir, &["src/lib.rs".to_string()]);
        assert!(rules.errors.is_empty(), "{:?}", rules.errors);
        assert_eq!(rules.documents.len(), 1, "{rules:?}");
        assert_eq!(rules.documents[0].source, "src/AGENTS.md");
        assert!(rules.documents[0].content.contains("Src rule v1"));

        // A changed file is a changed source: the same call must see v2.
        std::fs::write(dir.join("src/AGENTS.md"), "Src rule v2.").unwrap();
        let rules = scoped_rule_sources(&dir, &["src/lib.rs".to_string()]);
        assert!(rules.documents[0].content.contains("Src rule v2"));

        // A deleted file is no longer a source at all.
        std::fs::remove_file(dir.join("src/AGENTS.md")).unwrap();
        assert!(
            scoped_rule_sources(&dir, &["src/lib.rs".to_string()])
                .documents
                .is_empty()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn only_rule_documents_are_readable_as_rules() {
        let dir = temp_dir("read-source");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "pub fn x() {}").unwrap();
        std::fs::write(dir.join("src/AGENTS.md"), "Src rule.").unwrap();
        assert_eq!(
            read_rule_source(&dir, "src/lib.rs"),
            Err(RuleReadError::new(
                "src/lib.rs",
                RuleReadErrorKind::NotARuleDocument,
                "not a project rule document"
            ))
        );
        assert!(matches!(
            read_rule_source(&dir, "../AGENTS.md"),
            Err(RuleReadError {
                kind: RuleReadErrorKind::NotARuleDocument,
                ..
            })
        ));
        assert!(matches!(
            read_rule_source(&dir, "/etc/passwd"),
            Err(RuleReadError {
                kind: RuleReadErrorKind::NotARuleDocument,
                ..
            })
        ));
        assert!(matches!(
            read_rule_source(&dir, "missing/AGENTS.md"),
            Ok(RuleSourceRead::Absent)
        ));
        let RuleSourceRead::Loaded(rule) =
            read_rule_source(&dir, "src/AGENTS.md").expect("nested rules")
        else {
            panic!("expected a loaded rule");
        };
        assert!(rule.content.contains("Src rule"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn empty_when_no_rules() {
        let dir = temp_dir("none");
        assert!(load_rules(&dir).documents.is_empty());
        assert!(load_rules(&dir).errors.is_empty());
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

    #[test]
    fn an_invalid_utf8_rule_is_a_diagnostic_not_an_absent_rule() {
        let dir = temp_dir("invalid-utf8");
        std::fs::write(dir.join("AGENTS.md"), [0xff, 0xfe, 0x00, 0x41]).unwrap();

        let loaded = load_rules(&dir);
        assert!(loaded.documents.is_empty());
        assert_eq!(loaded.errors.len(), 1, "{:?}", loaded.errors);
        assert_eq!(loaded.errors[0].source, "AGENTS.md");
        assert_eq!(loaded.errors[0].kind, RuleReadErrorKind::InvalidUtf8);

        let plan = plan_rule_delivery(&dir, &RuleDeliveryPolicy::default());
        assert!(plan.documents.is_empty());
        assert_eq!(plan.report.read_errors, loaded.errors);
        assert_eq!(plan.sources.len(), 1);
        assert_eq!(plan.sources[0].source, "AGENTS.md");
        assert_eq!(plan.sources[0].status, RuleSourceStatus::Unreadable);
        assert!(plan.sources[0].sha256.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_rule_filename_is_diagnosed_without_an_absolute_path() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        let dir = temp_dir("non-utf8-name");
        let rules = dir.join(".leveler/rules");
        std::fs::create_dir_all(&rules).unwrap();
        let name = OsString::from_vec(b"bad-\xff.md".to_vec());
        if std::fs::write(rules.join(&name), "# Bad\nx\n").is_err() {
            // APFS rejects non-UTF-8 file names outright, so this failure
            // mode can only be exercised on filesystems that allow them.
            eprintln!("skipping: this filesystem rejects non-UTF-8 file names");
            std::fs::remove_dir_all(&dir).ok();
            return;
        }

        let loaded = load_rules(&dir);
        assert!(loaded.documents.is_empty());
        assert_eq!(loaded.errors.len(), 1, "{:?}", loaded.errors);
        assert_eq!(loaded.errors[0].kind, RuleReadErrorKind::InvalidUtf8);
        assert!(
            loaded.errors[0].source.starts_with(".leveler/rules/"),
            "the source must stay workspace-relative: {}",
            loaded.errors[0].source
        );
        assert!(
            !loaded.errors[0].source.contains(&*dir.to_string_lossy()),
            "an ordinary diagnostic must not carry the absolute workspace path: {}",
            loaded.errors[0].source
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_rule_path_that_is_a_directory_is_a_diagnostic() {
        let dir = temp_dir("rule-directory");
        std::fs::create_dir_all(dir.join("AGENTS.md")).unwrap();

        let loaded = load_rules(&dir);
        assert!(loaded.documents.is_empty());
        assert_eq!(loaded.errors.len(), 1, "{:?}", loaded.errors);
        assert_eq!(loaded.errors[0].kind, RuleReadErrorKind::NotAFile);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_empty_rule_is_distinct_from_a_missing_one() {
        let dir = temp_dir("empty-rule");
        std::fs::write(dir.join("AGENTS.md"), " \n\t\n").unwrap();
        let loaded = load_rules(&dir);
        assert!(loaded.documents.is_empty());
        assert!(loaded.errors.is_empty());
        assert_eq!(loaded.empty, vec!["AGENTS.md".to_string()]);
        let plan = plan_rule_delivery(&dir, &RuleDeliveryPolicy::default());
        assert_eq!(plan.sources[0].status, RuleSourceStatus::Empty);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_rule_file_is_a_diagnostic_not_an_empty_rule_set() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("unreadable");
        let path = dir.join("AGENTS.md");
        std::fs::write(&path, "secret rule").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&path).is_ok() {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).ok();
            std::fs::remove_dir_all(&dir).ok();
            eprintln!("skipping: this process can read mode-000 files (running as root?)");
            return;
        }

        let loaded = load_rules(&dir);
        assert!(loaded.documents.is_empty());
        assert_eq!(loaded.errors.len(), 1, "{:?}", loaded.errors);
        assert_eq!(loaded.errors[0].kind, RuleReadErrorKind::PermissionDenied);

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).ok();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_rules_directory_is_a_diagnostic() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("unreadable-dir");
        let rules = dir.join(".leveler/rules");
        std::fs::create_dir_all(&rules).unwrap();
        std::fs::write(rules.join("style.md"), "# Style\nbe kind\n").unwrap();
        std::fs::set_permissions(&rules, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read_dir(&rules).is_ok() {
            std::fs::set_permissions(&rules, std::fs::Permissions::from_mode(0o755)).ok();
            std::fs::remove_dir_all(&dir).ok();
            eprintln!("skipping: this process can read mode-000 directories (running as root?)");
            return;
        }

        let loaded = load_rules(&dir);
        assert!(loaded.documents.is_empty());
        assert_eq!(loaded.errors.len(), 1, "{:?}", loaded.errors);
        assert_eq!(loaded.errors[0].source, ".leveler/rules");
        assert_eq!(loaded.errors[0].kind, RuleReadErrorKind::PermissionDenied);

        std::fs::set_permissions(&rules, std::fs::Permissions::from_mode(0o755)).ok();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_rule_outside_the_workspace_is_refused() {
        use std::os::unix::fs::symlink;
        let dir = temp_dir("symlink-escape");
        let outside = temp_dir("symlink-escape-outside");
        std::fs::write(outside.join("AGENTS.md"), "OUTSIDE-SECRET").unwrap();
        symlink(outside.join("AGENTS.md"), dir.join("AGENTS.md")).unwrap();

        let loaded = load_rules(&dir);
        assert!(loaded.documents.is_empty(), "{loaded:?}");
        assert_eq!(loaded.errors.len(), 1, "{:?}", loaded.errors);
        assert_eq!(loaded.errors[0].kind, RuleReadErrorKind::OutsideWorkspace);

        // A nested rule has the same boundary as the root one.
        std::fs::create_dir_all(dir.join("src")).unwrap();
        symlink(outside.join("AGENTS.md"), dir.join("src/AGENTS.md")).unwrap();
        let scoped = scoped_rule_sources(&dir, &["src/lib.rs".to_string()]);
        assert!(scoped.documents.is_empty(), "{scoped:?}");
        assert_eq!(scoped.errors.len(), 1, "{:?}", scoped.errors);
        assert_eq!(scoped.errors[0].source, "src/AGENTS.md");
        assert_eq!(scoped.errors[0].kind, RuleReadErrorKind::OutsideWorkspace);

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&outside).ok();
    }

    #[cfg(unix)]
    #[test]
    fn a_sibling_directory_sharing_the_root_prefix_is_still_outside() {
        use std::os::unix::fs::symlink;
        let dir = temp_dir("symlink-prefix");
        let sibling = PathBuf::from(format!("{}-evil", dir.display()));
        std::fs::remove_dir_all(&sibling).ok();
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::write(sibling.join("AGENTS.md"), "PREFIX-EVIL").unwrap();
        symlink(sibling.join("AGENTS.md"), dir.join("AGENTS.md")).unwrap();

        let loaded = load_rules(&dir);
        assert!(loaded.documents.is_empty(), "{loaded:?}");
        assert_eq!(loaded.errors.len(), 1, "{:?}", loaded.errors);
        assert_eq!(loaded.errors[0].kind, RuleReadErrorKind::OutsideWorkspace);

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&sibling).ok();
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_that_stays_inside_the_workspace_is_a_valid_rule() {
        use std::os::unix::fs::symlink;
        let dir = temp_dir("symlink-inside");
        std::fs::create_dir_all(dir.join("docs")).unwrap();
        std::fs::write(dir.join("docs/style.md"), "# Style\nINSIDE-RULE\n").unwrap();
        symlink("docs/style.md", dir.join("AGENTS.md")).unwrap();

        let loaded = load_rules(&dir);
        assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
        assert_eq!(loaded.documents.len(), 1);
        assert_eq!(loaded.documents[0].source, "AGENTS.md");
        assert!(loaded.documents[0].content.contains("INSIDE-RULE"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn heading_like_text_inside_a_fence_is_not_a_section_boundary() {
        let doc = "# Real\nbody\n```md\n# fake heading\n```\n# After\nx\n";
        let sections = split_rule_sections(doc);
        assert_eq!(
            sections.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            vec!["real", "after"]
        );
        assert_eq!(
            sections
                .iter()
                .map(|s| s.content.as_str())
                .collect::<String>(),
            doc
        );
    }

    #[test]
    fn fences_need_matching_markers_and_a_long_enough_close() {
        let doc = "# A\na\n````\n# literal\n```\n# still literal\n````\n# B\nb\n";
        let sections = split_rule_sections(doc);
        assert_eq!(
            sections.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        let tildes = "# A\na\n~~~\n# literal\n~~~\n# B\nb\n";
        assert_eq!(
            split_rule_sections(tildes)
                .iter()
                .map(|s| s.id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }

    #[test]
    fn an_indented_hash_is_body_text_not_a_heading() {
        let doc = "# Real\nbody\n    # code comment\n# After\nx\n";
        assert_eq!(
            split_rule_sections(doc)
                .iter()
                .map(|s| s.id.as_str())
                .collect::<Vec<_>>(),
            vec!["real", "after"]
        );
    }

    #[test]
    fn a_directory_path_scopes_its_own_agents_md() {
        let dir = temp_dir("scoped-directory");
        std::fs::create_dir_all(dir.join("src/npc")).unwrap();
        std::fs::write(dir.join("src/AGENTS.md"), "Src rule.").unwrap();
        std::fs::write(dir.join("src/npc/AGENTS.md"), "Npc rule.").unwrap();

        let scoped = scoped_rule_sources(&dir, &["src/npc".to_string()]);
        assert!(scoped.errors.is_empty(), "{:?}", scoped.errors);
        assert_eq!(
            scoped
                .documents
                .iter()
                .map(|rule| rule.source.as_str())
                .collect::<Vec<_>>(),
            vec!["src/AGENTS.md", "src/npc/AGENTS.md"]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_partially_delivered_source_is_reported_as_partial() {
        let doc = format!(
            "# Keep\nshort\n# Drop\n{}\n",
            "ordinary prose. ".repeat(500)
        );
        let policy = RuleDeliveryPolicy {
            always: vec!["Keep".to_string()],
            budget_bytes: Some(1_200),
        };
        let plan = plan_instructions(vec![instruction("AGENTS.md", &doc)], &policy);
        assert_eq!(plan.sources[0].status, RuleSourceStatus::PartiallyDelivered);
        assert_eq!(plan.report.delivered_sections, vec!["AGENTS.md#keep"]);
        assert_eq!(plan.report.omitted_sections, vec!["AGENTS.md#drop"]);
    }
}
