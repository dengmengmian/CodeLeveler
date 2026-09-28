//! `leveler-context` — the context compiler (spec §8.7, §26).
//!
//! Assembles a bounded, relevant slice of the repository (map, candidate files,
//! related tests, merged project rules, token estimate) for planning and
//! execution, plus a repeated-read guard. First-stage strategy only — no AST
//! index yet (spec §26.3).
#![forbid(unsafe_code)]

pub mod compaction;
pub mod context;
pub mod guard;
pub mod repo_map;
pub mod rules;
pub mod symbols;

pub use compaction::{
    ACTIVE_OBJECTIVE_MARKER, COMPACT_KEEP_RECENT, FoldRequirement, PRE_REQUEST_COMPACT_THRESHOLD,
    accepted_summary, compact_messages, estimate_tokens, round_boundary, summary_request,
};
pub use context::{ContextCompiler, ContextPackage, estimate_text_tokens};
pub use guard::{ContentFingerprint, FileStateTracker};
pub use repo_map::RepositoryMap;
pub use rules::{
    MAX_RULE_BYTES, ProjectInstruction, READ_PROJECT_RULES_TOOL, RuleDelivery, RuleDeliveryPolicy,
    RuleSection, deliver_document, deliver_sections, deliver_sections_with_policy, load_rules,
    load_rules_for_delivery, load_rules_for_paths, load_scoped_rules, render_instructions,
    render_instructions_with, split_rule_sections,
};
pub use symbols::{defines, extract_symbols};
