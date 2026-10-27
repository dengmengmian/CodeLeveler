//! `leveler-context` — repository context and compaction.
//!
//! What the runtime actually assembles a request from: the repository map, the
//! project rules and their delivery policy, the compaction fold and its budget,
//! plus the repeated-read guard. The old `ContextCompiler` package pipeline
//! (candidate files, symbol scanning, its own token estimate) was removed with
//! the request projection that replaced it — it had no producer and no reader.
#![forbid(unsafe_code)]

pub mod compaction;
pub mod guard;
pub mod repo_map;
pub mod rules;
pub mod symbols;

pub use compaction::{
    ACTIVE_OBJECTIVE_MARKER, COMPACT_KEEP_RECENT, COMPACTION_SUMMARY_BUDGET_TOKENS,
    FoldRequirement, accepted_summary, compact_messages, compaction_span, estimate_tokens,
    objective_pin_message, retention_tail_budget, round_boundary, shrink_externalized_results,
    summary_request,
};
pub use guard::{ContentFingerprint, FileStateTracker};
pub use repo_map::RepositoryMap;
pub use rules::{
    MAX_RULE_BYTES, ProjectInstruction, READ_PROJECT_RULES_TOOL, RuleDeliveryPlan,
    RuleDeliveryPolicy, RuleDeliveryReport, RuleSection, is_rule_document, load_rules,
    plan_instructions, plan_rule_delivery, read_rule_source, render_instructions,
    scoped_rule_sources, split_rule_sections,
};
pub use symbols::{defines, extract_symbols};
