//! The transcript: a typed, growing list of conversation blocks.
//!
//! Not a `Vec<String>` . Each block renders and (later) folds
//! independently. It carries the blocks the base shell needs; extensions
//! add Tool/Plan/Diff/Verification/Attachment/Agent blocks.

use leveler_client_protocol::{MessageId, ToolCallId, UiCompletionReport};

use crate::markdown::MdDoc;

/// A streaming assistant message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistantBlock {
    pub id: MessageId,
    pub text: String,
    pub done: bool,
    /// Parsed markdown, computed once when the message completes (spec §62).
    pub rendered: Option<MdDoc>,
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
        };
        match self.items.last_mut() {
            Some(TranscriptItem::ToolGroup(group)) if group.open => group.calls.push(call),
            _ => self.items.push(TranscriptItem::ToolGroup(ToolGroupBlock {
                calls: vec![call],
                open: true,
                expanded: false,
            })),
        }
    }

    /// Complete a tool call, updating its status, preview, and duration.
    pub fn complete_tool(&mut self, id: &ToolCallId, ok: bool, preview: String, duration_ms: u64) {
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
            _ => None,
        };
        if toggled.is_some() {
            self.bump();
        }
        toggled
    }

    /// Toggle expand/collapse on whichever collapsible block came last.
    ///
    /// Tool groups and sub-agents both fold their detail away once finished, so
    /// one key opens whichever one you are looking at. Returns the new expanded
    /// state, or `None` when there is nothing collapsible.
    pub fn toggle_last_tool_group(&mut self) -> Option<bool> {
        self.bump();
        for item in self.items.iter_mut().rev() {
            match item {
                TranscriptItem::ToolGroup(group) => {
                    group.expanded = !group.expanded;
                    return Some(group.expanded);
                }
                TranscriptItem::SubAgent(block) => {
                    block.expanded = !block.expanded;
                    return Some(block.expanded);
                }
                _ => {}
            }
        }
        None
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

        t.complete_tool(&ToolCallId::new("t1"), true, "ok".into(), 1);
        let v3 = t.version();
        assert!(v3 > v2, "complete_tool must bump");

        // In-place mutation via the slice escape hatch must also invalidate.
        let _ = t.items_mut();
        assert!(t.version() > v3, "items_mut must bump");
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
            }],
            open: false,
            expanded,
        }
    }

    #[test]
    fn toggle_last_tool_group_only_flips_latest() {
        let mut ts = TranscriptState::default();
        ts.items.push(TranscriptItem::ToolGroup(group(false)));
        ts.items.push(TranscriptItem::ToolGroup(group(false)));

        let new = ts.toggle_last_tool_group();
        assert_eq!(new, Some(true));

        let groups: Vec<_> = ts
            .items
            .iter()
            .filter_map(|i| match i {
                TranscriptItem::ToolGroup(g) => Some(g.expanded),
                _ => None,
            })
            .collect();
        assert_eq!(groups, vec![false, true], "only latest group expands");

        let new = ts.toggle_last_tool_group();
        assert_eq!(new, Some(false));
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
