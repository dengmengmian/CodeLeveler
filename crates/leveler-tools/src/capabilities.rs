//! The capability handles a tool surface is composed from.
//!
//! Held by the composition root, consumed ONCE by
//! [`crate::registry::model_surface`], and NOT reachable from a running tool:
//! each tool is constructed with exactly the handles it uses.
//!
//! This replaced `ToolContext::services`, where the language servers, the
//! browser runtime, the memory root, the artifact store and the background
//! task registry all sat in one struct that every tool received on every call.
//! `read_file` could reach the browser and `grep` could start a language
//! server — not because either needed to, but because the context was a
//! service locator and a locator answers everyone.
//!
//! A `None` handle means this host cannot provide that capability. It is the
//! same mechanical fact the composer's [`crate::CapabilityPacks`] carries, and
//! the pack flag is what decides whether the tools are registered at all; the
//! handle is what they are built from once they are.

use std::path::PathBuf;
use std::sync::Arc;

use leveler_execution::{ArtifactStore, BackgroundTaskRegistry};

/// The optional long-lived services the built-in tools are constructed with.
#[derive(Clone)]
pub struct Capabilities {
    /// Language-server sessions, shared so servers index the workspace once
    /// (the code-intelligence pack).
    pub lsp: Arc<leveler_lsp::LspSessions>,
    /// The background process registry: the lifecycle `run_command(background)`
    /// creates, and what `get_task` / `wait_task` / `kill_task` manage. Not
    /// optional — a background task nobody can observe or stop is an orphan.
    pub background_tasks: Arc<BackgroundTaskRegistry>,
    /// Where oversized command output is spilled (content-addressed) instead
    /// of being truncated. `None` truncates with a marker.
    pub artifact_store: Option<Arc<ArtifactStore>>,
    /// Durable project memory root. `None` means memory is unconfigured, and
    /// the memory tools say so rather than inventing a location.
    pub memory_root: Option<PathBuf>,
    /// The daemon-owned browser runtime, shared across turns so the browser
    /// and its isolated profile survive client disconnect. `None` disables the
    /// browser tools (they error clearly).
    pub browser: Option<Arc<leveler_browser::BrowserRuntime>>,
    /// The search provider key `web_search` is constructed with. `None` is the
    /// same mechanical fact as `CapabilityPacks::web_search == false`; the host
    /// answers it once and both follow from that answer.
    pub search_api_key: Option<String>,
}

impl Capabilities {
    /// Handles that need nothing from a host: a fresh language-server pool and
    /// a fresh background registry over `environment`, no store, no memory
    /// root, no browser.
    ///
    /// A production host builds on top of this with the services it owns
    /// (see `leveler-app`); an eval or one-shot CLI run uses it as-is.
    pub fn in_process(environment: Arc<leveler_core::EnvSnapshot>) -> Self {
        Self {
            lsp: Arc::new(leveler_lsp::LspSessions::new(environment.clone())),
            background_tasks: Arc::new(BackgroundTaskRegistry::with_environment(environment)),
            artifact_store: None,
            memory_root: None,
            browser: None,
            search_api_key: None,
        }
    }

    /// Spill oversized command output to `store` instead of truncating it.
    pub fn with_artifact_store(mut self, store: Arc<ArtifactStore>) -> Self {
        self.artifact_store = Some(store);
        self
    }

    /// The project memory store directory (`active/` + `archive/`).
    pub fn with_memory_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.memory_root = Some(root.into());
        self
    }

    /// The search provider key `web_search` is built with. A blank value is
    /// not a configuration: it is normalised to `None` here so availability and
    /// construction cannot disagree.
    pub fn with_search_api_key(mut self, key: Option<String>) -> Self {
        self.search_api_key = key.map(|k| k.trim().to_string()).filter(|k| !k.is_empty());
        self
    }

    /// Reuse a process-lived background registry, so tasks survive the turn
    /// that started them.
    pub fn with_background_tasks(mut self, registry: Arc<BackgroundTaskRegistry>) -> Self {
        self.background_tasks = registry;
        self
    }

    /// Share the daemon-owned browser runtime.
    pub fn with_browser(mut self, runtime: Arc<leveler_browser::BrowserRuntime>) -> Self {
        self.browser = Some(runtime);
        self
    }
}
