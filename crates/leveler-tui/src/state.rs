//! [`AppState`] — the single source of truth the renderer reads and the reducer
//! mutates. Nothing else writes it directly .

use std::collections::{HashMap, VecDeque};

use leveler_client_protocol::{
    AttachmentRef, ClientCommand, CommandId, ModelRef, NotificationLevel, PermissionProfile,
    RuntimeStatus, SessionId, UiApprovalRequest, UiCheckpoint, UiClarificationRequest, UiDiff,
    UiPlan, UiSessionSummary,
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
    /// The session's Thinking Level, in the user's own vocabulary, as the
    /// runtime projected it. `None` when the runtime has not supplied one — the
    /// chip must not invent a level, and must never render a provider's own
    /// parameter instead.
    pub thinking: Option<leveler_client_protocol::UiThinkingState>,
    /// A startup fact the runtime decided while creating this session that the
    /// person must see — today, the persisted default model was not resolvable
    /// here and a configured model was used instead. It travels through the
    /// launcher because the runtime's own event stream has no subscriber yet at
    /// the moment the session is created, so a notice emitted there is lost.
    pub model_notice: Option<String>,
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
/// - [`Goal`](WorkbenchFocus::Goal): header goal summary (Enter opens detail)
/// - [`Plan`](WorkbenchFocus::Plan): one-line plan summary (Enter opens detail)
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
    Goal,
    Plan,
    Activity,
    /// The 待发送 list above the composer.
    Pending,
    /// A running command row in the transcript holds the keyboard focus.
    Command,
    /// The footer's aggregated background-jobs summary holds the focus.
    Background,
}

/// One keyboard-selectable execution in the transcript. A model tool call and
/// a user-typed `!command` share the Command focus — both are stoppable rows
/// with an output drawer, but their ids and cancel commands differ.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandTarget {
    /// A running agent tool call, addressed by its protocol id.
    Call(leveler_client_protocol::ToolCallId),
    /// A user-typed local command, addressed by its execution id.
    UserShell(leveler_core::UserShellId),
}

/// A stoppable execution and where it lives in the transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoppableCommand {
    pub item: usize,
    /// Call index inside the item's `ToolGroup`; `None` for a user shell.
    pub call: Option<usize>,
    pub target: CommandTarget,
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
    /// The process-group leader's OS pid, from the runtime's start/reconcile
    /// fact. Display only: the TUI never signals a pid; stopping still goes
    /// through the runtime's cancellation command.
    pub pid: Option<u32>,
    /// When this view last applied an output chunk. A projection freshness
    /// clock — "how long since the screen last moved" — never a process fact.
    /// `None` until the first chunk.
    pub last_output_at: Option<std::time::Instant>,
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
            pid: None,
            last_output_at: None,
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
    /// Version of metadata actually rendered from a runtime snapshot.
    pub snapshot_version: Option<i64>,
    pub session_id: SessionId,
    pub transcript: TranscriptState,
    /// Which conversation surface owns the viewport and the composer. One
    /// field decides both, so they cannot disagree about who is focused.
    pub surface: crate::btw::SurfaceFocus,
    /// The `/btw` side thread: its own conversation, draft and generation
    /// state. Never part of the main transcript, and never merged into the
    /// main run's context.
    pub btw: crate::btw::BtwThread,
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
    pub history_search: Option<crate::history_search::HistorySearch>,
    pub help: crate::help::HelpSearch,
    /// The structured next step offered as ghost text after a turn ends, or
    /// `None`. Presentation state only: see [`crate::suggestion`] — it is not
    /// composer content, not a draft, not history, and never submitted on its
    /// own. Ephemeral and never persisted.
    pub prompt_suggestion: Option<String>,
    /// Whether an asynchronously generated suggestion may still land for the
    /// current idle input window. Any user edit closes the window so a late
    /// model response cannot reappear after the user dismissed it.
    pub prompt_suggestion_allowed: bool,
    /// A recap request has fired and its response may still be displayed.
    /// A new turn closes this acceptance window. Typing does not.
    pub away_summary_pending: bool,
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
    /// How many files the diff reported during the current turn named. `diff`
    /// is whatever `/diff` last fetched — nothing refreshes it when a turn
    /// ends — so a turn-end summary counts only a diff its own turn saw.
    pub turn_diff_files: Option<usize>,
    /// The session-history query this client is waiting on, if any.
    pub history_query: Option<leveler_client_protocol::CommandId>,
    /// The `/memory` query owned by this TUI. Foreign/legacy broadcast replies
    /// are ignored so opening Web cannot inject a listing into the transcript.
    pub memory_query: Option<leveler_client_protocol::CommandId>,
    /// Agent-registry queries owned by this TUI. Responses traverse the
    /// shared session stream and must not render for another client.
    pub agents_list_query: Option<leveler_client_protocol::CommandId>,
    pub agent_detail_query: Option<leveler_client_protocol::CommandId>,
    pub child_contribution_query: Option<leveler_client_protocol::CommandId>,
    pub diff_query: Option<leveler_client_protocol::CommandId>,
    pub diff: Option<UiDiff>,
    /// A `RequestDiff` is in flight. Distinguishes "the answer is not here yet"
    /// from "there are no changes" — the Diff screen must never claim the
    /// workspace is clean while it is still computing the diff.
    pub diff_pending: bool,
    /// Why the last `RequestDiff` failed, if it did. Separate from an empty
    /// `diff`: an unreadable workspace must not be shown as "no changes".
    pub diff_error: Option<String>,
    pub diff_selected: usize,
    /// Whether the current busy turn was launched with `/goal`.
    pub goal_mode_active: bool,
    /// Collaboration mode: chat | plan | goal.
    pub collaboration: String,

    /// Attachments staged for the next message (spec §40).
    pub pending_attachments: Vec<AttachmentRef>,
    /// Whether the current model accepts images (from the snapshot, spec §42).
    pub vision: bool,

    /// Stored sessions and cursor for the Sessions screen (spec §52).
    pub sessions: Vec<UiSessionSummary>,
    pub sessions_selected: usize,
    pub context_tokens: u32,
    /// Latest model-reported input/output tokens for the current conversation.
    /// The status line and the footer read this last request. The turn receipt
    /// does not: that one sums [`Self::turn_input_tokens`].
    pub token_input: u32,
    pub token_output: u32,
    /// Prefix-cache hits within `token_input`, from the last round.
    pub token_cached: u32,
    /// Provider usage summed across the turn in progress. Copied onto the
    /// turn receipt when the turn ends, then cleared. Not the window gauge.
    pub turn_input_tokens: u32,
    pub turn_output_tokens: u32,
    pub turn_cached_tokens: u32,
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
    /// Selected child agent on the Team page, by runtime id. Selection only:
    /// the page owns no scroll state, so a resize cannot strand the row.
    pub team_selected: Option<String>,
    pub background_list_selected: Option<String>,
    /// Task ids whose failure the user has already seen. Session-local: it
    /// marks the issue "read", never solved — the task's terminal state,
    /// history and its place in the active issue list all stay until the
    /// source fact does. Deliberately not persisted — "unread" is a live
    /// attention signal, not durable data.
    pub background_failures_seen: std::collections::HashSet<String>,
    /// Last-painted state-strip background-indicator hit (row, x_start, x_end).
    pub background_hit: Option<(u16, u16, u16)>,
    /// Last-painted state-strip issue-indicator hit (row, x_start, x_end).
    pub issue_hit: Option<(u16, u16, u16)>,
    /// Selected row on the Issue page, by issue key (stable, never text).
    pub issue_selected: Option<String>,
    /// Issue rows the user has expanded, by issue key. Presentation state
    /// keyed by stable identity, so a resize, a return from a detail or new
    /// failures never lose or duplicate an expansion.
    pub issue_expanded: std::collections::HashSet<String>,
    /// Last-painted Issue-page row hits: (row, x_start, x_end, issue key).
    /// A click selects the row and toggles its expansion.
    pub issue_row_hits: Vec<(u16, u16, u16, String)>,
    /// Last-painted Issue-page detail-entry hits (the expanded row's
    /// "task detail" affordance): (row, x_start, x_end, issue key). Checked
    /// before the row hit so the entry never toggles its own row.
    pub issue_detail_hits: Vec<(u16, u16, u16, String)>,
    /// Last-painted `←` back-glyph hit on a Secondary Surface's header row:
    /// (row, x_start, x_end). Clicking it runs the SAME one-level back as Esc.
    pub back_hit: Option<(u16, u16, u16)>,
    /// Where the open Activity detail returns to. Set by the opener (the
    /// issue page, the background list); `None` means the conversation. The
    /// detail page itself never guesses its parent — it reads this.
    pub detail_return: Option<Screen>,
    /// The Issue page's selection as the last frame painted it: the content
    /// line the selected row sat on. The page follows the cursor when this
    /// changes, and only then — a wheel scroll the user asked for is not undone
    /// on the next frame.
    pub issue_selected_line: Option<usize>,
    /// The rows the active page painted this frame: (rect, the page's own row
    /// key). Republished every frame, so a click resolves against exactly what
    /// is on screen — after a scroll, a resize or a data update — and a row
    /// that was never painted owns no hit. The key's meaning belongs to the
    /// page: a stable id where one exists, the row's index otherwise.
    pub screen_rows: Vec<(ratatui::layout::Rect, String)>,
    /// Painted stop controls, bound to their screen and exact task identity.
    /// These are hit regions only; cancellation rechecks the live projection.
    pub background_list_hits: Vec<(ratatui::layout::Rect, String)>,
    pub background_list_rect: Option<ratatui::layout::Rect>,
    pub background_output_rect: Option<ratatui::layout::Rect>,
    pub background_stop_hits: Vec<(Screen, ratatui::layout::Rect, String)>,
    /// Which pane owns ↑/↓ on the Background Tasks page: the task list or the
    /// selected task's live output. Presentation state only.
    pub background_list_focus: crate::activity::BackgroundPane,
    /// Per-task log viewport (scroll/follow), keyed by task id. Kept per task
    /// so switching selection never carries another task's scroll position.
    /// Bounded by the same terminal-task window as the chrome map.
    pub background_task_views: std::collections::HashMap<String, crate::activity::ActivityView>,
    /// Last-painted one-line plan summary hit (row, x_start, x_end).
    pub plan_hit: Option<(u16, u16, u16)>,
    /// Last-painted header goal hit (row, x_start, x_end).
    pub goal_hit: Option<(u16, u16, u16)>,
    /// The running execution under the Command workbench focus. Presentation
    /// only: the stop path re-checks it against the live transcript before
    /// acting, so a finished or replayed row can never be stopped by a stale
    /// reference.
    pub command_selected: Option<CommandTarget>,
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
    pub repository: Option<String>,
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
    /// The session's Thinking Level for the active model, canonical and
    /// user-facing. `None` means the runtime did not report one.
    pub thinking: Option<leveler_client_protocol::UiThinkingState>,
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
    /// Set after the user pressed cancel a second time while the turn was still
    /// running. A further Ctrl+C quits the client so a hung turn cannot trap
    /// the user in cancel-only key handling. There is no stronger cancel to
    /// send — the first request is already durable and idempotent.
    pub cancel_repeat_armed: bool,
    /// The session's last turn ended in a state a continuation can re-enter
    /// (interrupted, or a recoverable provider failure). Purely presentational:
    /// the runtime re-checks before it resumes, and the client never decides
    /// resume vs. chat from this flag.
    pub resumable_task: bool,
    pub quit_armed: bool,

    /// Monotonic frame counter driving the busy spinner animation.
    pub tick: u64,
    /// The UI's monotonic motion clock, sampled by the event loop each
    /// iteration. Live animations read ELAPSED TIME from it, never the frame
    /// counter: a burst of stream deltas repaints many frames a second, and a
    /// frame-counted phase would sweep proportionally faster than its period.
    pub motion_clock: std::time::Duration,
    /// Whether live animations are painted at all. Resolved once from
    /// [`crate::motion::ENV_ANIMATION`]; false degrades every animated surface
    /// to its static form.
    pub live_animation: bool,
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
        // Read before the boot is moved into the state: this is a startup fact
        // the launcher carried from the runtime (see `Boot::model_notice`).
        let model_notice = boot.model_notice.clone();
        let mut state = Self {
            running: true,
            runtime_connected: true,
            snapshot_version: None,
            session_id: boot.session_id.clone(),
            transcript: TranscriptState::new(),
            surface: crate::btw::SurfaceFocus::Main,
            btw: crate::btw::BtwThread::default(),
            background_task_labels: std::collections::HashMap::new(),
            team: crate::multi_agent::TaskTeamView::default(),
            unfinished_goals: Vec::new(),
            composer: Composer::new(),
            history_search: None,
            help: crate::help::HelpSearch::default(),
            prompt_suggestion: None,
            prompt_suggestion_allowed: true,
            away_summary_pending: false,
            theme,
            size: (80, 24),
            active_screen: Screen::default(),
            tools_screen: ToolsScreenState::default(),
            trace: crate::observability::TraceView::default(),
            context: crate::context::ContextView::default(),
            clean: crate::clean::CleanState::default(),
            plan: None,
            project_rule_sources: Vec::new(),
            turn_diff_files: None,
            history_query: None,
            memory_query: None,
            agents_list_query: None,
            agent_detail_query: None,
            child_contribution_query: None,
            diff_query: None,
            diff: None,
            diff_pending: false,
            diff_error: None,
            diff_selected: 0,
            goal_mode_active: false,
            collaboration: "chat".into(),
            pending_attachments: Vec::new(),
            vision: false,
            sessions: Vec::new(),
            sessions_selected: 0,
            context_tokens: 0,
            token_input: 0,
            token_output: 0,
            token_cached: 0,
            turn_input_tokens: 0,
            turn_output_tokens: 0,
            turn_cached_tokens: 0,
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
            activity_open: None,
            activity_selected: None,
            activity_view: crate::activity::ActivityView::default(),
            activity_hits: Vec::new(),
            team_selected: None,
            background_list_selected: None,
            background_failures_seen: std::collections::HashSet::new(),
            background_hit: None,
            issue_hit: None,
            issue_selected: None,
            issue_expanded: std::collections::HashSet::new(),
            issue_row_hits: Vec::new(),
            issue_detail_hits: Vec::new(),
            back_hit: None,
            detail_return: None,
            issue_selected_line: None,
            screen_rows: Vec::new(),
            background_list_hits: Vec::new(),
            background_list_rect: None,
            background_output_rect: None,
            background_stop_hits: Vec::new(),
            background_list_focus: crate::activity::BackgroundPane::List,
            background_task_views: std::collections::HashMap::new(),
            plan_hit: None,
            goal_hit: None,
            command_selected: None,
            collaboration_collapsed: false,
            tools_expanded: false,
            turn_nav: None,
            slash_selected: 0,
            slash_popup_dismissed: false,
            file_candidates: Vec::new(),
            file_index_requested: false,
            skill_catalog: Vec::new(),
            skill_catalog_root: None,
            repository: None,
            branch: None,
            goal: String::new(),
            staged_goal: None,
            active_goal: None,
            model_label: "—".to_string(),
            thinking: boot.thinking.clone(),
            mode_label: "—".to_string(),
            pending_permission: None,
            clock_label: String::new(),
            context_window_tokens: boot.context_window,
            editor_chord_armed: false,
            cancel_armed: false,
            cancel_repeat_armed: false,
            resumable_task: false,
            quit_armed: false,
            tick: 0,
            motion_clock: std::time::Duration::ZERO,
            live_animation: crate::motion::live_animation_enabled(),
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
        // A startup fact the launcher carried from the runtime: today, a
        // persisted default model this environment could not resolve. Shown so
        // the model on screen is explained rather than silently different from
        // the one the user last chose.
        if let Some(notice) = model_notice {
            state.notification = Some(Notification {
                level: leveler_client_protocol::NotificationLevel::Warning,
                message: notice,
            });
        }
        let template = state.image_token_template();
        state.composer.set_image_token_template(&template);
        state.btw.draft.set_image_token_template(&template);
        let template = state.t().file_reference_chip.trim_end().to_string();
        state.composer.set_file_token_template(&template);
        state.btw.draft.set_file_token_template(&template);
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

    /// Whether an open clarification card owns the live question.
    ///
    /// The card is pinned under the conversation and cannot scroll away, so it
    /// is the one place the question is stated while it is being asked; the
    /// running call's row above it would otherwise print the same sentence a
    /// second time. A settled call — and a replayed session, which never
    /// reopens a card — keeps the question on the row as the record.
    pub(crate) fn question_card_open(&self) -> bool {
        matches!(self.overlay, Some(Overlay::Clarification(_)))
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
        let CommandTarget::Call(id) = self.command_selected.as_ref()? else {
            return None;
        };
        let (item, call) = self.call_location(id)?;
        self.command_is_stoppable(item, call).then_some(id)
    }

    /// The user shell id that should wear the Command focus marker, when the
    /// Command workbench focus is on a still-running `!command`.
    pub fn focused_user_shell_id(&self) -> Option<&leveler_core::UserShellId> {
        if self.workbench_focus != WorkbenchFocus::Command || !self.runtime_connected {
            return None;
        }
        let CommandTarget::UserShell(id) = self.command_selected.as_ref()? else {
            return None;
        };
        self.user_shell_is_stoppable(id).then_some(id)
    }

    /// The running executions this client may stop, in transcript order.
    /// Agent calls need a live turn; a user shell is the turn — it only needs
    /// a connected runtime.
    pub fn stoppable_commands(&self) -> Vec<StoppableCommand> {
        if !self.runtime_connected {
            return Vec::new();
        }
        let mut out = Vec::new();
        for (item, entry) in self.transcript.items().iter().enumerate() {
            match entry {
                crate::transcript::TranscriptItem::ToolGroup(group) if self.is_busy() => {
                    for (call, block) in group.calls.iter().enumerate() {
                        if self.command_is_stoppable(item, call) {
                            out.push(StoppableCommand {
                                item,
                                call: Some(call),
                                target: CommandTarget::Call(block.id.clone()),
                            });
                        }
                    }
                }
                crate::transcript::TranscriptItem::UserShell(shell)
                    if shell.status == crate::transcript::UserShellStatus::Running =>
                {
                    out.push(StoppableCommand {
                        item,
                        call: None,
                        target: CommandTarget::UserShell(shell.id.clone()),
                    });
                }
                _ => {}
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

    /// The transcript location of a Command-focus target: the item index and,
    /// for an agent call, its index inside the group.
    pub fn command_location(&self, target: &CommandTarget) -> Option<(usize, Option<usize>)> {
        match target {
            CommandTarget::Call(id) => self
                .call_location(id)
                .map(|(item, call)| (item, Some(call))),
            CommandTarget::UserShell(id) => self
                .transcript
                .user_shell_index(id)
                .map(|item| (item, None)),
        }
    }

    /// The transcript location of a tool call by its authoritative id.
    pub fn call_location(
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

    /// Whether a user shell is still running and the runtime is connected —
    /// the same staleness rule agent calls use before a stop is offered.
    fn user_shell_is_stoppable(&self, id: &leveler_core::UserShellId) -> bool {
        if !self.runtime_connected {
            return false;
        }
        let Some(item) = self.transcript.user_shell_index(id) else {
            return false;
        };
        matches!(
            self.transcript.items().get(item),
            Some(crate::transcript::TranscriptItem::UserShell(shell))
                if shell.status == crate::transcript::UserShellStatus::Running
        )
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
        self.cancel_repeat_armed = false;
        self.quit_armed = false;
    }
}
