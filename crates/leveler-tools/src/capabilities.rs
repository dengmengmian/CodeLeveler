//! The capability handles a tool surface is composed from.
//!
//! Held by the composition root, consumed ONCE by
//! [`crate::registry::model_surface`], and NOT reachable from a running tool:
//! each tool is constructed with exactly the handles it uses.
//!
//! This replaced `ToolContext::services`, where the language servers, the
//! browser runtime, the artifact store and the background
//! task registry all sat in one struct that every tool received on every call.
//! `read_file` could reach the browser and `grep` could start a language
//! server — not because either needed to, but because the context was a
//! service locator and a locator answers everyone.
//!
//! A `None` handle means this host cannot provide that capability. It is the
//! same mechanical fact the composer's [`crate::CapabilityPacks`] carries, and
//! the pack flag is what decides whether the tools are registered at all; the
//! handle is what they are built from once they are.

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

    /// The daemon-owned browser, shared across turns so its tabs, cookies and
    /// login state survive client disconnect. `None` is the same mechanical
    /// fact as `CapabilityPacks::browser == false`: no browser is drivable
    /// here, so the tools are not built.
    pub browser: Option<Arc<leveler_browser::Browser>>,
    /// The search backend `web_search` is constructed with. `None` is the same
    /// mechanical fact as `CapabilityPacks::web_search == false`; the host
    /// answers "is a provider configured" once and both follow from that
    /// answer. The provider owns its credential, endpoint and request shape.
    pub search_provider: Option<Arc<dyn crate::search::SearchProvider>>,
    /// The process-lived native desktop backend. `None` is the same mechanical
    /// fact as `CapabilityPacks::computer_use == false`. One handle is shared
    /// by every session; per-session observations are separated by the scope
    /// each call carries, because the desktop itself is one OS resource.
    pub computer_use: Option<Arc<leveler_cua::ComputerUse>>,
}

impl Capabilities {
    /// Handles that need nothing from a host: a fresh language-server pool and
    /// a fresh background registry over `environment`, no store, no browser
    /// root, no browser.
    ///
    /// A production host builds on top of this with the services it owns
    /// (see `leveler-app`); an eval or one-shot CLI run uses it as-is.
    pub fn in_process(environment: Arc<leveler_core::EnvSnapshot>) -> Self {
        Self {
            lsp: Arc::new(leveler_lsp::LspSessions::new(environment.clone())),
            background_tasks: Arc::new(BackgroundTaskRegistry::with_environment(environment)),
            artifact_store: None,

            browser: None,
            search_provider: None,
            computer_use: None,
        }
    }

    /// Spill oversized command output to `store` instead of truncating it.
    pub fn with_artifact_store(mut self, store: Arc<ArtifactStore>) -> Self {
        self.artifact_store = Some(store);
        self
    }

    /// The search backend `web_search` is built with. A `web_search` tool
    /// without a provider could only fail, so availability and construction
    /// come from this one handle.
    pub fn with_search_provider(
        mut self,
        provider: Option<Arc<dyn crate::search::SearchProvider>>,
    ) -> Self {
        self.search_provider = provider;
        self
    }

    /// Reuse a process-lived background registry, so tasks survive the turn
    /// that started them.
    pub fn with_background_tasks(mut self, registry: Arc<BackgroundTaskRegistry>) -> Self {
        self.background_tasks = registry;
        self
    }

    /// Share the daemon-owned browser.
    pub fn with_browser(mut self, browser: Arc<leveler_browser::Browser>) -> Self {
        self.browser = Some(browser);
        self
    }

    /// Share the process-lived native desktop backend.
    pub fn with_computer_use(mut self, computer: Arc<leveler_cua::ComputerUse>) -> Self {
        self.computer_use = Some(computer);
        self
    }
}
