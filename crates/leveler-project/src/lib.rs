//! `leveler-project` — project detection and filesystem layout.
//!
//! Identifies project languages (Rust, Go,
//! TypeScript) from marker files, and resolve where CodeLeveler keeps its config
//! and state. Full build-system/test-command inference is a later phase.
#![forbid(unsafe_code)]

pub mod config;
pub mod detect;
pub mod hygiene;
pub mod layout;
pub mod legacy_memory_purge;

pub use config::{
    CommandSpec, IndependentReview, MemoryConfig, ProjectConfig, RulesConfig, RunLimitsConfig,
    VerifySpec,
};
pub use detect::{Language, detect_languages};
pub use layout::{EphemeralHome, Layout, NoWorkspace};
pub use legacy_memory_purge::{PurgeReport, StoreReport, StoreVerdict, purge};
