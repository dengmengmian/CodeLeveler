//! `leveler-project` — project detection and filesystem layout.
//!
//! Identifies project languages (Rust, Go,
//! TypeScript) from marker files, and resolve where CodeLeveler keeps its config
//! and state. Full build-system/test-command inference is a later phase.
#![forbid(unsafe_code)]

pub mod config;
pub mod detect;
pub mod layout;

pub use config::{CommandSpec, ProjectConfig, RunLimitsConfig, VerifySpec};
pub use detect::{Language, detect_languages};
pub use layout::{Layout, legacy_repo_state_paths, migrate_legacy_repo_state};
