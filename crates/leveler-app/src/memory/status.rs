//! Read-only memory status for the CLI, slash commands and the TUI.
//!
//! Every value here comes from the durable stores or the runtime's own worker
//! slots; nothing is inferred from prompt text or model claims. The status is
//! assembled on a blocking thread because it opens each scope's SQLite state.

use std::path::PathBuf;

use leveler_core::SessionId;
use leveler_memory::config::MemoryRollout;
use leveler_memory::{CaptureWorkState, MaintenanceStore, MemoryScope, MemoryScopeStatus};

use super::MemoryRuntime;

/// What the memory pipeline is doing for one session right now.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MemoryActivityKind {
    #[default]
    Idle,
    Capturing,
    Dreaming,
    Failed,
}

impl MemoryActivityKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Capturing => "capturing",
            Self::Dreaming => "dreaming",
            Self::Failed => "failed",
        }
    }
}

/// Content-free progress the UI may show. Never includes observation text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryActivity {
    pub kind: MemoryActivityKind,
    /// A worker of this kind is running right now. `kind` records the last
    /// activity even after it finishes, so the UI can say what happened.
    pub active: bool,
    pub last_error: Option<String>,
    pub last_captured_range: Option<(u32, u32)>,
    pub last_dream_at: Option<i64>,
}

impl MemoryActivity {
    pub(crate) fn merge(&mut self, update: Self) {
        if update.kind != MemoryActivityKind::Idle {
            self.kind = update.kind;
        }
        self.active = update.active;
        if update.last_error.is_some() {
            self.last_error = update.last_error;
        }
        if update.last_captured_range.is_some() {
            self.last_captured_range = update.last_captured_range;
        }
        if update.last_dream_at.is_some() {
            self.last_dream_at = update.last_dream_at;
        }
    }
}

/// One scope's durable state plus the files a user may browse.
#[derive(Debug, Clone)]
pub struct MemoryScopeSummary {
    pub root: PathBuf,
    pub files: Vec<PathBuf>,
    pub state: Option<MemoryScopeStatus>,
    /// The read failed. A failure is shown, never rendered as an empty scope.
    pub error: Option<String>,
}

/// Everything the memory inspection surfaces may display.
#[derive(Debug, Clone)]
pub struct MemoryStatus {
    /// The configured rollout. A session pause narrows the effective rollout
    /// to `off` but does not rewrite this value.
    pub rollout: MemoryRollout,
    /// Memory is on for this session after the page toggle.
    pub enabled: bool,
    pub capture_enabled: bool,
    pub automatic_dream_enabled: bool,
    pub manual_dream_enabled: bool,
    pub batch_dream_enabled: bool,
    pub workspace_ephemeral: bool,
    pub global: MemoryScopeSummary,
    pub workspace: MemoryScopeSummary,
    pub session: Option<SessionMemoryStatus>,
    pub activity: MemoryActivity,
}

/// One session's capture cursors and queued work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMemoryStatus {
    pub session_id: String,
    pub requested: u32,
    pub captured: u32,
    pub indexed: u32,
    pub work: CaptureWorkState,
}

impl MemoryStatus {
    /// The scope the automatic pipeline writes: workspace, unless the process
    /// serves no workspace, in which case the global scope is the only one.
    pub fn primary_scope(&self) -> &MemoryScopeSummary {
        if self.workspace_ephemeral {
            &self.global
        } else {
            &self.workspace
        }
    }
}

impl MemoryRuntime {
    /// Read the current status. `session` adds that session's cursors.
    ///
    /// A configured `off` rollout touches nothing on disk: opening a scope
    /// would create its directory and state database, and a default
    /// installation must stay exactly as it was until the user opts in. A
    /// session pause is not that case: it stops new capture and injection,
    /// but the files the user already has are still read and shown.
    pub async fn status(&self, session: Option<&SessionId>) -> MemoryStatus {
        let configured = self.config();
        let config = self.config_for(session.map(SessionId::as_str));
        if configured.rollout == MemoryRollout::Off {
            let ephemeral = self.storage.is_ephemeral();
            return MemoryStatus {
                rollout: configured.rollout,
                enabled: false,
                // A temporary working directory has no workspace scope, so
                // every workspace-targeted operation is unavailable even
                // though the configuration permits it in general.
                capture_enabled: config.can_capture() && !ephemeral,
                automatic_dream_enabled: config.can_run_automatic_dream() && !ephemeral,
                manual_dream_enabled: config.can_run_manual_dream() && !ephemeral,
                batch_dream_enabled: config.batch_dream_enabled && !ephemeral,
                workspace_ephemeral: ephemeral,
                global: unopened_scope(self.storage.global_dir().to_path_buf()),
                workspace: unopened_scope(self.storage.workspace_dir().to_path_buf()),
                session: session.map(|session| SessionMemoryStatus {
                    session_id: session.as_str().to_owned(),
                    requested: 0,
                    captured: 0,
                    indexed: 0,
                    work: CaptureWorkState::default(),
                }),
                activity: session
                    .map(|session| self.activity(session.as_str()))
                    .unwrap_or_default(),
            };
        }
        let storage = self.storage.clone();
        let session_id = session.cloned();
        let scanned = tokio::task::spawn_blocking(move || {
            storage
                .ensure_initialized()
                .map_err(|error| error.to_string())?;
            let files = storage
                .list_memory_files()
                .map_err(|error| error.to_string())?;
            let global_dir = storage.global_dir().to_path_buf();
            let workspace_dir = storage.workspace_dir().to_path_buf();
            let global = read_scope(
                &global_dir,
                MemoryScope::Global,
                storage.global_dir(),
                storage.workspace_dir(),
                &files,
            );
            let workspace = if storage.is_ephemeral() {
                MemoryScopeSummary {
                    root: workspace_dir.clone(),
                    files: Vec::new(),
                    state: None,
                    error: None,
                }
            } else {
                read_scope(
                    &workspace_dir,
                    MemoryScope::Workspace,
                    storage.global_dir(),
                    storage.workspace_dir(),
                    &files,
                )
            };
            let session = session_id.map(|session_id| {
                read_session(&workspace_dir, storage.is_ephemeral(), &session_id)
            });
            Ok::<_, String>((global, workspace, session))
        })
        .await;
        let (global, workspace, session) = match scanned {
            Ok(Ok(value)) => value,
            Ok(Err(error)) => (
                failed_scope(self.storage.global_dir().to_path_buf(), error.clone()),
                failed_scope(self.storage.workspace_dir().to_path_buf(), error),
                None,
            ),
            Err(error) => {
                let message = format!("memory status task failed: {error}");
                (
                    failed_scope(self.storage.global_dir().to_path_buf(), message.clone()),
                    failed_scope(self.storage.workspace_dir().to_path_buf(), message),
                    None,
                )
            }
        };
        let activity = session
            .as_ref()
            .map(|session| self.activity(&session.session_id))
            .unwrap_or_default();
        MemoryStatus {
            rollout: configured.rollout,
            enabled: config.rollout != MemoryRollout::Off,
            capture_enabled: config.can_capture() && !self.storage.is_ephemeral(),
            automatic_dream_enabled: config.can_run_automatic_dream()
                && !self.storage.is_ephemeral(),
            manual_dream_enabled: config.can_run_manual_dream() && !self.storage.is_ephemeral(),
            batch_dream_enabled: config.batch_dream_enabled && !self.storage.is_ephemeral(),
            workspace_ephemeral: self.storage.is_ephemeral(),
            global,
            workspace,
            session,
            activity,
        }
    }
}

fn read_scope(
    scope_dir: &std::path::Path,
    scope: MemoryScope,
    global_dir: &std::path::Path,
    workspace_dir: &std::path::Path,
    files: &[PathBuf],
) -> MemoryScopeSummary {
    let state = MaintenanceStore::open(scope_dir, scope, global_dir, workspace_dir)
        .and_then(|store| store.status(leveler_core::now().timestamp()))
        .map_err(|error| error.to_string());
    MemoryScopeSummary {
        root: scope_dir.to_path_buf(),
        files: files
            .iter()
            .filter(|path| path.starts_with(scope_dir))
            .cloned()
            .collect(),
        state: state.as_ref().ok().cloned(),
        error: state.err(),
    }
}

fn read_session(
    workspace_dir: &std::path::Path,
    ephemeral: bool,
    session_id: &SessionId,
) -> SessionMemoryStatus {
    let mut status = SessionMemoryStatus {
        session_id: session_id.as_str().to_owned(),
        requested: 0,
        captured: 0,
        indexed: 0,
        work: CaptureWorkState::default(),
    };
    if ephemeral {
        return status;
    }
    let Ok(store) = leveler_memory::CaptureStore::open(workspace_dir, MemoryScope::Workspace)
    else {
        return status;
    };
    if let Ok(cursors) = store.cursors(session_id.as_str()) {
        status.requested = cursors.requested;
        status.captured = cursors.captured;
        status.indexed = cursors.indexed;
    }
    if let Ok(work) = store.work_state(session_id.as_str(), status.requested) {
        status.work = work;
    }
    status
}

fn failed_scope(root: PathBuf, error: String) -> MemoryScopeSummary {
    MemoryScopeSummary {
        root,
        files: Vec::new(),
        state: None,
        error: Some(error),
    }
}

/// A scope the `off` rollout deliberately never opened.
fn unopened_scope(root: PathBuf) -> MemoryScopeSummary {
    MemoryScopeSummary {
        root,
        files: Vec::new(),
        state: None,
        error: None,
    }
}
