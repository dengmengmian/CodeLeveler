//! [`AppState`] — the single source of truth the renderer reads and the reducer
//! mutates. Nothing else writes it directly .

use std::collections::{HashMap, VecDeque};

use leveler_client_protocol::{
    AttachmentRef, ClientCommand, CommandId, ModelRef, NotificationLevel, PermissionProfile,
    RuntimeStatus, SessionId, UiApprovalRequest, UiCheckpoint, UiClarificationRequest, UiDiff,
    UiPlan, UiSessionSummary, UiVerification,
};

use crate::composer::Composer;
use crate::i18n::{Locale, UiText};
use crate::overlay::Overlay;
use crate::screen::{Screen, ToolsScreenState};
use crate::theme::Theme;
use crate::transcript::TranscriptState;

/// A turn input (message, steer, `/goal`) the runtime has not answered yet.
///
/// `command_id` belongs to the logical command, not to one transport attempt:
/// every retry re-sends this exact id, and the runtime's durable command
/// receipt makes those retries one dispatch. Only the runtime's answer —
/// delivered or rejected — removes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingSubmission {
    pub command_id: CommandId,
    pub command: ClientCommand,
    /// The first attempt ended without an answer. The event loop keeps
    /// re-delivering the same envelope; until it settles, no new turn input is
    /// sent, because this one may already be running.
    pub unconfirmed: bool,
}

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
    /// Resolved `reasoning_effort` wire value (`max` / `high` / …). `None`
    /// when the runtime has not supplied one — the chip must not invent it.
    pub reasoning_effort: Option<String>,
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
/// - [`Activity`](WorkbenchFocus::Activity): compact activity rows (Enter opens detail)
/// - [`Background`](WorkbenchFocus::Background): the footer's aggregated
///   background summary (Enter opens the background-jobs list)
/// - [`Command`](WorkbenchFocus::Command): the transcript's running command
///   rows (Enter toggles output, `x` stops the focused execution)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WorkbenchFocus {
    #[default]
    Input,
    Conversation,
    Activity,
    /// The 待发送 list above the composer.
    Pending,
    /// A running command row in the transcript holds the keyboard focus.
    Command,
    /// The footer's aggregated background-jobs summary holds the focus.
    Background,
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

/// Background-process chrome derived from `BackgroundTaskStarted`.
///
/// `started_elapsed_secs` is the turn clock when the TUI applied the start
/// event — a projection timestamp, not a process clock, and never refreshed
/// by redraws. A terminal event marks the entry terminal in place so its
/// Activity row and detail stay reopenable; finished entries are bounded by
/// `activity::MAX_TERMINAL_BACKGROUND`. Historical completion also belongs to
/// the transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundTaskChrome {
    pub label: String,
    pub started_elapsed_secs: u64,
    /// `None` while Running. `Some(true)` completed ok; `Some(false)` failed or
    /// was stopped — read [`Self::stopped`] to tell those apart.
    pub ok: Option<bool>,
    /// The runtime's authoritative `Killed` terminal state: a user/agent cancel
    /// or session cleanup. A stopped task is not a failure, whatever its exit
    /// code. Never inferred from `exit_code` in the presentation layer.
    pub stopped: bool,
    pub exit_code: Option<i32>,
    pub duration_ms: Option<u64>,
    /// Output retained from authoritative events. Empty means none arrived.
    pub output: String,
}

impl BackgroundTaskChrome {
    pub fn running(label: impl Into<String>, started_elapsed_secs: u64) -> Self {
        Self {
            label: label.into(),
            started_elapsed_secs,
            ok: None,
            stopped: false,
            exit_code: None,
            duration_ms: None,
            output: String::new(),
        }
    }

    pub fn is_running(&self) -> bool {
        self.ok.is_none()
    }

    /// Terminal and unsuccessful because the work itself failed — not because
    /// it was stopped. This is the only state that counts toward the failure
    /// badge.
    pub fn is_failed(&self) -> bool {
        self.ok == Some(false) && !self.stopped
    }
}

/// A task's terminal outcome, projected from the runtime facts the chrome
/// carries. The single place the UI reads "how did it end".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundOutcome {
    Running,
    Completed,
    Failed,
    Stopped,
}

impl BackgroundTaskChrome {
    pub fn outcome(&self) -> BackgroundOutcome {
        match self.ok {
            None => BackgroundOutcome::Running,
            Some(true) => BackgroundOutcome::Completed,
            Some(false) if self.stopped => BackgroundOutcome::Stopped,
            Some(false) => BackgroundOutcome::Failed,
        }
    }
}

/// An active model-round retry, held while the runtime waits out its backoff.
///
/// The runtime owns the decision and the delay; this is only the live view of
/// it. `retry_at` is the wall-clock instant the next attempt will start, so the
/// status line can count down to the ACTUAL delay the scheduler is using —
/// never a second, guessed number.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reconnecting {
    /// The retry about to happen (1-based).
    pub attempt: u32,
    pub max_attempts: u32,
    /// The backoff the runtime announced for this retry.
    pub delay: std::time::Duration,
    /// When the next attempt starts (`now + delay` at event time).
    pub retry_at: std::time::Instant,
}

impl Reconnecting {
    /// Whole seconds until the next attempt starts, saturating at zero.
    pub fn remaining_secs(&self) -> u64 {
        self.retry_at
            .saturating_duration_since(std::time::Instant::now())
            .as_secs()
    }
}

/// How long the brief "Reconnected" confirmation owns the status line after a
/// retry attempt starts again.
pub const RECONNECTED_NOTICE: std::time::Duration = std::time::Duration::from_secs(3);

/// The whole UI state.
#[derive(Debug)]
pub struct AppState {
    pub running: bool,
    /// False once the event subscription closes. Commands stay disabled until
    /// the user exits and reconnects, avoiding a write-only UI.
    pub runtime_connected: bool,
    pub session_id: SessionId,
    pub transcript: TranscriptState,
    /// Which conversation surface owns the viewport and the composer. One
    /// field decides both, so they cannot disagree about who is focused.
    pub surface: crate::btw::SurfaceFocus,
    /// The `/btw` side thread: its own conversation, draft and generation
    /// state. Never part of the main transcript, and never merged into the
    /// main run's context.
    pub btw: crate::btw::BtwThread,
    /// Live raw-reasoning scratch. Feeds ONLY the status line's thinking
    /// indicator/token estimate — never rendered into the conversation, never
    /// persisted, cleared at every segment boundary (tool start, assistant
    /// start, turn end). Raw reasoning is not conversation content, and there
    /// is deliberately no historical representation of it at all.
    pub live_reasoning: String,
    /// Background task id → chrome, remembered from the start event so the
    /// status line can name what is running and its exit can name what
    /// finished. The exit event carries only the id. A terminal entry is kept
    /// until the next turn boundary so its Activity row and detail stay
    /// reopenable; running entries are pruned when the authoritative active
    /// set excludes them, and the whole map is cleared on session switch.
    /// Presentation-only; the runtime remains the lifecycle authority.
    pub background_task_labels: HashMap<String, BackgroundTaskChrome>,
    /// Multi-agent view model: what each child is for and what the parent did
    /// with what it produced. Built from events, never from prose.
    pub team: crate::multi_agent::TaskTeamView,
    /// Goals that still owe work, loaded on request (long-goal P2).
    ///
    /// Read-only. There is no action attached and no command to attach one to:
    /// resume is a policy the runtime has not decided.
    pub unfinished_goals: Vec<leveler_client_protocol::UiUnfinishedGoal>,
    pub composer: Composer,
    /// The structured next step offered as ghost text after a turn ends, or
    /// `None`. Presentation state only: see [`crate::suggestion`] — it is not
    /// composer content, not a draft, not history, and never submitted on its
    /// own. Ephemeral and never persisted.
    pub prompt_suggestion: Option<String>,
    pub theme: Theme,
    /// Terminal size (cols, rows).
    pub size: (u16, u16),

    /// The active full-screen view (Conversation by default).
    pub active_screen: Screen,
    pub tools_screen: ToolsScreenState,
    pub trace: crate::observability::TraceView,
    /// `/context` inspector state: the latest runtime accounting snapshot and
    /// presentation-only disclosure/selection.
    pub context: crate::context::ContextView,
    /// `/clean` page state: scan/cleanup stage, the plan, and the result.
    /// Presentation-only; the CLI host owns the scan and the deletion.
    pub clean: crate::clean::CleanState,

    /// Active plan from the current run, if any. Terminal plans move into the
    /// transcript and never remain in this slot.
    pub plan: Option<UiPlan>,
    /// Workspace-relative instruction sources active for the current turn.
    pub project_rule_sources: Vec<String>,
    pub verification: Option<UiVerification>,
    /// The verification the current turn produced. `verification` stays the
    /// latest result for the verification screen; a turn-end summary only
    /// speaks for checks its own turn ran.
    pub turn_verification: Option<UiVerification>,
    /// How many files the diff reported during the current turn named. `diff`
    /// is whatever `/diff` last fetched — nothing refreshes it when a turn
    /// ends — so a turn-end summary counts only a diff its own turn saw.
    pub turn_diff_files: Option<usize>,
    /// The session-history query this client is waiting on, if any.
    pub history_query: Option<leveler_client_protocol::CommandId>,
    pub diff: Option<UiDiff>,
    pub diff_selected: usize,
    /// Whether the current busy turn was launched with `/goal`.
    pub goal_mode_active: bool,
    /// Product work profile: economy | balanced.
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
    /// Turn inputs sent but not yet answered by the runtime, oldest first.
    pub pending_submissions: Vec<PendingSubmission>,
    /// 待发送: inputs written while a turn runs, not yet admitted by the
    /// runtime. Bottom control state; never painted into the conversation.
    pub pending_inputs: Vec<crate::pending_inputs::PendingInput>,
    /// Keyboard selection into `pending_inputs`.
    pub pending_selected: usize,
    /// The 待发送 row under the mouse, if any.
    pub pending_hover: Option<usize>,
    /// Last-painted 待发送 rows, for mouse hit-testing.
    pub pending_hits: Vec<crate::pending_inputs::PendingInputHit>,

    pub status: RuntimeStatus,
    /// Mechanical post-response stage reported by the runtime. `Some` means
    /// the model is no longer the dependency even though the turn stays busy.
    pub finalization_stage: Option<leveler_client_protocol::FinalizationStage>,
    /// Number of tools started in the active turn.
    pub turn_tool_calls: usize,
    /// Coarse activity label shown while busy (e.g. "运行 cargo test").
    pub activity: Option<String>,
    /// Elapsed for whatever `activity` names, when the activity owns a clock of
    /// its own — a long command's heartbeat. The status line shows this in
    /// place of the turn's elapsed, so one running command reads as one
    /// duration. `None` for every activity that has no clock but the turn's.
    pub activity_elapsed_secs: Option<u64>,
    /// The active model-round retry, while the runtime waits out its backoff.
    /// Transient connectivity: it owns the status line ahead of the generic
    /// model wait and is cleared the moment a fresh attempt starts. Never a
    /// transcript item.
    pub reconnecting: Option<Reconnecting>,
    /// Set when a fresh attempt begins after at least one retry; the brief
    /// "Reconnected" confirmation expires at this instant. Never a transcript
    /// item.
    pub reconnected_until: Option<std::time::Instant>,
    pub notification: Option<Notification>,

    /// The running embedded Web UI URL (with token), once `/web` has started it.
    /// `Some` also guards against launching a second server.
    pub web_url: Option<String>,
    /// The `/update` panel, while a self-update is in flight or just finished.
    /// Presentation only: the update itself belongs to `leveler-update`.
    pub update: Option<crate::update::UpdateView>,
    /// Set when an installed update requires replacing this process. The event
    /// loop exits and the CLI restarts into the new binary.
    pub restart_requested: bool,
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
    /// Conversation viewport + interaction state (see `conversation::view`).
    pub conv: crate::conversation::ConversationView,
    /// Which region owns arrow keys (Tab toggles).
    pub workbench_focus: WorkbenchFocus,
    /// Last painted Input/composer rect for click-to-focus.
    pub input_rect: Option<(u16, u16, u16, u16)>,
    /// Transcript index of the user shell the Shell Details screen shows.
    pub shell_screen_item: Option<usize>,
    /// Which first-class activity the Activity Detail screen is showing.
    pub activity_open: Option<crate::activity::ActivityId>,
    /// Keyboard selection into [`crate::activity::summaries`], stable by id.
    pub activity_selected: Option<crate::activity::ActivityId>,
    /// Activity Detail viewport: follows the newest output until the user
    /// scrolls back. Owned here like `conv`; the renderer publishes the
    /// measured viewport each frame and the reducer maps scroll intents.
    pub activity_view: crate::activity::ActivityView,
    /// Last-painted status-strip hits: (row y, activity). Mouse open uses this.
    pub activity_hits: Vec<(u16, crate::activity::ActivityId)>,
    /// Keyboard selection into the background-jobs list page, by task id.
    /// Stable across reordering; `None` selects the first row.
    pub background_list_selected: Option<String>,
    /// Task ids whose failure the user has already seen. Session-local: it
    /// suppresses the footer `×失败 N` reminder, never the task's terminal
    /// state or history. Deliberately not persisted — "unread" is a live
    /// attention signal, not durable data.
    pub background_failures_seen: std::collections::HashSet<String>,
    /// Last-painted footer background-summary hit (row, x_start, x_end).
    pub background_footer_hit: Option<(u16, u16, u16)>,
    /// The running command row under the Command workbench focus, by the
    /// execution's authoritative [`ToolCallId`]. Presentation only: the stop
    /// path re-checks it against the live transcript before acting, so a
    /// finished or replayed row can never be stopped by a stale reference.
    pub command_selected: Option<leveler_client_protocol::ToolCallId>,
    /// Plan panel collapsed to a single title row.
    pub plan_collapsed: bool,
    /// Collapse the collaboration surface to its one-line compact row.
    /// View preference only — visibility itself is derived from team activity.
    pub collaboration_collapsed: bool,

    /// Legacy global expand flag — no longer forces every tool group open.
    /// Kept so the workbench can render the currently focused tool group.
    pub tools_expanded: bool,
    /// Shift+↑/↓ review index into user turns (`None` = live edge).
    /// Composer draft is never cleared while navigating.
    pub turn_nav: Option<usize>,

    /// Highlighted row in the slash-command completion popup (Up/Down navigate).
    pub slash_selected: usize,
    /// User pressed Esc while the slash popup was open; stay hidden until the
    /// composer text changes (so Esc can actually leave the menu).
    pub slash_popup_dismissed: bool,
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
    /// The session's goal, verbatim from the runtime snapshot. This is the
    /// authoritative task objective and the fallback title for the Active Goal
    /// when a turn was started by something other than this client.
    pub goal: String,
    /// The goal title staged for the turn that is about to start, consumed by
    /// [`crate::reducer::runtime_apply::start_turn`]. `None` when no user input
    /// announced this turn.
    pub staged_goal: Option<crate::active_goal::StagedGoal>,
    /// The Active Goal indicator's read model. Presentation only: the runtime
    /// owns the task, its status and its resumability; this projects what the
    /// header shows and tracks active execution time across interruptions.
    pub active_goal: Option<crate::active_goal::ActiveGoal>,
    pub model_label: String,
    /// Resolved reasoning effort for the active model (`max`, `high`, …).
    /// `None` means the runtime did not report one.
    pub reasoning_effort: Option<String>,
    pub mode_label: String,
    /// Last permission profile we asked the runtime to adopt. Used only so
    /// rapid Shift+Tab can cycle while the displayed chip still waits for
    /// `SessionUpdated` — the chip is `mode`/`mode_label`, never this.
    pub pending_permission: Option<PermissionProfile>,
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
    /// The session's last turn ended in a state a continuation can re-enter
    /// (interrupted, or a recoverable provider failure). Purely presentational:
    /// the runtime re-checks before it resumes, and the client never decides
    /// resume vs. chat from this flag.
    pub resumable_task: bool,
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
        let dark = theme.is_dark();
        let mut state = Self {
            running: true,
            runtime_connected: true,
            session_id: boot.session_id.clone(),
            transcript: TranscriptState::new(),
            surface: crate::btw::SurfaceFocus::Main,
            btw: crate::btw::BtwThread::default(),
            live_reasoning: String::new(),
            background_task_labels: std::collections::HashMap::new(),
            team: crate::multi_agent::TaskTeamView::default(),
            unfinished_goals: Vec::new(),
            composer: Composer::new(),
            prompt_suggestion: None,
            theme,
            size: (80, 24),
            active_screen: Screen::default(),
            tools_screen: ToolsScreenState::default(),
            trace: crate::observability::TraceView::default(),
            context: crate::context::ContextView::default(),
            clean: crate::clean::CleanState::default(),
            plan: None,
            project_rule_sources: Vec::new(),
            verification: None,
            turn_verification: None,
            turn_diff_files: None,
            history_query: None,
            diff: None,
            diff_selected: 0,
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
            pending_submissions: Vec::new(),
            pending_inputs: Vec::new(),
            pending_selected: 0,
            pending_hover: None,
            pending_hits: Vec::new(),
            status: RuntimeStatus::Idle,
            finalization_stage: None,
            turn_tool_calls: 0,
            activity: None,
            activity_elapsed_secs: None,
            reconnecting: None,
            reconnected_until: None,
            notification: None,
            web_url: None,
            update: None,
            restart_requested: false,
            web_starting: false,
            remote: None,
            available_models: Vec::new(),
            mode: PermissionProfile::Assisted,
            screen_scroll: 0,
            conv: crate::conversation::ConversationView::default(),
            workbench_focus: WorkbenchFocus::Input,
            input_rect: None,
            shell_screen_item: None,
            activity_open: None,
            activity_selected: None,
            activity_view: crate::activity::ActivityView::default(),
            activity_hits: Vec::new(),
            background_list_selected: None,
            background_failures_seen: std::collections::HashSet::new(),
            background_footer_hit: None,
            command_selected: None,
            plan_collapsed: false,
            collaboration_collapsed: false,
            tools_expanded: false,
            turn_nav: None,
            slash_selected: 0,
            slash_popup_dismissed: false,
            file_candidates: Vec::new(),
            file_index_requested: false,
            skill_catalog: Vec::new(),
            skill_catalog_root: None,
            repository: String::new(),
            branch: None,
            goal: String::new(),
            staged_goal: None,
            active_goal: None,
            model_label: "—".to_string(),
            reasoning_effort: boot.reasoning_effort.clone(),
            mode_label: "—".to_string(),
            pending_permission: None,
            clock_label: String::new(),
            context_window_tokens: boot.context_window,
            editor_chord_armed: false,
            cancel_armed: false,
            force_cancel_armed: false,
            resumable_task: false,
            quit_armed: false,
            tick: 0,
            turn_started_at: None,
            elapsed_secs: 0,
            dark,
            jump_to_bottom: false,
            locale: boot.locale,
            untrusted_config: boot.untrusted_config.clone(),
            boot,
        };
        // The composer writes images into the sentence in the session's own
        // language, so it needs the template before the first paste.
        let template = state.image_token_template();
        state.composer.set_image_token_template(&template);
        state.btw.draft.set_image_token_template(&template);
        state
    }

    /// The tool call an open approval overlay is holding, if any.
    ///
    /// Presentation only: the runtime still owns the decision. This exists so a
    /// call waiting on the human stops wearing the running mark (§11) — a
    /// `rm -rf` that has been announced but not authorised is not work in
    /// progress. Derived from the request's own `call_id`; a request that names
    /// no call (a standing `request_permissions`) gates no row.
    pub(crate) fn approval_gated_call(&self) -> Option<&leveler_client_protocol::ToolCallId> {
        match self.overlay.as_ref()? {
            Overlay::Approval(overlay) => overlay.gated_call.as_ref(),
            _ => None,
        }
    }

    /// Localized UI strings for the active locale.
    pub fn t(&self) -> &'static UiText {
        self.locale.text()
    }

    /// How an image is written into a sentence: `[图片 #{}]`, in the session's
    /// language. The composer inserts it, the model reads it, and the
    /// conversation keeps it — one name for a picture, and never a path.
    pub fn image_token_template(&self) -> String {
        self.t().attachment_chip.trim_end().to_string()
    }

    /// A message as the conversation shows it. Its text already names the
    /// images a client wrote into it; this only speaks for a message that
    /// carried pictures and no words at all, which would otherwise read as an
    /// empty line.
    pub fn message_with_images(&self, images: usize, text: &str) -> String {
        if !text.trim().is_empty() || images == 0 {
            return text.to_string();
        }
        let template = self.image_token_template();
        (1..=images)
            .map(|n| template.replace("{}", &n.to_string()))
            .collect::<Vec<_>>()
            .join(" ")
    }

    pub fn is_busy(&self) -> bool {
        self.status == RuntimeStatus::Busy
    }

    /// Whether this client may offer a stop for a running command at all.
    ///
    /// A stop is a live execution action. Without a connected runtime and a
    /// live turn there is nothing to cancel: a residual `Running` row left by
    /// replay or a lagged resync must not expose a `CancelToolCall`.
    pub fn can_stop_commands(&self) -> bool {
        self.runtime_connected && self.is_busy()
    }

    /// The call id that should be painted with the Command focus marker, when
    /// the Command workbench focus is active and the selection is still a
    /// stoppable running command. `None` clears the marker for any stale
    /// selection, so a finished row never keeps wearing it.
    pub fn focused_command(&self) -> Option<&leveler_client_protocol::ToolCallId> {
        if self.workbench_focus != WorkbenchFocus::Command || !self.can_stop_commands() {
            return None;
        }
        let id = self.command_selected.as_ref()?;
        let (item, call) = self.command_location(id)?;
        self.command_is_stoppable(item, call).then_some(id)
    }

    /// The running commands this client may stop, in transcript order, as
    /// `(item, call, id)`. Empty unless [`Self::can_stop_commands`].
    pub fn stoppable_commands(&self) -> Vec<(usize, usize, leveler_client_protocol::ToolCallId)> {
        if !self.can_stop_commands() {
            return Vec::new();
        }
        let mut out = Vec::new();
        for (item, entry) in self.transcript.items().iter().enumerate() {
            let crate::transcript::TranscriptItem::ToolGroup(group) = entry else {
                continue;
            };
            for (call, block) in group.calls.iter().enumerate() {
                if self.command_is_stoppable(item, call) {
                    out.push((item, call, block.id.clone()));
                }
            }
        }
        out
    }

    /// Whether the call at `(item, call)` is a running command this client may
    /// stop: still running, not already asked to stop, not awaiting approval.
    fn command_is_stoppable(&self, item: usize, call: usize) -> bool {
        let Some(crate::transcript::TranscriptItem::ToolGroup(group)) =
            self.transcript.items().get(item)
        else {
            return false;
        };
        let Some(block) = group.calls.get(call) else {
            return false;
        };
        block.status == crate::transcript::ToolStatus::Running
            && block.stop != crate::transcript::StopRequest::Sent
            && self.approval_gated_call() != Some(&block.id)
    }

    /// The transcript location of a running command by its authoritative id.
    pub fn command_location(
        &self,
        id: &leveler_client_protocol::ToolCallId,
    ) -> Option<(usize, usize)> {
        for (item, entry) in self.transcript.items().iter().enumerate() {
            let crate::transcript::TranscriptItem::ToolGroup(group) = entry else {
                continue;
            };
            if let Some(call) = group.calls.iter().position(|c| &c.id == id) {
                return Some((item, call));
            }
        }
        None
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

impl AppState {
    /// The user shell block the Shell Details screen is focused on.
    pub fn focused_user_shell(&self) -> Option<&crate::transcript::UserShellBlock> {
        let index = self.shell_screen_item?;
        match self.transcript.items().get(index)? {
            crate::transcript::TranscriptItem::UserShell(shell) => Some(shell),
            _ => None,
        }
    }
}
