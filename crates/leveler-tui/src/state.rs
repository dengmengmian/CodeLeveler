//! [`AppState`] — the single source of truth the renderer reads and the reducer
//! mutates. Nothing else writes it directly .

use std::collections::{HashMap, VecDeque};

use leveler_client_protocol::{
    AttachmentRef, CommandId, ModelRef, NotificationLevel, PermissionProfile, RuntimeStatus,
    SessionId, UiApprovalRequest, UiCheckpoint, UiClarificationRequest, UiDiff, UiPlan,
    UiSessionSummary, UiVerification,
};

use crate::composer::Composer;
use crate::i18n::{Locale, UiText};
use crate::overlay::Overlay;
use crate::screen::{Screen, ToolsScreenState};
use crate::theme::Theme;
use crate::transcript::TranscriptState;

/// A runtime request that must eventually be answered by the user. Parked in
/// [`AppState::pending_interactions`] while another overlay holds the screen —
/// both kinds block their tool call in the runtime until answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingInteraction {
    Approval(UiApprovalRequest),
    Clarification(UiClarificationRequest),
}

impl PendingInteraction {
    /// Stable map key for sticky command-id retries.
    pub fn request_key(&self) -> String {
        match self {
            Self::Approval(r) => format!("a:{}", r.id.as_str()),
            Self::Clarification(r) => format!("c:{}", r.id.as_str()),
        }
    }
}

/// Static boot info the runtime snapshot does not carry.
#[derive(Debug, Clone)]
pub struct Boot {
    pub session_id: SessionId,
    pub user: String,
    pub version: String,
    /// Whether to show the welcome header for this (new) session.
    pub show_welcome: bool,
    /// Where to persist the composer draft across restarts (spec §24).
    pub draft_path: Option<std::path::PathBuf>,
    /// Where to persist the input history across restarts (JSON array).
    pub history_path: Option<std::path::PathBuf>,
    /// The active model's context window in tokens (for the context gauge).
    pub context_window: u32,
    /// UI language (resolved once at process start).
    pub locale: Locale,
    /// In-repo config files present but ignored for lack of trust, as display
    /// paths. Resolved by the composition root; see [`AppState::untrusted_config`].
    pub untrusted_config: Vec<String>,
}

/// A transient status-line notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub level: NotificationLevel,
    pub message: String,
}

/// Which workbench region owns ↑/↓ and related keys.
///
/// - [`Input`](WorkbenchFocus::Input): history browse, typing
/// - [`Conversation`](WorkbenchFocus::Conversation): viewport scroll
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WorkbenchFocus {
    #[default]
    Input,
    Conversation,
}

/// The `/remote` invite, as the screen shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteState {
    pub invite: crate::action::RemoteInvite,
    /// Set once a phone has claimed the invite and is waiting to be accepted.
    pub pending: Option<crate::action::PairingRequest>,
    /// What happened after the user decided, so the screen can say so.
    pub outcome: Option<String>,
}

/// One memoized conversation build: cache key, wrapped lines, and the
/// disclosure hit rows (absolute line index → transcript item index). The hit
/// rows are display-only — rebuilt with the lines, never persisted.
pub type ConvCacheEntry = (
    crate::workbench::ConvKey,
    std::rc::Rc<Vec<ratatui::text::Line<'static>>>,
    std::rc::Rc<Vec<(usize, usize)>>,
);

/// The whole UI state.
#[derive(Debug)]
pub struct AppState {
    pub running: bool,
    /// False once the event subscription closes. Commands stay disabled until
    /// the user exits and reconnects, avoiding a write-only UI.
    pub runtime_connected: bool,
    pub session_id: SessionId,
    pub transcript: TranscriptState,
    pub composer: Composer,
    pub theme: Theme,
    /// Terminal size (cols, rows).
    pub size: (u16, u16),

    /// The active full-screen view (Conversation by default).
    pub active_screen: Screen,
    pub tools_screen: ToolsScreenState,

    /// Latest plan / verification / diff from the current run, if any.
    pub plan: Option<UiPlan>,
    /// Workspace-relative instruction sources active for the current turn.
    pub project_rule_sources: Vec<String>,
    pub verification: Option<UiVerification>,
    pub diff: Option<UiDiff>,
    pub diff_selected: usize,
    /// Reasoning streamed by the model step currently in flight. A turn runs
    /// many steps; each step's thought stands on its own, so the next step's
    /// first delta replaces this rather than appending to it (see
    /// `reasoning_superseded`).
    pub reasoning: String,
    /// Set once the in-flight step commits to an action (a tool call), which
    /// ends its thought. The thought stays on screen while the tools run; the
    /// next step's first reasoning delta clears it.
    pub reasoning_superseded: bool,
    /// Whether the current busy turn was launched with `/goal`.
    pub goal_mode_active: bool,
    /// Product work profile: economy | balanced | delivery.
    pub work_profile: String,
    /// Collaboration mode: chat | plan | goal.
    pub collaboration: String,

    /// Attachments staged for the next message (spec §40).
    pub pending_attachments: Vec<AttachmentRef>,
    /// Whether the current model accepts images (from the snapshot, spec §42).
    pub vision: bool,

    /// Stored sessions and cursor for the Sessions screen (spec §52).
    pub sessions: Vec<UiSessionSummary>,
    pub sessions_selected: usize,
    /// Context package info from the last run (spec §53).
    pub context_files: Vec<String>,
    pub context_tokens: u32,
    /// Latest model-reported input/output tokens for the current conversation.
    pub token_input: u32,
    pub token_output: u32,
    /// Prefix-cache hits within `token_input`, from the last round.
    pub token_cached: u32,
    /// Conversation checkpoints (restore points, spec §68).
    pub checkpoints: Vec<UiCheckpoint>,

    /// The active modal overlay, if any (picker / approval). Captures key input.
    pub overlay: Option<Overlay>,
    /// Approval/clarification requests that arrived while another overlay was
    /// open. They wait here (oldest first) and are promoted as the overlay clears,
    /// so a later request never silently drops an earlier, unanswered one.
    pub pending_interactions: VecDeque<PendingInteraction>,
    /// Sticky `CommandId` per interaction request key (`a:<id>` / `c:<id>`), so
    /// a transport-retry of the same decision reuses the envelope id and hits
    /// runtime command-receipt dedup instead of double-dispatching.
    pub interaction_command_ids: HashMap<String, CommandId>,

    pub status: RuntimeStatus,
    /// Number of tools started in the active turn.
    pub turn_tool_calls: usize,
    /// Coarse activity label shown while busy (e.g. "运行 cargo test").
    pub activity: Option<String>,
    pub notification: Option<Notification>,

    /// The running embedded Web UI URL (with token), once `/web` has started it.
    /// `Some` also guards against launching a second server.
    pub web_url: Option<String>,
    /// True while `/web`'s server is starting (guards against double-launch).
    pub web_starting: bool,
    /// The invite `/remote` produced, and who is waiting on it. Present only
    /// while the invite screen is up.
    pub remote: Option<RemoteState>,

    /// Models the user can switch to, and the current execution mode — used to
    /// build the model/mode pickers.
    pub available_models: Vec<ModelRef>,
    pub mode: PermissionProfile,

    /// Scroll offset (in lines) of the active full-screen view's content.
    pub screen_scroll: usize,
    /// Conversation viewport scroll (line offset from top). Only Conversation scrolls.
    pub conversation_scroll: usize,
    /// When true, stick to the bottom as new activity arrives.
    pub conversation_auto_scroll: bool,
    /// Which region owns arrow keys (Tab toggles).
    pub workbench_focus: WorkbenchFocus,
    /// Content ticks observed while pinned away from bottom (for ▼ N).
    pub conversation_unread: usize,
    /// Last seen conversation line count (to detect growth while scrolled up).
    pub conversation_last_len: usize,
    /// Last painted Conversation rect (x, y, w, h) for mouse hit-testing.
    pub conversation_rect: Option<(u16, u16, u16, u16)>,
    /// Last painted Input/composer rect for click-to-focus.
    pub input_rect: Option<(u16, u16, u16, u16)>,
    /// Last painted scroll-to-bottom button rect, if visible.
    pub scroll_bottom_rect: Option<(u16, u16, u16, u16)>,
    /// Conversation text selection (mouse drag copy).
    pub selection: crate::selection::TextSelection,
    /// Edge auto-scroll while dragging a selection: `-1` up, `0` none, `1` down.
    pub selection_edge_dir: i8,
    /// Consecutive edge-scroll ticks (accelerates step size).
    pub selection_edge_streak: u32,
    /// Last mouse cell while dragging (screen col/row), for remapping after scroll.
    pub selection_last_mouse: Option<(u16, u16)>,
    /// Cached plain-text of conversation lines for the last render width.
    pub conversation_plain: Vec<String>,
    /// Content width used when `conversation_plain` was built.
    pub conversation_plain_width: usize,
    /// Memoized wrapped conversation lines, reused across repaints while no
    /// render input changed (see `AppState::conversation_lines`). Interior
    /// mutability so read-only render/measure paths can populate it.
    pub conversation_cache: std::cell::RefCell<Option<ConvCacheEntry>>,
    /// Plan panel collapsed to a single title row.
    pub plan_collapsed: bool,

    /// Legacy global expand flag — no longer forces every tool group open.
    /// Kept so the workbench can render the currently focused tool group.
    pub tools_expanded: bool,
    /// Whether the live reasoning block is fully expanded (Ctrl+O when
    /// reasoning is the current focus).
    pub reasoning_expanded: bool,
    /// Shift+↑/↓ review index into user turns (`None` = live edge).
    /// Composer draft is never cleared while navigating.
    pub turn_nav: Option<usize>,

    /// Highlighted row in the slash-command completion popup (Up/Down navigate).
    pub slash_selected: usize,
    /// User pressed Esc while the slash popup was open; stay hidden until the
    /// composer text changes (so Esc can actually leave the menu).
    pub slash_popup_dismissed: bool,
    /// Armed by the first `/clear`; a second `/clear` actually wipes context.
    pub clear_confirm_armed: bool,
    /// Repository paths used by `@file` completion.
    pub file_candidates: Vec<String>,
    pub file_index_requested: bool,
    /// Discovered skills as slash entries: `(name, description)`.
    /// Refreshed when the repo changes or the user types `/`.
    pub skill_catalog: Vec<(String, String)>,
    /// Root path the catalog was built from (avoids re-scanning every keystroke).
    pub skill_catalog_root: Option<String>,

    /// Header/welcome metadata, filled from the session snapshot.
    pub repository: String,
    pub branch: Option<String>,
    pub model_label: String,
    pub mode_label: String,
    /// Local wall clock `HH:MM`, refreshed by the event loop.
    pub clock_label: String,
    /// Mutable context window for the active model (updated on model switch).
    pub context_window_tokens: u32,

    /// `Ctrl+X` was pressed and is waiting for the second key of the chord
    /// (`Ctrl+X Ctrl+E` opens `$EDITOR`). Any other key spends it.
    pub editor_chord_armed: bool,

    // Ctrl+C escalation state.
    pub cancel_armed: bool,
    /// Set after ForceCancel was sent while still busy. A further Ctrl+C quits
    /// so a hung turn cannot trap the user in cancel-only key handling.
    pub force_cancel_armed: bool,
    pub quit_armed: bool,

    /// Monotonic frame counter driving the busy spinner animation.
    pub tick: u64,
    /// When the current busy turn began (managed by the event loop).
    pub turn_started_at: Option<std::time::Instant>,
    /// Elapsed seconds of the current busy turn (recomputed each frame).
    pub elapsed_secs: u64,
    /// Whether the dark theme is active (for `/theme` toggling).
    pub dark: bool,

    /// Request the event loop to rebuild the inline view at the live edge
    /// (Approach A: jump back after scrolling terminal history). Consumed once.
    pub jump_to_bottom: bool,

    /// UI language for chrome / help / notifications.
    pub locale: Locale,

    /// In-repo config files (`.leveler/hooks.yaml`, `.leveler/permissions.yaml`)
    /// present but ignored for lack of trust, as repo-relative display paths.
    ///
    /// The CLI prints this on stderr at startup, which the alternate screen
    /// swallows — so the TUI carries it itself, on the splash and on the
    /// composer border, for as long as it is true.
    pub untrusted_config: Vec<String>,

    boot: Boot,
}

impl AppState {
    pub fn new(theme: Theme, boot: Boot) -> Self {
        Self {
            running: true,
            runtime_connected: true,
            session_id: boot.session_id.clone(),
            transcript: TranscriptState::new(),
            composer: Composer::new(),
            theme,
            size: (80, 24),
            active_screen: Screen::default(),
            tools_screen: ToolsScreenState::default(),
            plan: None,
            project_rule_sources: Vec::new(),
            verification: None,
            diff: None,
            diff_selected: 0,
            reasoning: String::new(),
            reasoning_superseded: false,
            goal_mode_active: false,
            work_profile: "balanced".into(),
            collaboration: "chat".into(),
            pending_attachments: Vec::new(),
            vision: false,
            sessions: Vec::new(),
            sessions_selected: 0,
            context_files: Vec::new(),
            context_tokens: 0,
            token_input: 0,
            token_output: 0,
            token_cached: 0,
            checkpoints: Vec::new(),
            overlay: None,
            pending_interactions: VecDeque::new(),
            interaction_command_ids: HashMap::new(),
            status: RuntimeStatus::Idle,
            turn_tool_calls: 0,
            activity: None,
            notification: None,
            web_url: None,
            web_starting: false,
            remote: None,
            available_models: Vec::new(),
            mode: PermissionProfile::Assisted,
            screen_scroll: 0,
            conversation_scroll: 0,
            conversation_auto_scroll: true,
            workbench_focus: WorkbenchFocus::Input,
            conversation_unread: 0,
            conversation_last_len: 0,
            conversation_rect: None,
            input_rect: None,
            scroll_bottom_rect: None,
            selection: crate::selection::TextSelection::default(),
            selection_edge_dir: 0,
            selection_edge_streak: 0,
            selection_last_mouse: None,
            conversation_plain: Vec::new(),
            conversation_plain_width: 0,
            conversation_cache: std::cell::RefCell::new(None),
            plan_collapsed: false,
            tools_expanded: false,
            reasoning_expanded: false,
            turn_nav: None,
            slash_selected: 0,
            slash_popup_dismissed: false,
            clear_confirm_armed: false,
            file_candidates: Vec::new(),
            file_index_requested: false,
            skill_catalog: Vec::new(),
            skill_catalog_root: None,
            repository: String::new(),
            branch: None,
            model_label: "—".to_string(),
            mode_label: "—".to_string(),
            clock_label: String::new(),
            context_window_tokens: boot.context_window,
            editor_chord_armed: false,
            cancel_armed: false,
            force_cancel_armed: false,
            quit_armed: false,
            tick: 0,
            turn_started_at: None,
            elapsed_secs: 0,
            dark: true,
            jump_to_bottom: false,
            locale: boot.locale,
            untrusted_config: boot.untrusted_config.clone(),
            boot,
        }
    }

    /// Localized UI strings for the active locale.
    pub fn t(&self) -> &'static UiText {
        self.locale.text()
    }

    pub fn is_busy(&self) -> bool {
        self.status == RuntimeStatus::Busy
    }

    /// The active model's context window in tokens (0 = unknown).
    pub fn context_window(&self) -> u32 {
        self.context_window_tokens
    }

    pub fn user(&self) -> &str {
        &self.boot.user
    }

    pub fn version(&self) -> &str {
        &self.boot.version
    }

    pub fn show_welcome(&self) -> bool {
        self.boot.show_welcome
    }

    pub fn draft_path(&self) -> Option<&std::path::Path> {
        self.boot.draft_path.as_deref()
    }

    pub fn history_path(&self) -> Option<&std::path::Path> {
        self.boot.history_path.as_deref()
    }

    /// Clear any pending Ctrl+C escalation (any other activity resets it).
    pub fn disarm_ctrlc(&mut self) {
        self.cancel_armed = false;
        self.force_cancel_armed = false;
        self.quit_armed = false;
    }
}
