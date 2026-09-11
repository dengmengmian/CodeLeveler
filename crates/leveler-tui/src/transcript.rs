//! The transcript: a typed, growing list of conversation blocks.
//!
//! Not a `Vec<String>` . Each block renders and (later) folds
//! independently. It carries the blocks the base shell needs; extensions
//! add Tool/Plan/Diff/Verification/Attachment/Agent blocks.

use leveler_client_protocol::{MessageId, ToolCallId, UiCompletionReport};

use crate::markdown::MdDoc;

/// What one assistant message IS in the turn, decided by event order alone.
///
/// The classifier never reads the prose: a message followed by a tool call was
/// demonstrably not the answer, and a message the turn ended on was. Nothing
/// here is a guess about wording, length, or Markdown shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssistantKind {
    /// Streaming, or complete but the turn has not yet shown which it is.
    /// Presented under the same bound as [`Self::Progress`] so classification
    /// never moves the rows already on screen.
    Pending,
    /// Interim narration: a tool call followed it. Compacted by default, full
    /// text kept and one click away.
    Progress,
    /// The turn's answer: the turn ended on it with no tool call after. Always
    /// rendered in full, never folded.
    Final,
}

/// What [`TranscriptState::toggle_last_collapsible`] flipped. The caller needs
/// the distinction because the workbench's tool-expand flag must not follow an
/// assistant disclosure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToggledBlock {
    Tool(bool),
    AssistantProgress(bool),
}

/// A streaming assistant message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistantBlock {
    pub id: MessageId,
    pub text: String,
    pub done: bool,
    /// Parsed markdown, computed once when the message completes (spec §62).
    pub rendered: Option<MdDoc>,
    /// Progress vs. answer, assigned retroactively from event order.
    pub kind: AssistantKind,
    /// Per-block disclosure for a folded [`AssistantKind::Progress`] message.
    /// Every historical block owns its own flag — there is no global
    /// "assistant expanded" mode.
    pub expanded: bool,
}

/// The lifecycle state of a tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Running,
    Ok,
    Failed,
}

/// A tool invocation .
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallBlock {
    pub id: ToolCallId,
    pub name: String,
    /// Compacted JSON arguments.
    pub arguments: String,
    pub status: ToolStatus,
    /// The runtime's truncated output preview (once complete).
    pub preview: Option<String>,
    /// Wall-clock duration measured by the runtime client.
    pub duration_ms: Option<u64>,
    /// True when this call ran in the concurrent read-only batch.
    pub parallel: bool,
    /// The turn's `elapsed_secs` when this call started, so a running command
    /// can show a live elapsed (`now - started`) instead of a static block.
    pub started_elapsed_secs: u64,
    /// Canonical unified diff of what this edit ACTUALLY changed, reported by
    /// the tool that made it. The inline diff renders from THIS when it is
    /// present: `arguments` say what the model wanted, and only execution
    /// knows where the change landed. `None` for every non-edit call, and for
    /// an edit whose location could not be established.
    pub applied_diff: Option<String>,
}

/// A consecutive burst of tool calls between two assistant messages.
///
/// The group stays open after an individual call finishes because the model may
/// immediately issue another call. Keeping the whole burst live prevents an
/// early call from being committed to terminal scrollback before the UI knows
/// the group is complete and can collapse it to one summary line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolGroupBlock {
    pub calls: Vec<ToolCallBlock>,
    pub open: bool,
    /// Per-group disclosure. Ctrl+O toggles only the current (latest) group.
    pub expanded: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnEndStatus {
    Completed,
    Answered,
    Truncated,
    Incomplete,
    /// Work finished, but leveler could not independently verify it. Done, not
    /// verified — rendered as a ✓ with an "unverified" caveat, not an alarm.
    Unverified,
    /// Work finished and the project's own checks then failed over the final
    /// tree. Done, checks failed — both facts on one marker.
    ChecksFailed,
    Failed,
    Cancelled,
}

/// Persistent boundary between one finished turn and the next user input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnEndBlock {
    pub status: TurnEndStatus,
    pub tool_calls: usize,
    pub elapsed_secs: u64,
    /// Optional product summary, e.g. `3 files · verify ✓`.
    pub summary: Option<String>,
    /// Runtime reason for incomplete / truncated / unverified / failed turns.
    /// Kept on the marker so it does not vanish with the status notification.
    pub detail: Option<String>,
}

/// A trusted post-turn handoff supplied through structured runtime data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecapBlock {
    pub summary: Option<String>,
    pub next_step: String,
}

/// A durable goal recap (long-goal P3): the HISTORY presentation of one
/// persisted GoalCheckpoint. `recap.checkpoint_id` is its identity — the
/// expanded view presents the same persisted fields, never a client-side
/// re-summary — and it never moves into the lower runtime stack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalRecapBlock {
    pub recap: leveler_client_protocol::UiGoalRecap,
    pub expanded: bool,
}

/// A spawned sub-agent (multi-agent delegation), one block per agent, updated
/// from running → done in place.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubAgentProgress {
    pub active: bool,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cached_input_tokens: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubAgentBlock {
    pub id: String,
    /// Show the full result instead of the first few lines (Ctrl+O).
    pub expanded: bool,
    pub nickname: String,
    pub role: String,
    pub status: ToolStatus,
    /// The task while running; a short result summary once done.
    pub detail: String,
    pub progress: SubAgentProgress,
    /// Latest tool/step from the runtime (real event; not invented stats).
    pub recent_step: Option<String>,
    /// The turn's `elapsed_secs` when this sub-agent started, so a live view can
    /// show each agent's own running time (`now_elapsed - started`). `0` when the
    /// start time is unknown (finish-without-start fallback).
    pub started_elapsed_secs: u64,
    /// What the parent did with what this child produced, projected from
    /// the runtime's `contribution`. `Pending` until the child finishes;
    /// `NotMeasured` when the runtime produced no projection — which is not
    /// the same fact as a measured zero and must not render as one.
    pub contribution: crate::multi_agent::Contribution,
}

/// Ephemeral side question (`/btw`) — rendered in the UI but never loaded
/// back from session storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BtwBlock {
    pub question: String,
    pub answer: String,
    pub done: bool,
    pub failed: bool,
}

/// One block in the transcript.
/// Terminal state of one user shell execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserShellStatus {
    Running,
    Success,
    Failed,
    Cancelled,
}

impl UserShellStatus {
    pub fn from_wire(status: &str) -> Self {
        match status {
            "success" => Self::Success,
            "cancelled" => Self::Cancelled,
            _ => Self::Failed,
        }
    }
}

/// A user shell execution (`!command`) — NOT a tool call and NOT a user
/// message: it never enters the model conversation. Bounded, sanitized
/// output tail only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserShellBlock {
    pub id: leveler_core::UserShellId,
    pub command: String,
    pub cwd: String,
    pub status: UserShellStatus,
    pub output: String,
    pub output_truncated: bool,
    pub exit_code: Option<i32>,
    pub duration_ms: Option<u64>,
    /// Session-elapsed baseline when it started (drives the live runtime).
    /// SIGNED: a reconnect back-dates this below zero so a shell that ran 21
    /// minutes before the client attached still shows 21 minutes, not 0.
    pub started_elapsed_secs: i64,
    /// Show full output inline (mouse / Ctrl+O). Pure display state.
    pub expanded: bool,
}

/// Bounded client-side output tail for a user shell block.
pub const USER_SHELL_OUTPUT_CAP: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptItem {
    User(String),
    Assistant(AssistantBlock),
    ToolGroup(ToolGroupBlock),
    /// Visible model reasoning. Not a disclosure: rendered directly.
    SubAgent(SubAgentBlock),
    UserShell(UserShellBlock),
    Completion(UiCompletionReport),
    Error(String),
    /// Multi-line host note (e.g. `/memory` listing). Transcript-visible, not
    /// a 1-row status notification.
    Note(String),
    TurnEnd(TurnEndBlock),
    Recap(RecapBlock),
    /// Durable goal checkpoint presentation (`✽ 阶段回顾`), click-expandable.
    GoalRecap(GoalRecapBlock),
    Btw(BtwBlock),
}

/// The ordered list of transcript blocks.
#[derive(Debug, Default, Clone)]
pub struct TranscriptState {
    items: Vec<TranscriptItem>,
    /// Bumped on every mutation so the conversation renderer can cache its
    /// wrapped lines and only rebuild when the content actually changed. Every
    /// `&mut self` method calls [`Self::bump`]; over-bumping is safe (a wasted
    /// rebuild), under-bumping is not (stale render), so err toward bumping.
    version: u64,
}

impl TranscriptState {
    pub fn new() -> Self {
        Self::default()
    }

    /// A monotonic content version; changes whenever the transcript mutates.
    pub fn version(&self) -> u64 {
        self.version
    }

    #[inline]
    fn bump(&mut self) {
        self.version = self.version.wrapping_add(1);
    }

    pub fn items(&self) -> &[TranscriptItem] {
        &self.items
    }

    pub fn items_mut(&mut self) -> &mut [TranscriptItem] {
        // The caller takes a mutable slice; assume it mutates and invalidate.
        self.bump();
        &mut self.items
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn push_user(&mut self, text: String) {
        self.bump();
        self.close_tool_group();
        self.items.push(TranscriptItem::User(text));
    }

    pub fn push_user_if_new(&mut self, text: String) {
        self.bump();
        if matches!(self.items.last(), Some(TranscriptItem::User(existing)) if existing == &text) {
            return;
        }
        self.push_user(text);
    }

    pub fn push_error(&mut self, text: String) {
        self.bump();
        self.close_tool_group();
        self.items.push(TranscriptItem::Error(text));
    }

    /// Durable multi-line host note (memory list, etc.). Survives status TTL.
    pub fn push_note(&mut self, text: String) {
        self.bump();
        self.close_tool_group();
        self.items.push(TranscriptItem::Note(text));
    }

    /// Start a `/btw` side-question block (ephemeral).
    pub fn begin_btw(&mut self, question: String) {
        self.bump();
        self.close_tool_group();
        self.items.push(TranscriptItem::Btw(BtwBlock {
            question,
            answer: String::new(),
            done: false,
            failed: false,
        }));
    }

    pub fn append_btw(&mut self, delta: &str) {
        self.bump();
        if let Some(TranscriptItem::Btw(b)) = self
            .items
            .iter_mut()
            .rev()
            .find(|i| matches!(i, TranscriptItem::Btw(b) if !b.done))
        {
            b.answer.push_str(delta);
        }
    }

    pub fn finish_btw(&mut self, failed: bool) {
        self.bump();
        if let Some(TranscriptItem::Btw(b)) = self
            .items
            .iter_mut()
            .rev()
            .find(|i| matches!(i, TranscriptItem::Btw(b) if !b.done))
        {
            b.done = true;
            b.failed = failed;
        }
    }

    pub fn push_completion(&mut self, report: UiCompletionReport) {
        self.bump();
        self.close_tool_group();
        self.items.push(TranscriptItem::Completion(report));
    }

    pub fn push_turn_end(
        &mut self,
        status: TurnEndStatus,
        tool_calls: usize,
        elapsed_secs: u64,
        summary: Option<String>,
        detail: Option<String>,
    ) {
        // The turn ended with nothing acting on the last prose: that prose is
        // the answer. Runs before the duplicate guard so a suppressed second
        // marker still cannot leave a block undecided.
        self.decide_pending_assistants(AssistantKind::Final);
        let already_ended = matches!(self.items.last(), Some(TranscriptItem::TurnEnd(_)))
            || matches!(
                self.items.as_slice(),
                [.., TranscriptItem::TurnEnd(_), TranscriptItem::Recap(_)]
            );
        if already_ended {
            return;
        }
        self.close_tool_group();
        self.items.push(TranscriptItem::TurnEnd(TurnEndBlock {
            status,
            tool_calls,
            elapsed_secs,
            summary,
            detail,
        }));
    }

    pub fn push_recap(&mut self, recap: RecapBlock) {
        self.bump();
        if matches!(self.items.last(), Some(TranscriptItem::Recap(_))) {
            return;
        }
        self.close_tool_group();
        self.items.push(TranscriptItem::Recap(recap));
    }

    /// Append a durable goal recap. Idempotent on `checkpoint_id`:
    /// at-least-once event delivery (or a snapshot replay racing a live
    /// event) must not duplicate a checkpoint's history item.
    pub fn push_goal_recap(&mut self, recap: leveler_client_protocol::UiGoalRecap) {
        self.bump();
        if self.items.iter().any(|item| {
            matches!(item, TranscriptItem::GoalRecap(block)
                if block.recap.checkpoint_id == recap.checkpoint_id)
        }) {
            return;
        }
        self.close_tool_group();
        self.items.push(TranscriptItem::GoalRecap(GoalRecapBlock {
            recap,
            expanded: false,
        }));
    }

    /// Return a handoff only when a successful `update_goal` explicitly
    /// supplied a concrete `next_step`. Freeform assistant prose is never used.
    pub fn latest_turn_handoff(&self) -> Option<RecapBlock> {
        for item in self.items.iter().rev() {
            match item {
                TranscriptItem::TurnEnd(_) => break,
                TranscriptItem::ToolGroup(group) => {
                    for call in group.calls.iter().rev() {
                        if call.name != "update_goal" || call.status != ToolStatus::Ok {
                            continue;
                        }
                        let value =
                            serde_json::from_str::<serde_json::Value>(&call.arguments).ok()?;
                        if !matches!(
                            value.get("status").and_then(serde_json::Value::as_str),
                            Some("complete" | "blocked")
                        ) {
                            continue;
                        }
                        let next_step = value
                            .get("next_step")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string)
                            .and_then(|text| compact_summary(text, 160))?;
                        let summary = value
                            .get("summary")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string)
                            .and_then(|text| compact_summary(text, 320));
                        return Some(RecapBlock { summary, next_step });
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// Decide every still-undecided assistant block, from event order alone.
    ///
    /// Called at the two mechanical moments that carry the fact: a tool call
    /// starting proves the prose before it was not the answer, and a turn
    /// ending on prose proves it was. Idempotent — a block already decided
    /// keeps its kind, so a duplicate or replayed event cannot reclassify
    /// history.
    fn decide_pending_assistants(&mut self, kind: AssistantKind) {
        let mut changed = false;
        for item in &mut self.items {
            if let TranscriptItem::Assistant(block) = item
                && block.kind == AssistantKind::Pending
            {
                block.kind = kind;
                changed = true;
            }
        }
        if changed {
            self.bump();
        }
    }

    /// Begin a new assistant message that deltas will target.
    pub fn begin_assistant(&mut self, id: MessageId) {
        self.bump();
        // Guard against a duplicate start for the same id.
        if self.assistant_mut(&id).is_none() {
            self.close_tool_group();
            self.items.push(TranscriptItem::Assistant(AssistantBlock {
                id,
                text: String::new(),
                done: false,
                rendered: None,
                kind: AssistantKind::Pending,
                expanded: false,
            }));
        }
    }

    /// Remove an unfinished assistant block from a failed stream attempt.
    pub fn reset_assistant_attempt(&mut self, id: &MessageId) {
        self.bump();
        self.items.retain(|item| {
            !matches!(item, TranscriptItem::Assistant(block) if &block.id == id && !block.done)
        });
    }

    /// Append streamed text to an assistant message, starting it if needed. If
    /// the message was already finished (a late delta, e.g. a stream retry that
    /// re-emits after completion), reopen it so the new text actually renders
    /// (the cached `rendered` would otherwise hide it).
    pub fn append_assistant(&mut self, id: &MessageId, delta: &str) {
        self.bump();
        match self.assistant_mut(id) {
            Some(block) => {
                block.text.push_str(delta);
                block.done = false;
                block.rendered = None;
            }
            None => {
                self.close_tool_group();
                self.items.push(TranscriptItem::Assistant(AssistantBlock {
                    id: id.clone(),
                    text: delta.to_string(),
                    done: false,
                    rendered: None,
                    kind: AssistantKind::Pending,
                    expanded: false,
                }));
            }
        }
    }

    /// Mark an assistant message complete and parse its markdown once.
    pub fn finish_assistant(&mut self, id: &MessageId) {
        self.bump();
        if let Some(block) = self.assistant_mut(id) {
            block.done = true;
            block.rendered = Some(MdDoc::parse(&block.text));
        }
    }

    /// Finalize any block left in-flight when a turn ends (fail/cancel/lag can
    /// drop the `Completed` event). Without this, a running tool/sub-agent block
    /// or an unfinished assistant message stays "live" forever — never committing
    /// to scrollback, showing a stuck spinner/cursor. A cleanly-completed turn has
    /// nothing in-flight, so this is a no-op there.
    pub fn finalize_in_flight(&mut self) {
        self.bump();
        for item in &mut self.items {
            match item {
                TranscriptItem::Assistant(b) if !b.done => {
                    b.done = true;
                    b.rendered = Some(MdDoc::parse(&b.text));
                }
                TranscriptItem::ToolGroup(group) => {
                    group.open = false;
                    for call in &mut group.calls {
                        if call.status == ToolStatus::Running {
                            call.status = ToolStatus::Failed;
                        }
                    }
                }
                TranscriptItem::SubAgent(b) if b.status == ToolStatus::Running => {
                    b.status = ToolStatus::Failed;
                }
                _ => {}
            }
        }
    }

    /// Record a started tool call as a running block.
    pub fn push_tool_started(
        &mut self,
        id: ToolCallId,
        name: String,
        arguments: String,
        parallel: bool,
        started_elapsed_secs: u64,
    ) {
        self.bump();
        let call = ToolCallBlock {
            id,
            name,
            arguments,
            status: ToolStatus::Running,
            preview: None,
            duration_ms: None,
            parallel,
            started_elapsed_secs,
            applied_diff: None,
        };
        if let Some(TranscriptItem::ToolGroup(group)) = self.items.last()
            && group.open
            && starts_new_activity(group, &call)
        {
            self.close_tool_group();
        }
        // Visible work acting on the prose is the proof that the prose was not
        // the answer. Silent bookkeeping is NOT that proof: a real run ends
        // `answer → update_goal(complete) → turn end`, and counting that as a
        // boundary folded the answer away. Same rule the activity stream uses
        // to decide what is work at all.
        if is_grouping_visible(&call) {
            self.decide_pending_assistants(AssistantKind::Progress);
        }
        match self.items.last_mut() {
            Some(TranscriptItem::ToolGroup(group)) if group.open => group.calls.push(call),
            _ => self.items.push(TranscriptItem::ToolGroup(ToolGroupBlock {
                calls: vec![call],
                open: true,
                expanded: false,
            })),
        }
    }

    /// Complete a tool call, updating its status, preview, duration, and the
    /// applied diff an edit reported.
    pub fn complete_tool(
        &mut self,
        id: &ToolCallId,
        ok: bool,
        preview: String,
        duration_ms: u64,
        applied_diff: Option<String>,
    ) {
        self.bump();
        for item in self.items.iter_mut().rev() {
            let TranscriptItem::ToolGroup(group) = item else {
                continue;
            };
            if let Some(block) = group.calls.iter_mut().rev().find(|call| &call.id == id) {
                block.status = if ok {
                    ToolStatus::Ok
                } else {
                    ToolStatus::Failed
                };
                block.preview = Some(preview);
                block.duration_ms = Some(duration_ms);
                block.applied_diff = applied_diff;
                return;
            }
        }
    }

    /// Begin a user shell execution block (idempotent per id).
    pub fn push_user_shell_started(
        &mut self,
        id: leveler_core::UserShellId,
        command: String,
        cwd: String,
        started_elapsed_secs: i64,
    ) {
        self.bump();
        self.close_tool_group();
        if self.user_shell_index(&id).is_some() {
            return;
        }
        self.items.push(TranscriptItem::UserShell(UserShellBlock {
            id,
            command,
            cwd,
            status: UserShellStatus::Running,
            output: String::new(),
            output_truncated: false,
            exit_code: None,
            duration_ms: None,
            started_elapsed_secs,
            expanded: false,
        }));
    }

    /// Append sanitized output to a running user shell (bounded tail).
    pub fn append_user_shell_output(&mut self, id: &leveler_core::UserShellId, chunk: &str) {
        let Some(index) = self.user_shell_index(id) else {
            return;
        };
        self.bump();
        if let TranscriptItem::UserShell(shell) = &mut self.items[index] {
            shell.output.push_str(chunk);
            if shell.output.len() > USER_SHELL_OUTPUT_CAP {
                let cut = shell.output.len() - USER_SHELL_OUTPUT_CAP;
                let cut = leveler_core::floor_char_boundary(&shell.output, cut);
                shell.output.drain(..cut);
                shell.output_truncated = true;
            }
        }
    }

    /// Finish a user shell execution with its terminal facts.
    pub fn complete_user_shell(
        &mut self,
        id: &leveler_core::UserShellId,
        exit_code: Option<i32>,
        duration_ms: u64,
        status: UserShellStatus,
    ) {
        let Some(index) = self.user_shell_index(id) else {
            return;
        };
        self.bump();
        if let TranscriptItem::UserShell(shell) = &mut self.items[index] {
            shell.exit_code = exit_code;
            shell.duration_ms = Some(duration_ms);
            shell.status = status;
        }
    }

    /// Index of the user shell block with `id`, if present.
    pub fn user_shell_index(&self, id: &leveler_core::UserShellId) -> Option<usize> {
        self.items
            .iter()
            .position(|item| matches!(item, TranscriptItem::UserShell(shell) if &shell.id == id))
    }

    /// Toggle expand/collapse on the collapsible block at `index` (a
    /// ToolGroup or SubAgent item). The mouse path: a click on a disclosure
    /// row targets exactly that historical group, not the latest one.
    pub fn toggle_tool_group_at(&mut self, index: usize) -> Option<bool> {
        let toggled = match self.items.get_mut(index)? {
            TranscriptItem::ToolGroup(group) => {
                group.expanded = !group.expanded;
                Some(group.expanded)
            }
            TranscriptItem::SubAgent(block) => {
                block.expanded = !block.expanded;
                Some(block.expanded)
            }
            TranscriptItem::UserShell(shell) => {
                shell.expanded = !shell.expanded;
                Some(shell.expanded)
            }
            TranscriptItem::GoalRecap(block) => {
                block.expanded = !block.expanded;
                Some(block.expanded)
            }
            // A folded progress block toggles exactly itself. A Final answer
            // has no disclosure row, so it is not a toggle target at all.
            TranscriptItem::Assistant(block) if block.kind != AssistantKind::Final => {
                block.expanded = !block.expanded;
                Some(block.expanded)
            }
            _ => None,
        };
        if toggled.is_some() {
            self.bump();
        }
        toggled
    }

    /// Toggle expand/collapse on whichever collapsible block came last.
    ///
    /// Tool groups, sub-agents and folded progress prose all hide detail once
    /// finished, so one key opens whichever one you are looking at. The
    /// transcript knows nothing about widths or themes, so `assistant_folds`
    /// supplies the one fact it cannot derive: whether that block actually has
    /// a disclosure row on screen right now. Returns what was toggled, or
    /// `None` when there is nothing collapsible.
    pub fn toggle_last_collapsible(
        &mut self,
        assistant_folds: impl Fn(&AssistantBlock) -> bool,
    ) -> Option<ToggledBlock> {
        self.bump();
        for item in self.items.iter_mut().rev() {
            match item {
                TranscriptItem::ToolGroup(group) => {
                    group.expanded = !group.expanded;
                    return Some(ToggledBlock::Tool(group.expanded));
                }
                TranscriptItem::SubAgent(block) => {
                    block.expanded = !block.expanded;
                    return Some(ToggledBlock::Tool(block.expanded));
                }
                TranscriptItem::Assistant(block)
                    if block.kind == AssistantKind::Progress && assistant_folds(block) =>
                {
                    block.expanded = !block.expanded;
                    return Some(ToggledBlock::AssistantProgress(block.expanded));
                }
                _ => {}
            }
        }
        None
    }

    /// Classify replayed history, where no live event order survives: inside
    /// each user-delimited turn the LAST assistant message is the answer and
    /// every earlier one was interim narration. Only touches still-unclassified
    /// blocks, so replaying over a live transcript cannot demote an answer.
    pub fn classify_replayed_history(&mut self) {
        let mut answer_seen = false;
        for index in (0..self.items.len()).rev() {
            match &self.items[index] {
                TranscriptItem::User(_) => answer_seen = false,
                TranscriptItem::Assistant(block) if block.kind == AssistantKind::Pending => {
                    let kind = if answer_seen {
                        AssistantKind::Progress
                    } else {
                        AssistantKind::Final
                    };
                    answer_seen = true;
                    if let TranscriptItem::Assistant(block) = &mut self.items[index] {
                        block.kind = kind;
                    }
                    self.version = self.version.wrapping_add(1);
                }
                TranscriptItem::Assistant(_) => answer_seen = true,
                _ => {}
            }
        }
    }

    /// Dismiss the latest finished `/btw` card (done or failed). Returns true
    /// if a card was removed. Running (incomplete) cards are left alone.
    pub fn dismiss_latest_finished_btw(&mut self) -> bool {
        self.bump();
        if let Some(idx) = self.items.iter().rposition(|item| {
            matches!(
                item,
                TranscriptItem::Btw(b) if b.done
            )
        }) {
            self.items.remove(idx);
            true
        } else {
            false
        }
    }

    /// Whether any finished btw card is still on screen.
    pub fn has_finished_btw(&self) -> bool {
        self.items
            .iter()
            .any(|item| matches!(item, TranscriptItem::Btw(b) if b.done))
    }

    /// All tool-call blocks, in order (for the Tools screen).
    pub fn tool_calls(&self) -> Vec<&ToolCallBlock> {
        self.items
            .iter()
            .flat_map(|item| match item {
                TranscriptItem::ToolGroup(group) => group.calls.iter(),
                _ => [].iter(),
            })
            .collect()
    }

    fn close_tool_group(&mut self) {
        self.bump();
        if let Some(TranscriptItem::ToolGroup(group)) = self.items.last_mut() {
            group.open = false;
        }
    }

    fn sub_agent_mut(&mut self, id: &str) -> Option<&mut SubAgentBlock> {
        self.bump();
        self.items.iter_mut().rev().find_map(|item| match item {
            TranscriptItem::SubAgent(b) if b.id == id => Some(b),
            _ => None,
        })
    }

    /// Record or update a running sub-agent, one block per id. A repeated running
    /// update for the same id refreshes its detail in place instead of pushing a
    /// duplicate block.
    pub fn push_sub_agent_started(
        &mut self,
        id: String,
        nickname: String,
        role: String,
        task: String,
        started_elapsed_secs: u64,
    ) {
        if let Some(block) = self.sub_agent_mut(&id) {
            block.detail = task;
            return;
        }
        self.close_tool_group();
        self.items.push(TranscriptItem::SubAgent(SubAgentBlock {
            expanded: false,
            id,
            nickname,
            role,
            status: ToolStatus::Running,
            detail: task,
            progress: SubAgentProgress::default(),
            recent_step: None,
            started_elapsed_secs,
            contribution: crate::multi_agent::Contribution::Pending,
        }));
    }

    /// Mark a sub-agent done, updating its status and result summary in place. If
    /// the finish arrives before/without a start (e.g. a dropped event), still
    /// show a completed block so the result isn't lost.
    /// Finish a child using the runtime's projected finding count.
    ///
    /// `None` means the runtime did not measure this child; it is not a zero,
    /// and the renderer must be able to tell the two apart.
    pub fn complete_sub_agent_with_contribution(
        &mut self,
        id: &str,
        nickname: &str,
        ok: bool,
        summary: String,
        contribution: crate::multi_agent::Contribution,
    ) {
        let status = if ok {
            ToolStatus::Ok
        } else {
            ToolStatus::Failed
        };
        if let Some(block) = self.sub_agent_mut(id) {
            block.status = status;
            block.detail = summary;
            block.contribution = contribution.clone();
            return;
        }
        self.items.push(TranscriptItem::SubAgent(SubAgentBlock {
            id: id.to_string(),
            expanded: false,
            nickname: nickname.to_string(),
            role: String::new(),
            status,
            detail: summary,
            progress: SubAgentProgress::default(),
            recent_step: None,
            started_elapsed_secs: 0,
            contribution,
        }));
    }

    pub fn complete_sub_agent(&mut self, id: &str, nickname: &str, ok: bool, summary: String) {
        self.bump();
        let status = if ok {
            ToolStatus::Ok
        } else {
            ToolStatus::Failed
        };
        // No projection reached us: the runtime did not measure this child.
        // Saying so is honest; scanning the model-facing summary for a count
        // is how the UI used to invent one.
        let contribution = crate::multi_agent::Contribution::NotMeasured;
        if let Some(block) = self.sub_agent_mut(id) {
            block.status = status;
            block.detail = summary;
            block.progress.active = false;
            block.contribution = contribution.clone();
            return;
        }
        self.close_tool_group();
        self.items.push(TranscriptItem::SubAgent(SubAgentBlock {
            expanded: false,
            id: id.to_string(),
            nickname: nickname.to_string(),
            role: String::new(),
            status,
            detail: summary,
            progress: SubAgentProgress::default(),
            recent_step: None,
            started_elapsed_secs: 0,
            contribution,
        }));
    }

    pub fn update_sub_agent_progress(
        &mut self,
        id: &str,
        active: bool,
        input_tokens: u32,
        output_tokens: u32,
        cached_input_tokens: u32,
    ) {
        if let Some(block) = self.sub_agent_mut(id) {
            block.progress = SubAgentProgress {
                active,
                input_tokens,
                output_tokens,
                cached_input_tokens,
            };
        }
    }

    /// Record the latest real tool/step for a running sub-agent.
    pub fn update_sub_agent_activity(&mut self, id: &str, step: String) {
        if let Some(block) = self.sub_agent_mut(id)
            && block.status == ToolStatus::Running
        {
            block.recent_step = Some(step);
        }
    }

    /// Clear every block (visual `/clear`; does not delete the session).
    pub fn clear(&mut self) {
        self.bump();
        self.items.clear();
    }

    fn assistant_mut(&mut self, id: &MessageId) -> Option<&mut AssistantBlock> {
        self.bump();
        self.items.iter_mut().rev().find_map(|item| match item {
            TranscriptItem::Assistant(b) if &b.id == id => Some(b),
            _ => None,
        })
    }
}

fn compact_summary(text: String, max_chars: usize) -> Option<String> {
    let compact = MdDoc::parse(&text).plain_text();
    if compact.is_empty() {
        return None;
    }
    let mut summary: String = compact.chars().take(max_chars).collect();
    if compact.chars().count() > max_chars {
        summary.push('…');
    }
    Some(summary)
}

/// Whether `call` begins a new semantic activity instead of continuing `group`.
///
/// A transcript burst ("everything between two assistant messages") is a
/// runtime fact; an activity is what the reader sees. Welding reads, an edit
/// and a shell run into one block let the edit hold the whole burst open and
/// left the conversation with no narrative shape — so a fully settled group
/// yields to the next KIND of work.
fn starts_new_activity(group: &ToolGroupBlock, call: &ToolCallBlock) -> bool {
    // Still in flight: this is a real concurrent batch, never split it.
    if group.calls.iter().any(|c| c.status == ToolStatus::Running) {
        return false;
    }
    // Silent probes are not part of the visible narrative: they neither open a
    // boundary nor change what the current group is about.
    if !is_grouping_visible(call) {
        return false;
    }
    let Some(prev) = group.calls.iter().rev().find(|c| is_grouping_visible(c)) else {
        return false;
    };
    crate::tool_taxonomy::activity_class(&prev.name)
        != crate::tool_taxonomy::activity_class(&call.name)
}

/// Grouping reads the tool's declared visibility only — never its status,
/// which changes long after the grouping decision was made.
fn is_grouping_visible(call: &ToolCallBlock) -> bool {
    crate::tool_taxonomy::activity_visibility(&call.name, &call.arguments)
        != crate::tool_taxonomy::ActivityVisibility::Silent
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_client_protocol::ToolCallId;

    #[test]
    fn version_bumps_on_mutation_but_not_on_reads() {
        let mut t = TranscriptState::new();
        let v = t.version();

        // Reads never change the version.
        let _ = t.items();
        let _ = t.is_empty();
        let _ = t.len();
        assert_eq!(t.version(), v, "reads must not bump");

        // Representative mutations each advance it.
        t.push_user("hi".into());
        let v1 = t.version();
        assert!(v1 > v, "push_user must bump");

        t.push_tool_started(
            ToolCallId::new("t1"),
            "read_file".into(),
            "{}".into(),
            false,
            0,
        );
        let v2 = t.version();
        assert!(v2 > v1, "push_tool_started must bump");

        t.complete_tool(&ToolCallId::new("t1"), true, "ok".into(), 1, None);
        let v3 = t.version();
        assert!(v3 > v2, "complete_tool must bump");

        // In-place mutation via the slice escape hatch must also invalidate.
        let _ = t.items_mut();
        assert!(t.version() > v3, "items_mut must bump");
    }

    // ── Semantic activity boundaries ────────────────────────────────────────

    /// Push one call and settle it, so the next push sees a fully settled
    /// (but still open) group — the shape a sequential model turn produces.
    fn settled(t: &mut TranscriptState, id: &str, name: &str, args: &str) {
        t.push_tool_started(ToolCallId::new(id), name.into(), args.into(), false, 0);
        t.complete_tool(&ToolCallId::new(id), true, "ok".into(), 1, None);
    }

    fn group_shapes(t: &TranscriptState) -> Vec<Vec<String>> {
        t.items()
            .iter()
            .filter_map(|i| match i {
                TranscriptItem::ToolGroup(g) => {
                    Some(g.calls.iter().map(|c| c.name.clone()).collect())
                }
                _ => None,
            })
            .collect()
    }

    /// CASE A: read × 5 → edit → shell × 2 → read × 3, each phase settled
    /// before the next begins. One burst between two assistant messages is a
    /// runtime fact, not a unit of narrative: these are four separate pieces
    /// of work and must not be welded into one un-foldable group.
    #[test]
    fn settled_phases_of_different_kinds_start_their_own_activity_group() {
        let mut t = TranscriptState::new();
        for i in 0..5 {
            settled(&mut t, &format!("r{i}"), "read_file", r#"{"path":"a"}"#);
        }
        settled(&mut t, "e1", "apply_patch", r#"{"patch":"x"}"#);
        for i in 0..2 {
            settled(
                &mut t,
                &format!("s{i}"),
                "run_command",
                r#"{"command":"cargo test"}"#,
            );
        }
        for i in 0..3 {
            settled(&mut t, &format!("r2{i}"), "read_file", r#"{"path":"b"}"#);
        }
        let shapes = group_shapes(&t);
        assert_eq!(
            shapes.len(),
            4,
            "read / edit / shell / read are four activities: {shapes:?}"
        );
        assert_eq!(shapes[0].len(), 5);
        assert_eq!(shapes[1], vec!["apply_patch".to_string()]);
        assert_eq!(shapes[2].len(), 2);
        assert_eq!(shapes[3].len(), 3);
    }

    /// Reads and searches are one activity ("exploring"), not two.
    #[test]
    fn reads_and_searches_stay_in_one_exploration_group() {
        let mut t = TranscriptState::new();
        settled(&mut t, "r1", "read_file", r#"{"path":"a"}"#);
        settled(&mut t, "g1", "grep", r#"{"pattern":"x"}"#);
        settled(&mut t, "r2", "read_file", r#"{"path":"b"}"#);
        assert_eq!(group_shapes(&t).len(), 1, "{:?}", group_shapes(&t));
    }

    /// A group with work still in flight is a real parallel batch. The model
    /// streams the next call's arguments while earlier ones run; splitting
    /// there would shatter one concurrent burst into unrelated rows.
    #[test]
    fn a_group_with_running_work_never_splits() {
        let mut t = TranscriptState::new();
        t.push_tool_started(
            ToolCallId::new("r1"),
            "read_file".into(),
            r#"{"path":"a"}"#.into(),
            true,
            0,
        );
        t.push_tool_started(
            ToolCallId::new("e1"),
            "apply_patch".into(),
            r#"{"patch":"x"}"#.into(),
            true,
            0,
        );
        assert_eq!(group_shapes(&t).len(), 1, "{:?}", group_shapes(&t));
    }

    /// Silent probes (list_files, goal bookkeeping) are invisible in the
    /// conversation, so they must not chop the visible activity into pieces
    /// nor redefine what kind of work the group is doing.
    #[test]
    fn silent_calls_never_form_an_activity_boundary() {
        let mut t = TranscriptState::new();
        settled(&mut t, "r1", "read_file", r#"{"path":"a"}"#);
        settled(&mut t, "l1", "list_files", r#"{"path":"."}"#);
        settled(&mut t, "r2", "read_file", r#"{"path":"b"}"#);
        assert_eq!(group_shapes(&t).len(), 1, "{:?}", group_shapes(&t));
    }

    /// Consecutive edits are one activity and keep the same-file merge.
    #[test]
    fn consecutive_edits_stay_in_one_group() {
        let mut t = TranscriptState::new();
        settled(&mut t, "e1", "apply_patch", r#"{"patch":"a"}"#);
        settled(&mut t, "e2", "apply_patch", r#"{"patch":"b"}"#);
        assert_eq!(group_shapes(&t).len(), 1, "{:?}", group_shapes(&t));
    }

    // ---- Assistant progress / final classification (event order only) ----

    fn kinds(t: &TranscriptState) -> Vec<AssistantKind> {
        t.items()
            .iter()
            .filter_map(|i| match i {
                TranscriptItem::Assistant(b) => Some(b.kind),
                _ => None,
            })
            .collect()
    }

    fn say(t: &mut TranscriptState, id: &str, text: &str) {
        let id = MessageId::new(id);
        t.begin_assistant(id.clone());
        t.append_assistant(&id, text);
        t.finish_assistant(&id);
    }

    /// An assistant message a tool call followed was demonstrably not the
    /// answer. Nothing about its wording is consulted.
    #[test]
    fn a_tool_call_makes_the_preceding_message_progress() {
        let mut t = TranscriptState::new();
        say(&mut t, "m1", "先查一下现有实现");
        assert_eq!(kinds(&t), vec![AssistantKind::Pending], "not yet decided");
        settled(&mut t, "r1", "read_file", r#"{"path":"a"}"#);
        assert_eq!(kinds(&t), vec![AssistantKind::Progress]);
    }

    /// The message a turn ended on, with no tool call after it, is the answer.
    #[test]
    fn the_message_a_turn_ends_on_is_final() {
        let mut t = TranscriptState::new();
        say(&mut t, "m1", "先查一下");
        settled(&mut t, "r1", "read_file", r#"{"path":"a"}"#);
        say(&mut t, "m2", "改完了。");
        t.push_turn_end(TurnEndStatus::Completed, 1, 3, None, None);
        assert_eq!(
            kinds(&t),
            vec![AssistantKind::Progress, AssistantKind::Final]
        );
    }

    /// Classification is retroactive and per-block: a long turn alternating
    /// prose and tools ends with exactly one Final.
    #[test]
    fn only_the_last_message_of_a_turn_is_final() {
        let mut t = TranscriptState::new();
        say(&mut t, "m1", "a");
        settled(&mut t, "r1", "read_file", r#"{"path":"a"}"#);
        say(&mut t, "m2", "b");
        settled(&mut t, "e1", "apply_patch", r#"{"patch":"p"}"#);
        say(&mut t, "m3", "c");
        t.push_turn_end(TurnEndStatus::Completed, 2, 5, None, None);
        assert_eq!(
            kinds(&t),
            vec![
                AssistantKind::Progress,
                AssistantKind::Progress,
                AssistantKind::Final
            ]
        );
    }

    /// Goal bookkeeping is not acting on the prose. The real dogfood run
    /// ended `answer → update_goal(complete) → turn_finished`, and treating
    /// that silent call as a tool boundary folded the actual answer away.
    /// Only conversation-visible work decides that prose was interim.
    #[test]
    fn silent_bookkeeping_after_the_answer_does_not_demote_it() {
        let mut t = TranscriptState::new();
        say(&mut t, "m1", "两处都改好了。");
        settled(
            &mut t,
            "g1",
            "update_goal",
            r#"{"status":"complete","summary":"done"}"#,
        );
        t.push_turn_end(TurnEndStatus::Completed, 1, 9, None, None);
        assert_eq!(kinds(&t), vec![AssistantKind::Final]);
    }

    /// A silent exploration probe is not a boundary either — the prose stays
    /// undecided until real work or the turn marker settles it.
    #[test]
    fn a_silent_probe_leaves_the_prose_undecided() {
        let mut t = TranscriptState::new();
        say(&mut t, "m1", "先看看目录");
        settled(&mut t, "l1", "list_files", r#"{"path":"."}"#);
        assert_eq!(kinds(&t), vec![AssistantKind::Pending]);
        settled(&mut t, "r1", "read_file", r#"{"path":"a"}"#);
        assert_eq!(kinds(&t), vec![AssistantKind::Progress]);
    }

    /// `update_goal(blocked)` IS user-facing work — it is why the run stopped —
    /// so prose in front of it was narration, not the answer.
    #[test]
    fn a_blocked_goal_update_is_visible_work_and_does_demote() {
        let mut t = TranscriptState::new();
        say(&mut t, "m1", "卡住了，先说明");
        settled(
            &mut t,
            "g1",
            "update_goal",
            r#"{"status":"blocked","summary":"缺少凭据"}"#,
        );
        assert_eq!(kinds(&t), vec![AssistantKind::Progress]);
    }

    /// A cancelled or failed turn still classifies: the marker is the same
    /// mechanical signal, so no block is left Pending forever.
    #[test]
    fn a_cancelled_turn_still_classifies_its_last_message() {
        let mut t = TranscriptState::new();
        say(&mut t, "m1", "正在改");
        t.push_turn_end(TurnEndStatus::Cancelled, 0, 1, None, None);
        assert_eq!(kinds(&t), vec![AssistantKind::Final]);
    }

    /// Replayed history carries no event order, so the turn shape decides:
    /// the last assistant message before the next user turn is that turn's
    /// answer, every earlier one was narration.
    #[test]
    fn replayed_history_classifies_by_turn_shape() {
        let mut t = TranscriptState::new();
        t.push_user("第一个需求".into());
        say(&mut t, "m1", "先看代码");
        say(&mut t, "m2", "改好了");
        t.push_user("第二个需求".into());
        say(&mut t, "m3", "又改好了");
        t.classify_replayed_history();
        assert_eq!(
            kinds(&t),
            vec![
                AssistantKind::Progress,
                AssistantKind::Final,
                AssistantKind::Final
            ]
        );
    }

    /// Replay must never demote an answer that live events already proved.
    #[test]
    fn replay_classification_leaves_decided_blocks_alone() {
        let mut t = TranscriptState::new();
        say(&mut t, "m1", "答案");
        t.push_turn_end(TurnEndStatus::Completed, 0, 1, None, None);
        say(&mut t, "m2", "后续");
        t.classify_replayed_history();
        assert_eq!(kinds(&t), vec![AssistantKind::Final, AssistantKind::Final]);
    }

    /// Each historical progress block owns its disclosure: expanding one
    /// leaves the others exactly as they were.
    #[test]
    fn expanding_one_progress_block_leaves_the_others_folded() {
        let mut t = TranscriptState::new();
        say(&mut t, "m1", "一");
        settled(&mut t, "r1", "read_file", r#"{"path":"a"}"#);
        say(&mut t, "m2", "二");
        settled(&mut t, "r2", "read_file", r#"{"path":"b"}"#);
        let first = t
            .items()
            .iter()
            .position(|i| matches!(i, TranscriptItem::Assistant(_)))
            .expect("first progress block");
        t.toggle_tool_group_at(first);
        let flags: Vec<bool> = t
            .items()
            .iter()
            .filter_map(|i| match i {
                TranscriptItem::Assistant(b) => Some(b.expanded),
                _ => None,
            })
            .collect();
        assert_eq!(flags, vec![true, false]);
    }

    /// A Final answer has no disclosure, so nothing can fold it — not a
    /// click on its rows, not Ctrl+O.
    #[test]
    fn a_final_answer_can_never_be_folded() {
        let mut t = TranscriptState::new();
        say(&mut t, "m1", "答案");
        t.push_turn_end(TurnEndStatus::Completed, 0, 1, None, None);
        let at = t
            .items()
            .iter()
            .position(|i| matches!(i, TranscriptItem::Assistant(_)))
            .expect("the answer");
        assert_eq!(t.toggle_tool_group_at(at), None);
        assert_eq!(t.toggle_last_collapsible(|_| true), None);
    }

    /// Ctrl+O reaches a folded progress block, and reports it as an assistant
    /// toggle so the workbench tool flag does not follow.
    #[test]
    fn ctrl_o_reaches_a_folded_progress_block() {
        let mut t = TranscriptState::new();
        say(&mut t, "m1", "过程");
        settled(&mut t, "r1", "read_file", r#"{"path":"a"}"#);
        // The tool group is latest, so it wins — unchanged behaviour.
        assert_eq!(
            t.toggle_last_collapsible(|_| true),
            Some(ToggledBlock::Tool(true))
        );
        say(&mut t, "m2", "更多过程");
        settled(&mut t, "r2", "read_file", r#"{"path":"b"}"#);
        say(&mut t, "m3", "过程三");
        t.items_mut().iter_mut().for_each(|i| {
            if let TranscriptItem::Assistant(b) = i {
                b.kind = AssistantKind::Progress;
            }
        });
        assert_eq!(
            t.toggle_last_collapsible(|_| true),
            Some(ToggledBlock::AssistantProgress(true))
        );
    }

    /// A progress block short enough to render whole has no disclosure row,
    /// so Ctrl+O must skip it and keep reaching the tool group behind it.
    #[test]
    fn ctrl_o_skips_a_progress_block_that_did_not_fold() {
        let mut t = TranscriptState::new();
        settled(&mut t, "r1", "read_file", r#"{"path":"a"}"#);
        say(&mut t, "m1", "短");
        t.items_mut().iter_mut().for_each(|i| {
            if let TranscriptItem::Assistant(b) = i {
                b.kind = AssistantKind::Progress;
            }
        });
        assert_eq!(
            t.toggle_last_collapsible(|_| false),
            Some(ToggledBlock::Tool(true)),
            "an unfolded progress block must not swallow the key"
        );
    }

    fn group(expanded: bool) -> ToolGroupBlock {
        ToolGroupBlock {
            calls: vec![ToolCallBlock {
                id: ToolCallId::new("t1"),
                name: "read_file".into(),
                arguments: r#"{"path":"a"}"#.into(),
                status: ToolStatus::Ok,
                preview: Some("ok".into()),
                duration_ms: Some(1),
                parallel: false,
                started_elapsed_secs: 0,
                applied_diff: None,
            }],
            open: false,
            expanded,
        }
    }

    #[test]
    fn toggle_last_collapsible_only_flips_latest() {
        let mut ts = TranscriptState::default();
        ts.items.push(TranscriptItem::ToolGroup(group(false)));
        ts.items.push(TranscriptItem::ToolGroup(group(false)));

        let new = ts.toggle_last_collapsible(|_| false);
        assert_eq!(new, Some(ToggledBlock::Tool(true)));

        let groups: Vec<_> = ts
            .items
            .iter()
            .filter_map(|i| match i {
                TranscriptItem::ToolGroup(g) => Some(g.expanded),
                _ => None,
            })
            .collect();
        assert_eq!(groups, vec![false, true], "only latest group expands");

        let new = ts.toggle_last_collapsible(|_| false);
        assert_eq!(new, Some(ToggledBlock::Tool(false)));
        let groups: Vec<_> = ts
            .items
            .iter()
            .filter_map(|i| match i {
                TranscriptItem::ToolGroup(g) => Some(g.expanded),
                _ => None,
            })
            .collect();
        assert_eq!(groups, vec![false, false]);
    }

    #[test]
    fn completing_a_sub_agent_without_a_projection_is_unmeasured_not_zero() {
        let mut ts = TranscriptState::default();
        ts.push_sub_agent_started(
            "agent-1".into(),
            "Euclid".into(),
            "explorer".into(),
            "look around".into(),
            0,
        );
        ts.complete_sub_agent(
            "agent-1",
            "Euclid",
            true,
            "Structured findings adopted: f-1 — judge each with resolve_finding.".into(),
        );
        let block = match ts.items.last() {
            Some(TranscriptItem::SubAgent(b)) => b,
            _ => panic!("expected sub-agent"),
        };
        assert_eq!(
            block.contribution,
            crate::multi_agent::Contribution::NotMeasured,
            "no projection reached the UI; inventing a count from the summary is what this replaced"
        );
        assert_eq!(block.status, ToolStatus::Ok);
    }
}
