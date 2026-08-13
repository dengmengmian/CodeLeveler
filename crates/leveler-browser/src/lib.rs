//! Structured browser automation for CodeLeveler.
//!
//! This crate owns the **browser domain** — the typed values and (in later
//! phases) the `BrowserRuntime` that manages a driver subprocess, an isolated
//! per-project profile, semantic snapshots, and safe element refs. The engine
//! (Playwright, driven over structured IPC) is only an implementation detail
//! behind this boundary; nothing outside sees raw protocol frames.
//!
//! Design invariants (see `docs/BROWSER_CAPABILITY_ANALYSIS.md`):
//! - Semantic snapshot + safe refs are the primary control protocol; a ref
//!   never silently retargets a lookalike element ([`BrowserError::RefStale`]).
//! - The runtime is owned by the daemon, survives client disconnect, and lives
//!   under the canonical `~/.leveler` layout — never the workspace, never the
//!   user's real Chrome profile.
//! - `serde_json::Value` is confined to the driver transport; the domain is
//!   typed.

#![forbid(unsafe_code)]

mod error;
mod types;

pub use error::{BrowserError, BrowserResult};
pub use types::{
    BrowserActionResult, BrowserEngine, BrowserNode, BrowserNodeState, BrowserPageId, BrowserRef,
    BrowserRuntimeInfo, BrowserRuntimeStatus, BrowserSessionId, BrowserSnapshot, DialogInfo,
    TabInfo,
};
