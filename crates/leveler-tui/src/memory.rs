//! The `/memory` management page: a projection of the runtime's memory
//! listing plus the consent workflow over pending candidates.
//!
//! The store remains the only authority. This module holds what the page
//! needs to paint (one `MemoryList` snapshot), which rows the user selected
//! (by stable record id, never by position), and which lifecycle commands are
//! in flight. It never writes memory files and never promotes an entry on its
//! own; accepting is always an explicit key the user pressed.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent};
use leveler_client_protocol::{
    ClientCommand, CommandId, NotificationLevel, SessionId, UiMemoryCandidate, UiMemoryEntry,
};

use crate::action::Effect;
use crate::screen::Screen;
use crate::state::{AppState, Notification};

/// Which pane of the memory page is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MemoryView {
    #[default]
    Home,
    Review,
    Detail,
    Active,
    Archived,
}

/// The two consent decisions the review pane can send. Both are lifecycle
/// operations on the runtime's store, never local edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryOp {
    Accept,
    Reject,
}

/// Per-item outcome of one batch. The page reports every requested record;
/// a partial failure is never collapsed into "all succeeded".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryBatchReport {
    pub op: MemoryOp,
    pub succeeded: Vec<String>,
    pub failed: Vec<String>,
    pub unconfirmed: Vec<String>,
}

/// Presentation state for the memory page.
///
/// `pending`/`active`/`archived` are the last authoritative listing. They are
/// replaced wholesale by each `MemoryList` answer; nothing here is derived and
/// no record is ever mutated locally.
#[derive(Debug, Clone, Default)]
pub struct MemoryPage {
    pub view: MemoryView,
    /// The pane a detail view returns to.
    pub detail_return: MemoryView,
    pub menu: usize,
    pub loading: bool,
    pub active: Vec<UiMemoryEntry>,
    pub archived: Vec<UiMemoryEntry>,
    pub pending: Vec<UiMemoryCandidate>,
    /// Stable id of the focused row in the current pane.
    pub focus: Option<String>,
    /// Stable ids the user toggled for a batch decision.
    pub selected: BTreeSet<String>,
    /// The page's own listing query (open/refresh).
    pub list_query: Option<CommandId>,
    /// The discovery query issued on session open; only its answer announces
    /// newly found candidates.
    pub probe_query: Option<CommandId>,
    /// Lifecycle commands awaiting a runtime receipt, keyed by record id.
    ops: BTreeMap<String, MemoryOp>,
    /// Outcomes the runtime itself reported for ids that already answered.
    /// A missing entry means the command never produced evidence.
    receipts: BTreeMap<String, bool>,
    op_kind: Option<MemoryOp>,
    op_titles: BTreeMap<String, String>,
    op_started: Option<Instant>,
    /// Pending ids the post-decision refresh must classify.
    op_requested: Vec<String>,
    /// The last completed batch, painted until the user leaves the review.
    pub batch: Option<MemoryBatchReport>,
    /// Record ids the user has already been told about. Persisted by the boot
    /// layer so a restart does not re-announce the same batch.
    pub notified: BTreeSet<String>,
    /// Newly discovered records, held until the turn ends. Discovery inside a
    /// running turn must not compete with the task.
    pub notice_pending: Option<usize>,
    /// Set when `notified` changed and the boot layer should persist it.
    pub notice_dirty: bool,
}

impl MemoryPage {
    /// Open a pane and ask the runtime for the current listing.
    pub fn open(state: &mut AppState, view: MemoryView) -> Vec<Effect> {
        state.active_screen = Screen::Memory;
        state.memory.view = view;
        state.memory.detail_return = MemoryView::Home;
        state.memory.batch = None;
        state.screen_scroll = 0;
        if view != MemoryView::Home {
            state.memory.sync_focus();
        }
        query_list(state).into_iter().collect()
    }

    /// Ask the runtime for the pending set when a session opens. This is the
    /// discovery path for candidates proposed while this client was not
    /// attached (or by another session); without it they stay invisible until
    /// someone types `/memory`.
    pub fn probe(state: &mut AppState) -> Option<Effect> {
        if state.memory.probe_query.is_some() {
            return None;
        }
        let query_id = CommandId::generate();
        state.memory.probe_query = Some(query_id.clone());
        Some(Effect::Send(ClientCommand::ListMemory {
            session_id: state.session_id.clone(),
            query_id: Some(query_id),
            include_archived: true,
        }))
    }

    /// Adopt one authoritative listing.
    ///
    /// `announce` is true only for the session-open probe: a listing the user
    /// opened themselves is not news, and a post-decision refresh is not news
    /// either.
    pub fn apply_list(
        &mut self,
        active: Vec<UiMemoryEntry>,
        archived: Vec<UiMemoryEntry>,
        pending: Vec<UiMemoryCandidate>,
        announce: bool,
    ) {
        self.active = active;
        self.archived = archived;
        self.pending = pending;
        self.loading = false;

        let pending_ids: BTreeSet<String> = self.pending.iter().map(|c| c.id.clone()).collect();
        // Drop state about records that no longer exist. This also keeps the
        // persisted notified set bounded by the real pending set.
        self.selected.retain(|id| pending_ids.contains(id));
        let stale: Vec<String> = self.notified.difference(&pending_ids).cloned().collect();
        for id in stale {
            self.notified.remove(&id);
            // A record that left pending must also leave the persisted set;
            // otherwise the file only ever grows and a future record reusing
            // the id would be suppressed.
            self.notice_dirty = true;
        }

        if announce {
            let fresh = pending_ids.difference(&self.notified).count();
            if fresh > 0 {
                self.notice_pending = Some(fresh);
            }
        }
        for id in &pending_ids {
            if self.notified.insert(id.clone()) {
                self.notice_dirty = true;
            }
        }

        if self.op_kind.is_some() && self.ops.is_empty() {
            self.finish_batch();
        }
        self.sync_focus();
    }

    /// The authoritative answer `notified` should persist.
    pub fn take_notice_update(&mut self) -> Option<BTreeSet<String>> {
        if !self.notice_dirty {
            return None;
        }
        self.notice_dirty = false;
        Some(self.notified.clone())
    }

    /// Whether the discovery notice may be surfaced now. Called after every
    /// action so a notice found mid-turn waits for the turn's end.
    pub fn take_pending_notice(&mut self) -> Option<usize> {
        self.notice_pending.take()
    }

    /// Resolve receipts quoted by the runtime's own notification. The message
    /// carries the requested record id (`[…]`) for both success and failure,
    /// so one receipt settles exactly one in-flight command. `success` comes
    /// from the runtime's notification level, not from the message wording.
    /// Returns true when the batch has fully settled and a proving refresh
    /// must be requested.
    pub fn resolve_receipt(&mut self, message: &str, success: bool) -> bool {
        if self.op_kind.is_none() || !self.busy() {
            return false;
        }
        let settled: Vec<String> = self
            .ops
            .keys()
            .filter(|id| message.contains(&format!("[{id}]")))
            .cloned()
            .collect();
        for id in settled {
            self.ops.remove(&id);
            self.receipts.insert(id, success);
        }
        self.ops.is_empty()
    }

    /// A runtime that never answers must not leave the page stuck on the
    /// in-flight state. The refresh still decides the truth; a record with no
    /// proof is reported as unconfirmed, never as accepted.
    pub fn check_timeout(&mut self, now: Instant) -> bool {
        if self.op_kind.is_none() || self.ops.is_empty() {
            return false;
        }
        let Some(started) = self.op_started else {
            return false;
        };
        if now.duration_since(started) < OP_TIMEOUT {
            return false;
        }
        self.ops.clear();
        true
    }

    /// True when a decision is still waiting for the runtime.
    pub fn busy(&self) -> bool {
        !self.ops.is_empty()
    }

    pub fn focused_id(&self) -> Option<&str> {
        self.focus.as_deref()
    }

    pub fn focused_candidate(&self) -> Option<&UiMemoryCandidate> {
        let id = self.focus.as_deref()?;
        self.pending.iter().find(|c| c.id == id)
    }

    pub fn focused_entry(&self) -> Option<&UiMemoryEntry> {
        let id = self.focus.as_deref()?;
        self.active
            .iter()
            .chain(self.archived.iter())
            .find(|e| e.id == id)
    }

    /// Ids a decision key acts on: every toggled row, in the listing's own
    /// order, or the focused row when nothing is toggled.
    pub fn decision_targets(&self) -> Vec<String> {
        let mut targets: Vec<String> = self
            .pending
            .iter()
            .filter(|c| self.selected.contains(&c.id))
            .map(|c| c.id.clone())
            .collect();
        if targets.is_empty()
            && let Some(id) = self
                .focus
                .as_deref()
                .filter(|id| self.pending.iter().any(|c| c.id == *id))
        {
            targets.push(id.to_string());
        }
        targets
    }

    /// Start one batch decision. Returns the commands to send, one per record.
    /// A second batch is refused while the first is unsettled: pressing the
    /// key twice must not submit the same consent twice.
    pub fn begin_decision(&mut self, op: MemoryOp, session_id: &SessionId) -> Vec<ClientCommand> {
        if self.busy() {
            return Vec::new();
        }
        let targets = self.decision_targets();
        if targets.is_empty() {
            return Vec::new();
        }
        let mut commands = Vec::with_capacity(targets.len());
        for id in &targets {
            if let Some(candidate) = self.pending.iter().find(|c| c.id == *id) {
                self.op_titles.insert(id.clone(), candidate.title.clone());
            }
            self.ops.insert(id.clone(), op);
            commands.push(match op {
                MemoryOp::Accept => ClientCommand::AcceptMemory {
                    session_id: session_id.clone(),
                    id: id.clone(),
                },
                MemoryOp::Reject => ClientCommand::RejectMemory {
                    session_id: session_id.clone(),
                    id: id.clone(),
                },
            });
        }
        self.op_requested = targets;
        self.op_kind = Some(op);
        self.op_started = Some(Instant::now());
        self.receipts.clear();
        self.batch = None;
        commands
    }

    /// Fill the batch report once the post-decision listing arrives. A record
    /// is a success only when the runtime's own receipt says so; the listing
    /// then has to agree (the id must be gone from pending). No receipt means
    /// no proof, so a record that vanished without one is `unconfirmed` — a
    /// timeout is never reported as accepted.
    fn finish_batch(&mut self) {
        let Some(op) = self.op_kind.take() else {
            return;
        };
        let pending: BTreeSet<&str> = self.pending.iter().map(|c| c.id.as_str()).collect();
        let mut report = MemoryBatchReport {
            op,
            succeeded: Vec::new(),
            failed: Vec::new(),
            unconfirmed: Vec::new(),
        };
        for id in std::mem::take(&mut self.op_requested) {
            let title = self
                .op_titles
                .get(&id)
                .cloned()
                .unwrap_or_else(|| id.clone());
            let still_pending = pending.contains(id.as_str());
            let receipt = self.receipts.remove(&id);
            match (receipt, still_pending) {
                (Some(true), false) => report.succeeded.push(title),
                (Some(false), true) | (None, true) => report.failed.push(title),
                (Some(true), true) | (Some(false), false) | (None, false) => {
                    report.unconfirmed.push(title)
                }
            }
        }
        self.batch = Some(report);
        self.op_titles.clear();
        self.op_started = None;
        self.selected.clear();
    }

    /// Move the focus by one row within the current pane. Home moves the menu.
    pub fn move_focus(&mut self, delta: isize) {
        match self.view {
            MemoryView::Home => {
                self.menu = step(self.menu, delta, HOME_ITEMS);
            }
            MemoryView::Review => {
                let ids: Vec<String> = self.pending.iter().map(|c| c.id.clone()).collect();
                self.focus = step_id(self.focus.as_deref(), &ids, delta);
            }
            MemoryView::Active => {
                let ids: Vec<String> = self.active.iter().map(|e| e.id.clone()).collect();
                self.focus = step_id(self.focus.as_deref(), &ids, delta);
            }
            MemoryView::Archived => {
                let ids: Vec<String> = self.archived.iter().map(|e| e.id.clone()).collect();
                self.focus = step_id(self.focus.as_deref(), &ids, delta);
            }
            MemoryView::Detail => {}
        }
    }

    /// Point the focus at an id the page painted. A row key that is no longer
    /// in the listing selects nothing; the page never guesses.
    pub fn select(&mut self, key: &str) {
        if let Some(index) = key
            .strip_prefix("menu:")
            .and_then(|n| n.parse::<usize>().ok())
        {
            self.menu = index.min(HOME_ITEMS - 1);
            return;
        }
        let exists = match self.view {
            MemoryView::Review => self.pending.iter().any(|c| c.id == key),
            MemoryView::Active => self.active.iter().any(|e| e.id == key),
            MemoryView::Archived => self.archived.iter().any(|e| e.id == key),
            MemoryView::Home | MemoryView::Detail => false,
        };
        if exists {
            self.focus = Some(key.to_string());
        }
    }

    /// Toggle the focused row's selection.
    pub fn toggle_selected(&mut self) {
        if self.view != MemoryView::Review {
            return;
        }
        let Some(id) = self.focus.clone() else {
            return;
        };
        if !self.pending.iter().any(|c| c.id == id) {
            return;
        }
        if !self.selected.remove(&id) {
            self.selected.insert(id);
        }
    }

    /// The focused record may have disappeared after a refresh; keep the
    /// cursor on a real row rather than on a dead id.
    pub fn sync_focus(&mut self) {
        let ids: Vec<String> = match self.view {
            MemoryView::Review => self.pending.iter().map(|c| c.id.clone()).collect(),
            MemoryView::Active => self.active.iter().map(|e| e.id.clone()).collect(),
            MemoryView::Archived => self.archived.iter().map(|e| e.id.clone()).collect(),
            MemoryView::Home | MemoryView::Detail => return,
        };
        if self
            .focus
            .as_deref()
            .is_none_or(|id| !ids.iter().any(|row| row == id))
        {
            self.focus = ids.first().cloned();
        }
    }
}

/// Keystrokes for the memory page. Esc walks one level back and never
/// confirms anything; only `a`/`r` do.
pub fn screen_key(state: &mut AppState, key: KeyEvent) -> Vec<Effect> {
    match key.code {
        KeyCode::Esc => {
            back_one(state);
            Vec::new()
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if state.memory.view == MemoryView::Detail {
                state.screen_scroll = state.screen_scroll.saturating_sub(1);
            } else {
                state.memory.move_focus(-1);
            }
            Vec::new()
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if state.memory.view == MemoryView::Detail {
                state.screen_scroll = state.screen_scroll.saturating_add(1);
            } else {
                state.memory.move_focus(1);
            }
            Vec::new()
        }
        KeyCode::PageUp if state.memory.view == MemoryView::Detail => {
            state.screen_scroll = state.screen_scroll.saturating_sub(10);
            Vec::new()
        }
        KeyCode::PageDown if state.memory.view == MemoryView::Detail => {
            state.screen_scroll = state.screen_scroll.saturating_add(10);
            Vec::new()
        }
        KeyCode::Char(' ') => {
            state.memory.toggle_selected();
            Vec::new()
        }
        KeyCode::Enter => open_focused(state),
        KeyCode::Char('a') | KeyCode::Char('A') => decide(state, MemoryOp::Accept),
        KeyCode::Char('r') | KeyCode::Char('R') => decide(state, MemoryOp::Reject),
        _ => Vec::new(),
    }
}

/// One level back: detail → its list, list → home, home → conversation.
pub fn back_one(state: &mut AppState) {
    match state.memory.view {
        MemoryView::Detail => {
            state.memory.view = state.memory.detail_return;
            if state.memory.view == MemoryView::Home {
                state.memory.view = MemoryView::Review;
            }
            state.screen_scroll = 0;
        }
        MemoryView::Review | MemoryView::Active | MemoryView::Archived => {
            state.memory.view = MemoryView::Home;
            state.screen_scroll = 0;
        }
        MemoryView::Home => {
            state.active_screen = Screen::Conversation;
            state.screen_scroll = 0;
            state.screen_rows.clear();
        }
    }
}

/// Whether the memory page owns the current Esc. The shared `back()` delegates
/// here so the header's `←` and the Esc key share one rule.
pub fn owns_back(state: &AppState) -> bool {
    state.active_screen == Screen::Memory && state.memory.view != MemoryView::Home
}

pub fn wheel(state: &mut AppState, notches: isize) {
    if state.memory.view == MemoryView::Detail {
        let lines = (notches * 3).unsigned_abs();
        state.screen_scroll = if notches < 0 {
            state.screen_scroll.saturating_sub(lines)
        } else {
            state.screen_scroll.saturating_add(lines)
        };
    } else {
        state.memory.move_focus(notches);
    }
}

pub fn select_row(state: &mut AppState, key: &str) {
    if state.active_screen == Screen::Memory {
        state.memory.select(key);
    }
}

fn open_focused(state: &mut AppState) -> Vec<Effect> {
    match state.memory.view {
        MemoryView::Home => {
            let view = match state.memory.menu {
                0 => MemoryView::Active,
                1 => MemoryView::Review,
                _ => MemoryView::Archived,
            };
            state.memory.view = view;
            state.screen_scroll = 0;
            state.memory.sync_focus();
            Vec::new()
        }
        MemoryView::Review => {
            if state.memory.focused_candidate().is_some() {
                state.memory.detail_return = MemoryView::Review;
                state.memory.view = MemoryView::Detail;
                state.screen_scroll = 0;
            }
            Vec::new()
        }
        MemoryView::Active | MemoryView::Archived => {
            if state.memory.focused_entry().is_some() {
                state.memory.detail_return = state.memory.view;
                state.memory.view = MemoryView::Detail;
                state.screen_scroll = 0;
            }
            Vec::new()
        }
        MemoryView::Detail => Vec::new(),
    }
}

fn decide(state: &mut AppState, op: MemoryOp) -> Vec<Effect> {
    if state.memory.view != MemoryView::Review {
        return Vec::new();
    }
    state
        .memory
        .begin_decision(op, &state.session_id)
        .into_iter()
        .map(Effect::Send)
        .collect()
}

/// Ask for one listing. A later query supersedes an earlier one; the answer
/// carries the query id, so a stale response is ignored by the reducer.
pub fn query_list(state: &mut AppState) -> Option<Effect> {
    let query_id = CommandId::generate();
    state.memory.list_query = Some(query_id.clone());
    state.memory.loading = true;
    Some(Effect::Send(ClientCommand::ListMemory {
        session_id: state.session_id.clone(),
        query_id: Some(query_id),
        include_archived: true,
    }))
}

/// The session-open discovery probe. Kept here so the reducer and tests share
/// one construction of the command.
pub fn probe_effect(state: &mut AppState) -> Option<Effect> {
    MemoryPage::probe(state)
}

/// Surface a discovered-batch notice once the runtime is idle and no other
/// notice owns the one-line strip. A busy turn never sees it; the next tick
/// retries after a competing notice expires.
pub fn flush_notice(state: &mut AppState) {
    if state.is_busy() || state.notification.is_some() {
        return;
    }
    let Some(count) = state.memory.notice_pending else {
        return;
    };
    let message = notice_message(state.t(), count);
    state.memory.notice_pending = None;
    state.notification = Some(Notification {
        level: NotificationLevel::Info,
        message,
    });
}

/// The notice text for `count` newly discovered candidates.
pub fn notice_message(text: &crate::i18n::UiText, count: usize) -> String {
    if count == 1 {
        text.memory_notice_one.to_string()
    } else {
        text.memory_notice_many
            .replace("{count}", &count.to_string())
    }
}

/// `/memory` opens to Home; `/memory review` opens the consent queue directly.
pub fn open_page(state: &mut AppState, review: bool) -> Vec<Effect> {
    MemoryPage::open(
        state,
        if review {
            MemoryView::Review
        } else {
            MemoryView::Home
        },
    )
}

/// A record whose acceptance could be read as permission to destroy data is
/// marked as scope-unconfirmed. Detection is deterministic and local; it never
/// asks a model what the user meant.
pub fn is_high_risk(body: &str) -> bool {
    const MARKERS: &[&str] = &[
        "重置数据库",
        "清空数据库",
        "删除数据库",
        "删除生产",
        "清空数据",
        "drop database",
        "reset database",
        "delete database",
        "truncate table",
        "rm -rf",
    ];
    let lower = body.to_ascii_lowercase();
    MARKERS.iter().any(|marker| lower.contains(marker))
}

pub fn memory_notice_path(state: &AppState) -> Option<PathBuf> {
    state.memory_notice_path()
}

/// The set of record ids the user has already been told about. A missing or
/// unreadable file is an empty set: the failure mode is one extra notice,
/// never a silent loss of a pending record.
pub fn load_notified(path: &Path) -> BTreeSet<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return BTreeSet::new();
    };
    match serde_json::from_str::<Vec<String>>(&text) {
        Ok(ids) => ids.into_iter().collect(),
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "ignoring unreadable memory notice state");
            BTreeSet::new()
        }
    }
}

pub fn save_notified(path: &Path, ids: &BTreeSet<String>) {
    if let Some(parent) = path.parent()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        tracing::warn!(path = %parent.display(), %error, "could not create memory notice directory");
        return;
    }
    let ids: Vec<&String> = ids.iter().collect();
    match serde_json::to_string(&ids) {
        Ok(json) => {
            if let Err(error) = std::fs::write(path, json) {
                tracing::warn!(path = %path.display(), %error, "could not persist memory notice state");
            }
        }
        Err(error) => {
            tracing::warn!(%error, "could not encode memory notice state");
        }
    }
}

const HOME_ITEMS: usize = 3;
const OP_TIMEOUT: Duration = Duration::from_secs(5);

fn step(current: usize, delta: isize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    if delta < 0 {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        (current + delta as usize).min(len - 1)
    }
}

fn step_id(current: Option<&str>, ids: &[String], delta: isize) -> Option<String> {
    if ids.is_empty() {
        return None;
    }
    let index = current
        .and_then(|id| ids.iter().position(|row| row == id))
        .unwrap_or(0);
    Some(ids[step(index, delta, ids.len())].clone())
}
