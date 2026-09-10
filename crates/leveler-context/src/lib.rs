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
    ACTIVE_OBJECTIVE_MARKER, COMPACT_KEEP_RECENT, CompactionSummary, PRE_REQUEST_COMPACT_THRESHOLD,
    compact_messages, estimate_tokens, summarize_with_model,
};
pub use context::{ContextCompiler, ContextPackage, estimate_text_tokens};
pub use guard::{ContentFingerprint, FileStateTracker};
pub use repo_map::RepositoryMap;
pub use rules::{
    ProjectInstruction, load_rules, load_rules_for_paths, load_scoped_rules, render_instructions,
};
pub use symbols::{defines, extract_symbols};
