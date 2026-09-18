//! Self-update for CodeLeveler.
//!
//! GitHub Releases are the only update source. This crate owns the whole
//! semantics of an update — version ordering, asset selection, checksum
//! verification, installation — and exposes it as one service the CLI, the
//! start-up path, and the TUI all share. It knows nothing about terminals,
//! config files, or argument parsing.
//!
//! It never restarts the process itself: the caller restores the terminal and
//! then calls [`restart`], so a TUI upgrade does not inherit the alternate
//! screen.

pub mod error;
pub mod install;
pub mod policy;
pub mod restart;
pub mod service;
pub mod source;
pub mod state;
pub mod target;
pub mod version;

pub use error::UpdateError;
pub use policy::UpdatePolicy;
pub use restart::{restart, restart_with};
pub use service::{UpdateOutcome, UpdateService, UpdateStep};
pub use source::{
    Asset, GitHubReleaseSource, Release, ReleaseAssets, ReleaseSource, github_repo, select_asset,
};
pub use state::UpdateState;
pub use target::{host_target_triple, release_asset_name};
pub use version::{Version, current_version, parse_version, should_upgrade};
