//! Session transcript search and turn navigation.
//!
//! `/find` answers "where did this session mention X" and `/jump` answers
//! "take me back to turn N". Both are views over the SAME transcript this
//! client already holds and the SAME durable artifacts its tool rows reference:
//! the module owns no second history model and sends nothing to the model.
//!
//! Three facts bound what a search can honestly claim:
//!
//! * the transcript is what the runtime replayed into this client (a reopened
//!   session's recent turns; older turns were never loaded and cannot be
//!   navigated to),
//! * a tool result above the model's output budget lives on disk and is
//!   scanned where it lies, never copied into the transcript,
//! * an artifact the runtime has already swept can no longer be searched, and
//!   the view says so instead of treating the missing bytes as "no match".
//!
//! Model reasoning is deliberately NOT indexed: the search box must not become
//! a second way to read private reasoning, even when a Thought block is
//! collapsed in the transcript.

pub mod artifacts;
pub mod highlight;
pub mod matching;
pub mod render;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use leveler_client_protocol::{MessageId, NotificationLevel, ToolCallId};
use leveler_core::UserShellId;

use crate::action::Effect;
use crate::i18n::UiText;
use crate::screen::Screen;
use crate::state::{AppState, Notification, WorkbenchFocus};
use crate::transcript::{
    AssistantBlock, SubAgentBlock, ToolCallBlock, TranscriptItem, TurnEndStatus, UserShellBlock,
};

/// Most rows one search may return across the whole session.
pub const MAX_RESULTS: usize = 500;
/// Most matches one document may contribute before the result set is marked
/// truncated.
pub const MAX_RESULTS_PER_DOC: usize = 20;
/// Longest accepted query. Far beyond any real search term; a paste of a whole
/// file is not a query.
pub const MAX_QUERY_CHARS: usize = 200;
/// Characters of context kept on each side of a result's match.
pub const SNIPPET_CONTEXT: usize = 80;

/// Which page of the Find surface is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FindTab {
    #[default]
    Search,
    Turns,
}

/// What kind of authored content a result came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FindSource {
    User,
    Assistant,
    ToolName,
    ToolArguments,
    ToolOutput,
    Edit,
    Command,
    CommandOutput,
    Error,
    Notice,
    Recap,
    Child,
    Plan,
    Review,
}

impl FindSource {
    pub fn label(self, t: &UiText) -> &'static str {
        let f = &t.find;
        match self {
            Self::User => f.source_user,
            Self::Assistant => f.source_assistant,
            Self::ToolName => f.source_tool,
            Self::ToolArguments => f.source_arguments,
            Self::ToolOutput => f.source_output,
            Self::Edit => f.source_edit,
            Self::Command => f.source_command,
            Self::CommandOutput => f.source_command_output,
            Self::Error => f.source_error,
            Self::Notice => f.source_notice,
            Self::Recap => f.source_recap,
            Self::Child => f.source_child,
            Self::Plan => f.source_plan,
            Self::Review => f.source_review,
        }
    }
}

/// Which on-disk authority an artifact path belongs to. A tool result is a
/// content-addressed store file; a local command's complete output is a
/// validated capture directory. They have different roots and different
/// validators, so the kind travels with the hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArtifactKind {
    ToolOutput,
    ShellCapture,
}

/// Where an artifact match opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactHitTarget {
    pub path: Arc<PathBuf>,
    pub kind: ArtifactKind,
    /// One-based line in the artifact.
    pub line: usize,
    /// Human label for the output page's title.
    pub title: Arc<str>,
}

/// One search result, carrying enough identity to survive a replay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindHit {
    /// Transcript item the content belongs to, as this corpus was built.
    pub item: usize,
    /// Call index inside the item's `ToolGroup`.
    pub call: Option<usize>,
    pub call_id: Option<ToolCallId>,
    pub shell_id: Option<UserShellId>,
    pub source: FindSource,
    /// One-based user-turn ordinal the content sits in; 0 when none.
    pub turn: usize,
    /// Index of this occurrence inside its document.
    pub occurrence: usize,
    pub snippet: String,
    pub match_start: usize,
    pub match_end: usize,
    pub artifact: Option<ArtifactHitTarget>,
    /// Message identity for user/assistant rows.
    pub id: Option<MessageId>,
    /// Fingerprint of the authored text, so a legacy `User(String)` row with
    /// no id is recognized after a replay instead of silently pointing at
    /// whatever now occupies its old index.
    pub text_fingerprint: u64,
}

/// Stable equality for a result across a re-sort. It names the content, not
/// the row position.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FindHitKey {
    pub item: usize,
    pub call_id: Option<ToolCallId>,
    pub shell_id: Option<UserShellId>,
    pub source: FindSource,
    pub occurrence: usize,
    pub artifact_line: Option<usize>,
}

impl FindHit {
    pub fn key(&self) -> FindHitKey {
        FindHitKey {
            item: self.item,
            call_id: self.call_id.clone(),
            shell_id: self.shell_id.clone(),
            source: self.source,
            occurrence: self.occurrence,
            artifact_line: self.artifact.as_ref().map(|hit| hit.line),
        }
    }
}

/// One user turn as the Turns page lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnEntry {
    /// One-based position inside the LOADED transcript.
    pub ordinal: usize,
    pub item: usize,
    pub id: Option<MessageId>,
    pub preview: String,
    /// The turn's terminal, when the log carried one.
    pub status: Option<TurnEndStatus>,
    pub elapsed_secs: Option<u64>,
    /// The newest user turn while its turn is still running.
    pub running: bool,
}

/// One searchable document, built from the transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindDoc {
    pub item: usize,
    pub call: Option<usize>,
    pub call_id: Option<ToolCallId>,
    pub shell_id: Option<UserShellId>,
    pub source: FindSource,
    pub turn: usize,
    pub text: String,
    pub id: Option<MessageId>,
    pub text_fingerprint: u64,
}

/// An artifact the search must scan on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactRequest {
    pub item: usize,
    pub call: Option<usize>,
    pub call_id: Option<ToolCallId>,
    pub shell_id: Option<UserShellId>,
    pub path: Arc<PathBuf>,
    pub title: Arc<str>,
    pub kind: ArtifactKind,
}

/// A snapshot of the transcript's searchable content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindCorpus {
    /// Transcript version the documents were built from.
    pub version: u64,
    pub docs: Vec<FindDoc>,
    pub turns: Vec<TurnEntry>,
    pub artifacts: Vec<ArtifactRequest>,
    /// Outputs that only exist in truncated form (swept artifact, or a result
    /// the runtime elided with no store). Their text was searched, but the
    /// search cannot claim to have seen the whole result.
    pub partial_outputs: usize,
    /// Turns the runtime left out of the replay, when it said so.
    pub omitted_turns: u32,
}

impl FindCorpus {
    pub fn doc_count(&self) -> usize {
        self.docs.len()
    }
}

/// Concurrency identity shared by the reducer and every search worker.
///
/// Equality is pointer identity: the effect carrying it and the page state
/// must be the SAME flag, not two equal counters.
#[derive(Debug, Clone)]
pub struct FindCancel(Arc<AtomicU64>);

impl FindCancel {
    fn new() -> Self {
        Self(Arc::new(AtomicU64::new(0)))
    }

    pub fn store(&self, generation: u64) {
        self.0.store(generation, Ordering::Relaxed);
    }

    pub fn load(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }

    pub fn atomic(&self) -> &AtomicU64 {
        &self.0
    }
}

impl PartialEq for FindCancel {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for FindCancel {}

/// Reader-facing state of the `/find` page.
#[derive(Debug)]
pub struct FindState {
    pub tab: FindTab,
    pub query: String,
    pub selected_search: usize,
    pub selected_turn: usize,
    pub hits: Vec<FindHit>,
    pub corpus: Option<Arc<FindCorpus>>,
    /// Identity of the current search. A completion carrying any other value
    /// is stale and must not replace what is on screen.
    pub generation: u64,
    /// Every worker checks this before each document / chunk. Bumping it
    /// cancels work nobody is waiting for any more.
    pub cancel: FindCancel,
    pub searching: bool,
    pub capped: bool,
    pub artifacts_total: usize,
    pub artifacts_done: usize,
    pub artifacts_unavailable: usize,
    pub partial_outputs: usize,
    pub return_focus: WorkbenchFocus,
    /// A page-local explanation (expired result, missing artifact).
    pub notice: Option<String>,
}

impl Default for FindState {
    fn default() -> Self {
        Self {
            tab: FindTab::Search,
            query: String::new(),
            selected_search: 0,
            selected_turn: 0,
            hits: Vec::new(),
            corpus: None,
            generation: 0,
            cancel: FindCancel::new(),
            searching: false,
            capped: false,
            artifacts_total: 0,
            artifacts_done: 0,
            artifacts_unavailable: 0,
            partial_outputs: 0,
            return_focus: WorkbenchFocus::Input,
            notice: None,
        }
    }
}

impl FindState {
    /// Cancel any in-flight search and forget its results.
    pub fn reset(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.cancel.store(self.generation);
        self.query.clear();
        self.selected_search = 0;
        self.selected_turn = 0;
        self.hits.clear();
        self.corpus = None;
        self.searching = false;
        self.capped = false;
        self.artifacts_total = 0;
        self.artifacts_done = 0;
        self.artifacts_unavailable = 0;
        self.partial_outputs = 0;
        self.notice = None;
    }
}

/// The browser-side highlight after a result is opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindHighlight {
    pub item: usize,
    pub call: Option<ToolCallId>,
    pub query: String,
}

/// Appended text (IME / paste burst) while the Find page owns the keyboard.
pub fn push_text(state: &mut AppState, text: &str) -> Vec<Effect> {
    let flat: String = text
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect();
    if flat.is_empty() {
        return Vec::new();
    }
    if state.find.tab != FindTab::Search {
        state.find.tab = FindTab::Search;
    }
    let room = MAX_QUERY_CHARS.saturating_sub(state.find.query.chars().count());
    state.find.query.extend(flat.chars().take(room));
    refresh(state)
}

/// Open the Find page, optionally with a query already typed.
pub fn open(state: &mut AppState, query: &str, tab: FindTab) -> Vec<Effect> {
    if state.active_screen != Screen::Find {
        state.find.return_focus = state.workbench_focus;
    }
    state.active_screen = Screen::Find;
    state.detail_return = Some(Screen::Conversation);
    state.screen_scroll = 0;
    state.find_highlight = None;
    state.find.tab = tab;
    state.find.query = query.chars().take(MAX_QUERY_CHARS).collect();
    state.find.selected_search = 0;
    state.find.selected_turn = 0;
    state.find.notice = None;
    refresh(state)
}

/// A `/jump` with no argument: the Turns page.
pub fn open_turns(state: &mut AppState) -> Vec<Effect> {
    open(state, "", FindTab::Turns)
}

/// `/jump <turn>`: jump straight to a user turn, validating the range.
pub fn jump_slash(state: &mut AppState, command: &str) -> Vec<Effect> {
    let Some(argument) = command
        .split_once(char::is_whitespace)
        .map(|(_, rest)| rest.trim())
    else {
        return open_turns(state);
    };
    if argument.is_empty() {
        return open_turns(state);
    }
    let Ok(ordinal) = argument.parse::<usize>() else {
        state.notification = Some(Notification {
            level: NotificationLevel::Warning,
            message: state.t().find.jump_usage.to_string(),
        });
        return Vec::new();
    };
    jump_to_turn(state, ordinal)
}

/// Jump to a 1-based turn ordinal inside the loaded transcript.
pub fn jump_to_turn(state: &mut AppState, ordinal: usize) -> Vec<Effect> {
    let turns = turn_entries(state);
    if ordinal == 0 || ordinal > turns.len() {
        let message = state
            .t()
            .find
            .jump_range
            .replace("{}", &turns.len().max(1).to_string());
        state.notification = Some(Notification {
            level: NotificationLevel::Warning,
            message,
        });
        return Vec::new();
    }
    let entry = turns[ordinal - 1].clone();
    let total = turns.len();
    reveal_item(state, entry.item, None, None);
    if state.active_screen == Screen::Find {
        close(state);
    }
    let message = state
        .t()
        .find
        .jump_done
        .replacen("{}", &ordinal.to_string(), 1)
        .replacen("{}", &total.to_string(), 1);
    state.notification = Some(Notification {
        level: NotificationLevel::Info,
        message,
    });
    state.find_highlight = None;
    Vec::new()
}

/// Close the page and cancel any search it owns.
pub fn close(state: &mut AppState) {
    let focus = state.find.return_focus;
    state.find.reset();
    state.find_highlight = None;
    if state.active_screen == Screen::Find {
        state.active_screen = Screen::Conversation;
        state.screen_scroll = 0;
    }
    state.workbench_focus = focus;
}

/// Rebuild the corpus if the transcript moved, then restart the search for the
/// current query.
pub fn refresh(state: &mut AppState) -> Vec<Effect> {
    let version = state.transcript.version();
    let stale = state
        .find
        .corpus
        .as_ref()
        .is_none_or(|corpus| corpus.version != version);
    if stale {
        let corpus = Arc::new(build_corpus(state));
        state.find.corpus = Some(corpus);
    }
    let Some(corpus) = state.find.corpus.clone() else {
        return Vec::new();
    };

    let generation = state.find.generation.wrapping_add(1);
    state.find.generation = generation;
    state.find.cancel.store(generation);
    state.find.hits.clear();
    state.find.capped = false;
    state.find.searching = false;
    state.find.selected_search = 0;
    state.find.artifacts_total = 0;
    state.find.artifacts_done = 0;
    state.find.artifacts_unavailable = 0;
    state.find.partial_outputs = 0;
    state.find.notice = None;

    let query = state.find.query.clone();
    if query.trim().is_empty() {
        return Vec::new();
    }
    state.find.searching = true;
    state.find.partial_outputs = corpus.partial_outputs;
    state.find.artifacts_total = corpus.artifacts.len();
    let mut effects = vec![Effect::RunFindTranscript {
        generation,
        corpus: corpus.clone(),
        query: query.clone(),
    }];
    if !corpus.artifacts.is_empty() {
        effects.push(Effect::RunFindArtifacts {
            generation,
            cancel: state.find.cancel.clone(),
            requests: corpus.artifacts.clone(),
            query,
        });
    }
    effects
}

/// Sort results into transcript order, keeping the reader's selected row.
pub fn apply_transcript_results(
    state: &mut AppState,
    generation: u64,
    hits: Vec<FindHit>,
    capped: bool,
) {
    if state.find.generation != generation {
        return;
    }
    let selected = state
        .find
        .hits
        .get(state.find.selected_search)
        .map(FindHit::key);
    state.find.hits = hits;
    state.find.capped |= capped;
    sort_hits(&mut state.find.hits);
    if state.find.hits.len() > MAX_RESULTS {
        state.find.hits.truncate(MAX_RESULTS);
        state.find.capped = true;
    }
    restore_selection(state, selected);
    state.find.searching = false;
}

/// Fold one artifact's scan into the result list.
///
/// `ReadError` is a real answer about availability, never "no matches": it is
/// counted and shown in the page's scope line.
pub fn apply_artifact_result(
    state: &mut AppState,
    generation: u64,
    request: &ArtifactRequest,
    outcome: artifacts::ScanOutcome,
) {
    if state.find.generation != generation {
        return;
    }
    let selected = state
        .find
        .hits
        .get(state.find.selected_search)
        .map(FindHit::key);
    state.find.artifacts_done += 1;
    match &outcome.status {
        artifacts::ArtifactStatus::Cancelled => return,
        artifacts::ArtifactStatus::ReadError(_) => {
            state.find.artifacts_unavailable += 1;
            return;
        }
        artifacts::ArtifactStatus::Capped => state.find.capped = true,
        artifacts::ArtifactStatus::Scanned => {}
    }
    for (occurrence, hit) in outcome.hits.into_iter().enumerate() {
        state.find.hits.push(FindHit {
            item: request.item,
            call: request.call,
            call_id: request.call_id.clone(),
            shell_id: request.shell_id.clone(),
            source: if request.shell_id.is_some() {
                FindSource::CommandOutput
            } else {
                FindSource::ToolOutput
            },
            turn: turn_for_item(state, request.item),
            occurrence,
            snippet: hit.snippet,
            match_start: hit.match_start,
            match_end: hit.match_end,
            artifact: Some(ArtifactHitTarget {
                path: request.path.clone(),
                kind: request.kind,
                line: hit.line,
                title: request.title.clone(),
            }),
            id: None,
            text_fingerprint: 0,
        });
    }
    sort_hits(&mut state.find.hits);
    if state.find.hits.len() > MAX_RESULTS {
        state.find.hits.truncate(MAX_RESULTS);
        state.find.capped = true;
    }
    restore_selection(state, selected);
}

fn sort_hits(hits: &mut [FindHit]) {
    hits.sort_by(|left, right| {
        (
            left.item,
            left.call,
            left.source,
            left.occurrence,
            left.artifact.as_ref().map(|hit| hit.line),
        )
            .cmp(&(
                right.item,
                right.call,
                right.source,
                right.occurrence,
                right.artifact.as_ref().map(|hit| hit.line),
            ))
    });
}

fn restore_selection(state: &mut AppState, selected: Option<FindHitKey>) {
    let position = selected.and_then(|key| state.find.hits.iter().position(|hit| hit.key() == key));
    state.find.selected_search = position.unwrap_or_else(|| {
        state
            .find
            .selected_search
            .min(state.find.hits.len().saturating_sub(1))
    });
}

/// Search the transcript documents. Runs off the paint loop.
pub fn search_corpus(corpus: &FindCorpus, query: &str) -> (Vec<FindHit>, bool) {
    let mut hits = Vec::new();
    let mut capped = false;
    for doc in &corpus.docs {
        let matches = matching::find_all(&doc.text, query, MAX_RESULTS_PER_DOC + 1);
        if matches.len() > MAX_RESULTS_PER_DOC {
            capped = true;
        }
        for (occurrence, found) in matches.into_iter().take(MAX_RESULTS_PER_DOC).enumerate() {
            let (snippet, match_start, match_end) =
                matching::snippet(&doc.text, found, SNIPPET_CONTEXT);
            hits.push(FindHit {
                item: doc.item,
                call: doc.call,
                call_id: doc.call_id.clone(),
                shell_id: doc.shell_id.clone(),
                source: doc.source,
                turn: doc.turn,
                occurrence,
                snippet,
                match_start,
                match_end,
                artifact: None,
                id: doc.id.clone(),
                text_fingerprint: doc.text_fingerprint,
            });
            if hits.len() >= MAX_RESULTS {
                return (hits, true);
            }
        }
    }
    (hits, capped)
}

/// Build the searchable projection of the current transcript.
pub fn build_corpus(state: &AppState) -> FindCorpus {
    let items = state.transcript.items();
    let turns = build_turns(items, state.is_busy());
    let mut docs = Vec::new();
    let mut requests = Vec::new();
    let mut seen_paths: HashSet<PathBuf> = HashSet::new();
    let mut partial_outputs = 0usize;
    let mut turn = 0usize;

    for (index, item) in items.iter().enumerate() {
        match item {
            TranscriptItem::User(text) => {
                turn += 1;
                push_doc(
                    &mut docs,
                    index,
                    None,
                    None,
                    None,
                    FindSource::User,
                    turn,
                    text,
                    None,
                );
            }
            TranscriptItem::UserInput(block) => {
                turn += 1;
                push_doc(
                    &mut docs,
                    index,
                    None,
                    None,
                    None,
                    FindSource::User,
                    turn,
                    &block.text,
                    Some(block.id.clone()),
                );
            }
            TranscriptItem::Assistant(block) => {
                push_assistant(&mut docs, index, turn, block);
            }
            // Deliberately absent: private model reasoning is not a search
            // surface. See the module docs.
            TranscriptItem::Thought(_) => {}
            TranscriptItem::ToolGroup(group) => {
                for (call_index, call) in group.calls.iter().enumerate() {
                    push_tool_call(
                        state,
                        &mut docs,
                        &mut requests,
                        &mut seen_paths,
                        &mut partial_outputs,
                        index,
                        call_index,
                        turn,
                        call,
                    );
                }
            }
            TranscriptItem::SubAgent(block) => {
                push_sub_agent(&mut docs, index, turn, block);
            }
            TranscriptItem::UserShell(shell) => {
                push_user_shell(
                    state,
                    &mut docs,
                    &mut requests,
                    &mut seen_paths,
                    &mut partial_outputs,
                    index,
                    turn,
                    shell,
                );
            }
            TranscriptItem::Plan(plan) => {
                let text = plan
                    .steps
                    .iter()
                    .map(|step| step.description.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                push_doc(
                    &mut docs,
                    index,
                    None,
                    None,
                    None,
                    FindSource::Plan,
                    turn,
                    &text,
                    None,
                );
            }
            TranscriptItem::Error(text) => push_doc(
                &mut docs,
                index,
                None,
                None,
                None,
                FindSource::Error,
                turn,
                text,
                None,
            ),
            TranscriptItem::Failure(block) => {
                let text = format!(
                    "{}\n{}\n{}\n{}",
                    block.title,
                    block.summary,
                    block.subtitle.as_deref().unwrap_or(""),
                    block.detail
                );
                push_doc(
                    &mut docs,
                    index,
                    None,
                    None,
                    None,
                    FindSource::Error,
                    turn,
                    &text,
                    None,
                );
            }
            TranscriptItem::Note(text) => push_doc(
                &mut docs,
                index,
                None,
                None,
                None,
                FindSource::Notice,
                turn,
                text,
                None,
            ),
            TranscriptItem::AwaySummary(text) => push_doc(
                &mut docs,
                index,
                None,
                None,
                None,
                FindSource::Recap,
                turn,
                text,
                None,
            ),
            TranscriptItem::Recap(block) => {
                let text = format!(
                    "{}\n{}",
                    block.summary.as_deref().unwrap_or(""),
                    block.next_step.as_deref().unwrap_or("")
                );
                push_doc(
                    &mut docs,
                    index,
                    None,
                    None,
                    None,
                    FindSource::Recap,
                    turn,
                    &text,
                    None,
                );
            }
            TranscriptItem::GoalRecap(block) => {
                let recap = &block.recap;
                let mut text = recap.display_summary.clone();
                if let Some(next) = &recap.next_action {
                    text.push('\n');
                    text.push_str(next);
                }
                for milestone in &recap.completed_milestones {
                    text.push('\n');
                    text.push_str(milestone);
                }
                push_doc(
                    &mut docs,
                    index,
                    None,
                    None,
                    None,
                    FindSource::Recap,
                    turn,
                    &text,
                    None,
                );
            }
            TranscriptItem::Review(block) => push_doc(
                &mut docs,
                index,
                None,
                None,
                None,
                FindSource::Review,
                turn,
                &block.summary.focus,
                None,
            ),
            TranscriptItem::Completion(_) | TranscriptItem::TurnEnd(_) => {}
        }
    }

    FindCorpus {
        version: state.transcript.version(),
        docs,
        turns,
        artifacts: requests,
        partial_outputs,
        omitted_turns: state.history_omitted_turns,
    }
}

#[allow(clippy::too_many_arguments)]
fn push_tool_call(
    state: &AppState,
    docs: &mut Vec<FindDoc>,
    requests: &mut Vec<ArtifactRequest>,
    seen_paths: &mut HashSet<PathBuf>,
    partial_outputs: &mut usize,
    item: usize,
    call_index: usize,
    turn: usize,
    call: &ToolCallBlock,
) {
    let call_id = Some(call.id.clone());
    if !call.name.is_empty() {
        push_doc(
            docs,
            item,
            Some(call_index),
            call_id.clone(),
            None,
            FindSource::ToolName,
            turn,
            &call.name,
            None,
        );
    }
    if !call.arguments.trim().is_empty() {
        push_doc(
            docs,
            item,
            Some(call_index),
            call_id.clone(),
            None,
            FindSource::ToolArguments,
            turn,
            &call.arguments,
            None,
        );
    }
    if let Some(diff) = call.applied_diff.as_deref()
        && !diff.trim().is_empty()
    {
        push_doc(
            docs,
            item,
            Some(call_index),
            call_id.clone(),
            None,
            FindSource::Edit,
            turn,
            diff,
            None,
        );
    }

    let mut artifact_backed = false;
    if let Some(preview) = call.preview.as_deref() {
        // A command tool externalizes each stream itself and appends a
        // locator line, long before the model-facing result cap would run.
        // That is a second, equally authoritative pointer format and the
        // search must follow it too.
        let spill_paths = command_spill_paths(preview);
        let spilled = !spill_paths.is_empty();
        let mut spill_incomplete = false;
        for (stream, raw) in spill_paths {
            if let Some(path) = crate::artifact_ref::validated_tool_output(state, &raw) {
                queue_artifact(
                    requests,
                    seen_paths,
                    item,
                    Some(call_index),
                    call_id.clone(),
                    None,
                    &path,
                    format!("{} · {}", call.name, stream).as_str(),
                    turn,
                    ArtifactKind::ToolOutput,
                );
            } else {
                spill_incomplete = true;
            }
        }
        if spill_incomplete {
            *partial_outputs += 1;
        }
        if let Some(raw) = crate::artifact_ref::saved_tool_output_path(preview) {
            if let Some(path) = crate::artifact_ref::validated_tool_output(state, &raw) {
                queue_artifact(
                    requests,
                    seen_paths,
                    item,
                    Some(call_index),
                    call_id.clone(),
                    None,
                    &path,
                    format!(
                        "{} · {}",
                        call.name,
                        crate::render::truncate_display(
                            call.arguments.lines().next().unwrap_or("").trim(),
                            48
                        )
                    )
                    .as_str(),
                    turn,
                    ArtifactKind::ToolOutput,
                );
                artifact_backed = true;
            } else {
                *partial_outputs += 1;
            }
        } else if has_elision_marker(preview) && !spilled {
            // An elision with no stream locator really did lose its middle.
            // One WITH a valid locator is fully searchable in the artifact.
            *partial_outputs += 1;
        }
        if !artifact_backed {
            push_doc(
                docs,
                item,
                Some(call_index),
                call_id,
                None,
                FindSource::ToolOutput,
                turn,
                preview,
                None,
            );
        }
    } else if !call.output.is_empty() {
        if call.output_truncated {
            *partial_outputs += 1;
        }
        push_doc(
            docs,
            item,
            Some(call_index),
            call_id,
            None,
            FindSource::ToolOutput,
            turn,
            &call.output,
            None,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn push_user_shell(
    state: &AppState,
    docs: &mut Vec<FindDoc>,
    requests: &mut Vec<ArtifactRequest>,
    seen_paths: &mut HashSet<PathBuf>,
    partial_outputs: &mut usize,
    item: usize,
    turn: usize,
    shell: &UserShellBlock,
) {
    push_doc(
        docs,
        item,
        None,
        None,
        Some(shell.id.clone()),
        FindSource::Command,
        turn,
        &shell.command,
        None,
    );
    let mut artifact_backed = false;
    if let Some(directory) = shell.full_output.as_deref() {
        match crate::artifact_ref::validated_shell_capture(state, directory) {
            Ok(capture) => {
                queue_artifact(
                    requests,
                    seen_paths,
                    item,
                    None,
                    None,
                    Some(shell.id.clone()),
                    &capture.combined,
                    format!(
                        "{} · 输出",
                        crate::render::truncate_display(
                            shell.command.lines().next().unwrap_or("").trim(),
                            48
                        )
                    )
                    .as_str(),
                    turn,
                    ArtifactKind::ShellCapture,
                );
                artifact_backed = true;
            }
            Err(_) => {
                if shell.output_truncated {
                    *partial_outputs += 1;
                }
            }
        }
    } else if shell.output_truncated {
        *partial_outputs += 1;
    }
    if !artifact_backed && !shell.output.is_empty() {
        push_doc(
            docs,
            item,
            None,
            None,
            Some(shell.id.clone()),
            FindSource::CommandOutput,
            turn,
            &shell.output,
            None,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn queue_artifact(
    requests: &mut Vec<ArtifactRequest>,
    seen_paths: &mut HashSet<PathBuf>,
    item: usize,
    call: Option<usize>,
    call_id: Option<ToolCallId>,
    shell_id: Option<UserShellId>,
    path: &PathBuf,
    title: &str,
    turn: usize,
    kind: ArtifactKind,
) {
    if !seen_paths.insert(path.clone()) {
        // Content-addressed storage can hand two rows the same file. Scanning
        // it twice would duplicate every match; the first transcript row owns
        // the result.
        return;
    }
    requests.push(ArtifactRequest {
        item,
        call,
        call_id,
        shell_id,
        path: Arc::new(path.clone()),
        title: Arc::from(format!("{title} · T{turn:02}")),
        kind,
    });
}

fn push_assistant(docs: &mut Vec<FindDoc>, item: usize, turn: usize, block: &AssistantBlock) {
    if block.text.trim().is_empty() {
        return;
    }
    docs.push(FindDoc {
        item,
        call: None,
        call_id: None,
        shell_id: None,
        source: FindSource::Assistant,
        turn,
        text: block.text.clone(),
        id: Some(block.id.clone()),
        text_fingerprint: fingerprint(&block.text),
    });
}

fn push_sub_agent(docs: &mut Vec<FindDoc>, item: usize, turn: usize, block: &SubAgentBlock) {
    // Only what the conversation paints: the spawn prompt (`task`) is an
    // instruction, not a caption, and indexing it would surface text the
    // reader was never shown.
    let mut text = String::new();
    if let Some(title) = &block.title {
        text.push_str(title);
        text.push('\n');
    }
    text.push_str(&block.detail);
    if let Some(step) = &block.recent_step {
        text.push('\n');
        text.push_str(step);
    }
    if text.trim().is_empty() {
        return;
    }
    docs.push(FindDoc {
        item,
        call: None,
        call_id: None,
        shell_id: None,
        source: FindSource::Child,
        turn,
        text: text.clone(),
        id: None,
        text_fingerprint: fingerprint(&text),
    });
}

#[allow(clippy::too_many_arguments)]
fn push_doc(
    docs: &mut Vec<FindDoc>,
    item: usize,
    call: Option<usize>,
    call_id: Option<ToolCallId>,
    shell_id: Option<UserShellId>,
    source: FindSource,
    turn: usize,
    text: &str,
    id: Option<MessageId>,
) {
    if text.is_empty() {
        return;
    }
    docs.push(FindDoc {
        item,
        call,
        call_id,
        shell_id,
        source,
        turn,
        text: text.to_string(),
        text_fingerprint: fingerprint(text),
        id,
    });
}

/// Paths a command tool appended for the streams it could not fit inline:
/// `[stdout full output: /abs/path]`. Returns `(stream, path)` in preview
/// order.
fn command_spill_paths(preview: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in preview.lines() {
        for stream in ["stdout", "stderr"] {
            let prefix = format!("[{stream} full output: ");
            if let Some(rest) = line.strip_prefix(&prefix)
                && let Some(path) = rest.strip_suffix(']')
            {
                let path = path.trim();
                if !path.is_empty() {
                    out.push((stream.to_string(), path.to_string()));
                }
            }
        }
    }
    out
}

/// Both elision shapes this session can produce: the registry's
/// `… [N bytes (…) elided to fit the context] …` and the command tool's
/// `… [N of M bytes (…) elided] …`. A line that says it removed content makes
/// the preview partial, and the page must not call it complete.
fn has_elision_marker(text: &str) -> bool {
    text.lines()
        .any(|line| line.starts_with("… [") && line.ends_with("] …") && line.contains("elided"))
}

fn fingerprint(text: &str) -> u64 {
    // FNV-1a. Identity only: collisions are not an authority boundary here,
    // because a hit still has to resolve to an item of the same kind.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The loaded transcript's user turns, newest status included.
pub fn turn_entries(state: &AppState) -> Vec<TurnEntry> {
    build_turns(state.transcript.items(), state.is_busy())
}

fn build_turns(items: &[TranscriptItem], busy: bool) -> Vec<TurnEntry> {
    let mut turns = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let (id, text) = match item {
            TranscriptItem::User(text) => (None, text.as_str()),
            TranscriptItem::UserInput(block) => (Some(block.id.clone()), block.text.as_str()),
            _ => continue,
        };
        let preview = text
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("")
            .trim()
            .to_string();
        let mut status = None;
        let mut elapsed_secs = None;
        for later in &items[index + 1..] {
            match later {
                TranscriptItem::User(_) | TranscriptItem::UserInput(_) => break,
                TranscriptItem::TurnEnd(block) => {
                    status = Some(block.status);
                    elapsed_secs = Some(block.elapsed_secs);
                }
                _ => {}
            }
        }
        turns.push(TurnEntry {
            ordinal: turns.len() + 1,
            item: index,
            id,
            preview,
            status,
            elapsed_secs,
            running: false,
        });
    }
    if busy && let Some(last) = turns.last_mut() {
        last.running = last.status.is_none();
    }
    turns
}

fn turn_for_item(state: &AppState, item: usize) -> usize {
    let mut turn = 0;
    for (index, entry) in state.transcript.items().iter().enumerate() {
        if matches!(
            entry,
            TranscriptItem::User(_) | TranscriptItem::UserInput(_)
        ) {
            turn += 1;
        }
        if index >= item {
            return turn;
        }
    }
    turn
}

/// Rows the current tab may select.
fn selection_len(state: &AppState) -> usize {
    match state.find.tab {
        FindTab::Search => state.find.hits.len(),
        FindTab::Turns => turn_entries(state).len(),
    }
}

fn selection(state: &AppState) -> usize {
    match state.find.tab {
        FindTab::Search => state.find.selected_search,
        FindTab::Turns => state.find.selected_turn,
    }
}

fn set_selection(state: &mut AppState, value: usize) {
    match state.find.tab {
        FindTab::Search => state.find.selected_search = value,
        FindTab::Turns => state.find.selected_turn = value,
    }
}

fn move_selection(state: &mut AppState, delta: isize) {
    let len = selection_len(state);
    if len == 0 {
        return;
    }
    let current = selection(state).min(len - 1);
    let next = if delta < 0 {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        (current + delta as usize).min(len - 1)
    };
    set_selection(state, next);
}

fn page_size(state: &AppState) -> usize {
    (state.size.1 as usize).saturating_sub(8).max(1)
}

/// Keys for the Find page. Every binding here is consumed by the page: none
/// changes the turn, the goal or the runtime.
pub fn handle_key(state: &mut AppState, key: &KeyEvent) -> Vec<Effect> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key
        .modifiers
        .intersects(KeyModifiers::ALT | KeyModifiers::SUPER);
    match key.code {
        KeyCode::Esc => {
            close(state);
            Vec::new()
        }
        KeyCode::Tab | KeyCode::BackTab => {
            state.find.tab = match state.find.tab {
                FindTab::Search => FindTab::Turns,
                FindTab::Turns => FindTab::Search,
            };
            Vec::new()
        }
        // Arrow keys only: `j`/`k` are ordinary query characters here. A
        // search box that cannot contain "json" is not a search box.
        KeyCode::Up if !ctrl && !alt => {
            move_selection(state, -1);
            Vec::new()
        }
        KeyCode::Down if !ctrl && !alt => {
            move_selection(state, 1);
            Vec::new()
        }
        KeyCode::PageUp => {
            move_selection(state, -(page_size(state) as isize));
            Vec::new()
        }
        KeyCode::PageDown => {
            move_selection(state, page_size(state) as isize);
            Vec::new()
        }
        KeyCode::Enter => jump_selected(state),
        KeyCode::Backspace => {
            if state.find.tab != FindTab::Search {
                state.find.tab = FindTab::Search;
            }
            crate::help::pop_grapheme(&mut state.find.query);
            refresh(state)
        }
        KeyCode::Char('u') if ctrl => {
            if state.find.tab != FindTab::Search {
                state.find.tab = FindTab::Search;
            }
            state.find.query.clear();
            refresh(state)
        }
        KeyCode::Char(ch) if !ctrl && !alt && !ch.is_control() => {
            if state.find.tab != FindTab::Search {
                state.find.tab = FindTab::Search;
            }
            let room = MAX_QUERY_CHARS.saturating_sub(state.find.query.chars().count());
            if room > 0 {
                state.find.query.push(ch);
            }
            refresh(state)
        }
        _ => Vec::new(),
    }
}

/// A click on a published page row selects it.
pub fn select_row(state: &mut AppState, key: &str) {
    let Ok(index) = key.parse::<usize>() else {
        return;
    };
    let len = selection_len(state);
    if index < len {
        set_selection(state, index);
    }
}

/// Wheel motion moves the page's selection, the same as every other list page.
pub fn wheel(state: &mut AppState, notches: isize) {
    move_selection(state, notches);
}

fn jump_selected(state: &mut AppState) -> Vec<Effect> {
    match state.find.tab {
        FindTab::Search => {
            let Some(hit) = state.find.hits.get(state.find.selected_search).cloned() else {
                return Vec::new();
            };
            jump_hit(state, &hit)
        }
        FindTab::Turns => {
            let turns = turn_entries(state);
            let Some(entry) = turns.get(state.find.selected_turn).cloned() else {
                return Vec::new();
            };
            reveal_item(state, entry.item, None, None);
            close(state);
            state.find_highlight = None;
            Vec::new()
        }
    }
}

fn jump_hit(state: &mut AppState, hit: &FindHit) -> Vec<Effect> {
    if let Some(artifact) = &hit.artifact {
        // The artifact may have been swept between the scan and the jump, or
        // its path replaced. Re-validate against the authority it came from,
        // never just `is_file`: a tool path came from model-facing output.
        let validated = match artifact.kind {
            ArtifactKind::ToolOutput => {
                crate::artifact_ref::validated_file_under_state(state, &artifact.path)
            }
            ArtifactKind::ShellCapture => {
                crate::artifact_ref::revalidated_shell_capture(state, &artifact.path)
            }
        };
        let Some(path) = validated else {
            state.find.notice = Some(state.t().find.artifact_missing.to_string());
            state.find.artifacts_unavailable += 1;
            return Vec::new();
        };
        let bytes = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
        let source = crate::output_view::OutputSource::File { path, bytes };
        let width = crate::reducer::output_view_width(state);
        let effect_source = source.clone();
        let mut view =
            crate::output_view::OutputView::new(source, artifact.title.to_string(), width);
        view.generation = 1;
        view.target_line = Some(artifact.line);
        close(state);
        state.output_view = Some(view);
        state.active_screen = Screen::Output;
        state.detail_return = Some(Screen::Conversation);
        state.screen_scroll = 0;
        state.conv.selection.clear();
        return vec![Effect::LoadOutputIndex {
            source: effect_source,
            width,
            generation: 1,
        }];
    }

    let Some((item, call)) = resolve_target(state, hit) else {
        state.find.notice = Some(state.t().find.expired.to_string());
        return Vec::new();
    };
    let query = state.find.query.clone();
    reveal_item(state, item, call, Some(&query));
    close(state);
    // close() clears the transient highlight; the jump owns it again.
    state.find_highlight = Some(FindHighlight {
        item,
        call: hit.call_id.clone(),
        query,
    });
    Vec::new()
}

/// Resolve a hit against the CURRENT transcript. Identity wins over position:
/// a call is found by its id, a shell by its id, and an item without an id is
/// recognized by its authored text. A miss is reported, never guessed.
fn resolve_target(state: &AppState, hit: &FindHit) -> Option<(usize, Option<usize>)> {
    if let Some(id) = &hit.call_id {
        for (index, item) in state.transcript.items().iter().enumerate() {
            if let TranscriptItem::ToolGroup(group) = item
                && let Some(call) = group.calls.iter().position(|call| &call.id == id)
            {
                return Some((index, Some(call)));
            }
        }
        return None;
    }
    if let Some(id) = &hit.shell_id {
        for (index, item) in state.transcript.items().iter().enumerate() {
            if let TranscriptItem::UserShell(shell) = item
                && &shell.id == id
            {
                return Some((index, None));
            }
        }
        return None;
    }
    let item = state.transcript.items().get(hit.item)?;
    let matches = match (hit.source, item) {
        (FindSource::User, TranscriptItem::User(text)) => fingerprint(text) == hit.text_fingerprint,
        (FindSource::User, TranscriptItem::UserInput(block)) => hit.id.as_ref() == Some(&block.id),
        (FindSource::Assistant, TranscriptItem::Assistant(block)) => {
            hit.id.as_ref() == Some(&block.id)
        }
        (FindSource::Error, TranscriptItem::Error(_) | TranscriptItem::Failure(_)) => true,
        (FindSource::Notice, TranscriptItem::Note(_)) => true,
        (
            FindSource::Recap,
            TranscriptItem::AwaySummary(_)
            | TranscriptItem::Recap(_)
            | TranscriptItem::GoalRecap(_),
        ) => true,
        (FindSource::Child, TranscriptItem::SubAgent(block)) => {
            fingerprint(&sub_agent_text(block)) == hit.text_fingerprint
        }
        (FindSource::Plan, TranscriptItem::Plan(_)) => true,
        (FindSource::Review, TranscriptItem::Review(_)) => true,
        _ => false,
    };
    matches.then_some((hit.item, None))
}

fn sub_agent_text(block: &SubAgentBlock) -> String {
    let mut text = String::new();
    if let Some(title) = &block.title {
        text.push_str(title);
        text.push('\n');
    }
    text.push_str(&block.detail);
    if let Some(step) = &block.recent_step {
        text.push('\n');
        text.push_str(step);
    }
    text
}

/// Expand the target, scroll the matched row into view, and leave the
/// transcript ready to paint it.
fn reveal_item(state: &mut AppState, item: usize, call: Option<usize>, query: Option<&str>) {
    state.transcript.expand_item_display(item);
    if let Some(call) = call {
        state.transcript.set_call_expanded(item, call, true);
    }
    // The fold changed the projected lines; selection and URL hit-testing must
    // rebuild against what is now painted.
    state.conv.plain.clear();
    state.conv.plain_layout.clear();
    state.conv.plain_width = 0;

    let width = crate::conversation::geometry::content_width(state);
    let (lines, _, _, _, item_spans, sources, _) = state.conversation_build(width);
    let call_id = call.and_then(|call| match state.transcript.items().get(item) {
        Some(TranscriptItem::ToolGroup(group)) => group.calls.get(call).map(|call| call.id.clone()),
        _ => None,
    });
    let (start, end) = match call {
        Some(_) => {
            let ranges: Vec<(usize, usize)> = sources
                .iter()
                .filter(|source| source.call.as_ref() == call_id.as_ref() && source.item == item)
                .map(|source| (source.start, source.end))
                .collect();
            match (ranges.first(), ranges.last()) {
                (Some(first), Some(last)) => (first.0, last.1),
                _ => item_spans.get(item).copied().unwrap_or((0, 0)),
            }
        }
        None => item_spans.get(item).copied().unwrap_or((0, 0)),
    };
    let row = query
        .and_then(|query| find_row_in(&lines, start, end, query))
        .unwrap_or(start);
    let height = crate::conversation::geometry::viewport_height(state);
    let max_scroll = crate::conversation::geometry::max_scroll(lines.len(), height);
    state.conv.auto_scroll = false;
    state.conv.unread = 0;
    state.conv.scroll = row.saturating_sub(2).min(max_scroll);
}

/// First row in `[start, end)` whose painted text contains `query`, matched
/// case-insensitively like the search itself.
fn find_row_in(
    lines: &[ratatui::text::Line<'static>],
    start: usize,
    end: usize,
    query: &str,
) -> Option<usize> {
    lines
        .iter()
        .enumerate()
        .skip(start)
        .take(end.saturating_sub(start))
        .find(|(_, line)| {
            let plain: String = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect();
            !matching::find_all(&plain, query, 1).is_empty()
        })
        .map(|(row, _)| row)
}

/// The scope line the page shows under its tabs. Its whole job is to keep the
/// search's real coverage visible: omitted turns and incomplete artifacts are
/// part of the answer.
pub fn scope_line(state: &AppState, t: &UiText) -> String {
    let corpus = state.find.corpus.as_ref();
    let turns = corpus.map(|corpus| corpus.turns.len()).unwrap_or(0);
    let incomplete = state
        .find
        .partial_outputs
        .saturating_add(state.find.artifacts_unavailable);
    let mut line = t.find.scope_base.replace("{}", &turns.to_string());
    if state.find.artifacts_total > 0 {
        line.push_str(
            &t.find
                .scope_artifact
                .replacen("{}", &state.find.artifacts_total.to_string(), 1)
                .replacen("{}", &incomplete.to_string(), 1),
        );
    } else if incomplete > 0 {
        line.push_str(&t.find.scope_partial.replace("{}", &incomplete.to_string()));
    }
    if let Some(corpus) = corpus
        && corpus.omitted_turns > 0
    {
        line.push_str(
            &t.find
                .scope_omitted
                .replace("{}", &corpus.omitted_turns.to_string()),
        );
    }
    if corpus.is_some_and(|corpus| corpus.version != state.transcript.version()) {
        line.push_str(t.find.scope_stale);
    }
    line
}

/// Turn status label for the Turns page. Reuses the transcript's own wording
/// so one turn never gets two different verdicts.
pub fn turn_status_label(status: TurnEndStatus, t: &UiText) -> String {
    match status {
        TurnEndStatus::Completed => format!("✓ {}", t.turn_end_completed),
        TurnEndStatus::CompletedPartiallyUnverified => {
            format!("· {}", t.final_completed_partially_unverified)
        }
        TurnEndStatus::CompletedWithWarnings | TurnEndStatus::Truncated => {
            format!("⚠ {}", t.final_completed_warnings)
        }
        TurnEndStatus::Answered => format!("✓ {}", t.turn_end_answered),
        TurnEndStatus::AnsweredUnregistered => {
            format!("⚠ {}", t.turn_end_answered_unregistered)
        }
        TurnEndStatus::Incomplete => format!("⚠ {}", t.final_blocked),
        TurnEndStatus::Blocked => format!("⚠ {}", t.final_goal_blocked),
        TurnEndStatus::Yielded => format!("↻ {}", t.turn_end_yielded),
        TurnEndStatus::NoFinalAnswer => format!("⚠ {}", t.turn_no_final_answer),
        TurnEndStatus::Failed => format!("✗ {}", t.final_failed),
        TurnEndStatus::Cancelled => format!("⊘ {}", t.final_cancelled),
    }
}

/// Elapsed time as a compact `1m 12s` / `12s` label.
pub fn format_elapsed(secs: u64) -> String {
    let minutes = secs / 60;
    let seconds = secs % 60;
    if minutes > 0 {
        format!("{minutes}m {seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Boot;
    use leveler_client_protocol::SessionId;
    use std::path::Path;

    fn state(root: &Path) -> AppState {
        let mut state = AppState::new(
            crate::theme::Theme::no_color(),
            Boot {
                session_id: SessionId::new("find-session"),
                user: "u".into(),
                version: "0.1.0".into(),
                show_welcome: false,
                draft_path: Some(root.join("state").join("draft.txt")),
                history_path: None,
                context_window: 0,
                locale: crate::i18n::Locale::Zh,
                untrusted_config: Vec::new(),
                model_notice: None,
                thinking: None,
            },
        );
        state.size = (100, 40);
        state
    }

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "leveler-find-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("state").join("artifacts")).unwrap();
        root
    }

    fn sources_for(corpus: &FindCorpus, query: &str) -> Vec<FindSource> {
        let (hits, _) = search_corpus(corpus, query);
        hits.into_iter().map(|hit| hit.source).collect()
    }

    #[test]
    fn the_corpus_indexes_visible_content_and_never_reasoning() {
        let root = temp_root("corpus");
        let mut state = state(&root);
        state.transcript.push_user("Implement refund".into());
        let message = MessageId::new("a1");
        state.transcript.begin_assistant(message.clone());
        state
            .transcript
            .append_assistant(&message, "Refund workflow done");
        state.transcript.finish_assistant(&message);
        state.transcript.begin_thought();
        state.transcript.append_thought("private refund reasoning");
        state.transcript.finish_thought(Some(5));
        let call = ToolCallId::new("c1");
        state.transcript.push_tool_started(
            call.clone(),
            "run_command".into(),
            r#"{"command":"cargo test refund"}"#.into(),
            false,
            0,
            Some(1),
            None,
        );
        state
            .transcript
            .complete_tool(&call, true, "PASS refund_idempotency_test".into(), 5, None);
        state.transcript.push_error("refund failed loudly".into());
        state.transcript.push_note("refund note".into());

        let corpus = build_corpus(&state);
        let sources = sources_for(&corpus, "refund");
        for expected in [
            FindSource::User,
            FindSource::Assistant,
            FindSource::ToolArguments,
            FindSource::ToolOutput,
            FindSource::Error,
            FindSource::Notice,
        ] {
            assert!(
                sources.contains(&expected),
                "missing {expected:?}: {sources:?}"
            );
        }
        assert!(
            !corpus
                .docs
                .iter()
                .any(|doc| doc.text.contains("private refund reasoning")),
            "a Thought block must not be searchable"
        );
        assert_eq!(
            sources_for(&corpus, "run_command"),
            vec![FindSource::ToolName],
            "the tool name is its own searchable document"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn folded_tool_output_is_still_searchable() {
        let root = temp_root("folded");
        let mut state = state(&root);
        state.transcript.push_user("run tests".into());
        let call = ToolCallId::new("c1");
        state.transcript.push_tool_started(
            call.clone(),
            "run_command".into(),
            "{}".into(),
            false,
            0,
            Some(1),
            None,
        );
        state
            .transcript
            .complete_tool(&call, false, "test_session_resume FAILED".into(), 9, None);
        let item = state.transcript.items().len() - 1;
        state
            .transcript
            .set_item_display(item, crate::fold::DisplayMode::Collapsed);

        let corpus = build_corpus(&state);
        let (hits, _) = search_corpus(&corpus, "test_session_resume FAILED");
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].source, FindSource::ToolOutput);
        assert_eq!(hits[0].item, item);
        assert!(hits[0].call_id.is_some());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn artifact_backed_output_is_scanned_from_disk_not_duplicated_in_memory() {
        let root = temp_root("artifact");
        let artifact = root
            .join("state")
            .join("artifacts")
            .join("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef.txt");
        std::fs::write(&artifact, "head\nUNIQUE_ARTIFACT_TOKEN\ntail\n").unwrap();
        let mut state = state(&root);
        state.transcript.push_user("run long".into());
        let call = ToolCallId::new("c1");
        state.transcript.push_tool_started(
            call.clone(),
            "run_command".into(),
            "{}".into(),
            false,
            0,
            Some(1),
            None,
        );
        let reference =
            leveler_model::tool_output::saved_reference(artifact.to_str().unwrap(), "hash", 34);
        state
            .transcript
            .complete_tool(&call, true, format!("{reference}\nhead\ntail"), 5, None);
        let corpus = build_corpus(&state);
        assert_eq!(corpus.artifacts.len(), 1);
        assert!(
            !corpus
                .docs
                .iter()
                .any(|doc| doc.text.contains("UNIQUE_ARTIFACT_TOKEN")),
            "artifact text must not be copied into the corpus"
        );
        assert!(
            !corpus
                .docs
                .iter()
                .any(|doc| doc.source == FindSource::ToolOutput),
            "the bounded preview is not the record when the artifact exists"
        );

        let mut state = state;
        let request = corpus.artifacts[0].clone();
        state.find.generation = 1;
        let cancel = state.find.cancel.clone();
        cancel.store(1);
        let outcome =
            artifacts::scan_file(&request.path, "UNIQUE_ARTIFACT_TOKEN", cancel.atomic(), 1);
        apply_artifact_result(&mut state, 1, &request, outcome);
        assert_eq!(state.find.hits.len(), 1, "{:?}", state.find.hits);
        assert_eq!(state.find.hits[0].artifact.as_ref().unwrap().line, 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_command_stream_locator_is_scanned_and_the_preview_stays_searchable() {
        let root = temp_root("command-spill");
        let stdout = root
            .join("state")
            .join("artifacts")
            .join("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef.txt");
        std::fs::write(&stdout, "1\n2\nUNIQUE_STREAM_TOKEN\n").unwrap();
        let mut state = state(&root);
        state.transcript.push_user("run long".into());
        let call = ToolCallId::new("c1");
        state.transcript.push_tool_started(
            call.clone(),
            "shell_command".into(),
            "{}".into(),
            false,
            0,
            Some(1),
            None,
        );
        let preview = format!(
            "exit: 0\n--- stdout ---\n1\n… [12 of 34 bytes (~3 tokens) elided] …\n3\n[stdout full output: {}]\n",
            stdout.display()
        );
        state
            .transcript
            .complete_tool(&call, true, preview, 5, None);

        let corpus = build_corpus(&state);
        assert_eq!(corpus.artifacts.len(), 1, "{:?}", corpus.artifacts);
        assert_eq!(corpus.partial_outputs, 0, "the stream artifact is complete");
        // The visible preview is still indexed: its exit line and elision are
        // text the reader can see.
        let (preview_hits, _) = search_corpus(&corpus, "exit: 0");
        assert_eq!(preview_hits.len(), 1);
        assert_eq!(preview_hits[0].source, FindSource::ToolOutput);
        // The token exists only in the stream artifact.
        let (none, _) = search_corpus(&corpus, "UNIQUE_STREAM_TOKEN");
        assert!(none.is_empty());

        let mut state = state;
        state.find.generation = 1;
        state.find.cancel.store(1);
        let request = corpus.artifacts[0].clone();
        let outcome = artifacts::scan_file(
            &request.path,
            "UNIQUE_STREAM_TOKEN",
            state.find.cancel.atomic(),
            1,
        );
        apply_artifact_result(&mut state, 1, &request, outcome);
        assert_eq!(state.find.hits.len(), 1, "{:?}", state.find.hits);
        assert_eq!(state.find.hits[0].artifact.as_ref().unwrap().line, 3);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_swept_artifact_falls_back_to_the_preview_and_is_declared_partial() {
        let root = temp_root("swept");
        let mut state = state(&root);
        state.transcript.push_user("run long".into());
        let call = ToolCallId::new("c1");
        state.transcript.push_tool_started(
            call.clone(),
            "run_command".into(),
            "{}".into(),
            false,
            0,
            Some(1),
            None,
        );
        let missing = root
            .join("state")
            .join("artifacts")
            .join("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef.txt");
        let reference =
            leveler_model::tool_output::saved_reference(missing.to_str().unwrap(), "hash", 34);
        state.transcript.complete_tool(
            &call,
            true,
            format!("{reference}\nvisible tail token"),
            5,
            None,
        );
        let corpus = build_corpus(&state);
        assert!(corpus.artifacts.is_empty());
        assert_eq!(corpus.partial_outputs, 1);
        let (hits, _) = search_corpus(&corpus, "visible tail token");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].source, FindSource::ToolOutput);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn turns_are_numbered_from_user_messages_with_their_terminal_and_clock() {
        let root = temp_root("turns");
        let mut state = state(&root);
        state.transcript.push_user("first request".into());
        state
            .transcript
            .push_turn_end(TurnEndStatus::Completed, 1, 12, None, None);
        state.transcript.push_user("second request".into());
        state
            .transcript
            .push_turn_end(TurnEndStatus::Failed, 2, 3, None, None);

        let turns = turn_entries(&state);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].ordinal, 1);
        assert_eq!(turns[0].preview, "first request");
        assert_eq!(turns[0].status, Some(TurnEndStatus::Completed));
        assert_eq!(turns[0].elapsed_secs, Some(12));
        assert_eq!(turns[1].ordinal, 2);
        assert_eq!(turns[1].status, Some(TurnEndStatus::Failed));
        assert_eq!(turns[1].elapsed_secs, Some(3));
        assert!(!turns[1].running);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_running_last_turn_is_marked_running_only_without_a_terminal() {
        let root = temp_root("running");
        let mut state = state(&root);
        state.transcript.push_user("working".into());
        state.status = leveler_client_protocol::RuntimeStatus::Busy;
        let turns = turn_entries(&state);
        assert!(turns[0].running);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn search_never_crosses_a_session_boundary() {
        let root_a = temp_root("session-a");
        let root_b = temp_root("session-b");
        let mut a = state(&root_a);
        a.transcript.push_user("alpha secret".into());
        let mut b = state(&root_b);
        b.transcript.push_user("beta other".into());
        let corpus_a = build_corpus(&a);
        let corpus_b = build_corpus(&b);
        let (hits_a, _) = search_corpus(&corpus_a, "alpha");
        let (hits_b, _) = search_corpus(&corpus_b, "alpha");
        assert_eq!(hits_a.len(), 1);
        assert!(hits_b.is_empty());
        let _ = std::fs::remove_dir_all(&root_a);
        let _ = std::fs::remove_dir_all(&root_b);
    }

    #[test]
    fn a_call_hit_resolves_by_identity_after_the_transcript_moves() {
        let root = temp_root("identity");
        let mut before = state(&root);
        before.transcript.push_user("go".into());
        let call = ToolCallId::new("stable-call");
        before.transcript.push_tool_started(
            call.clone(),
            "run_command".into(),
            "{}".into(),
            false,
            0,
            Some(1),
            None,
        );
        before
            .transcript
            .complete_tool(&call, false, "test_session_resume FAILED".into(), 9, None);
        let corpus = build_corpus(&before);
        let (hits, _) = search_corpus(&corpus, "FAILED");
        assert_eq!(hits.len(), 1);

        // A replay/notification lands ahead of the call: its index moves, but
        // the id is the same call and the hit must follow it.
        let mut after = state(&root);
        after.transcript.push_note("inserted note".into());
        after.transcript.push_note("second note".into());
        after.transcript.push_user("go".into());
        after.transcript.push_tool_started(
            call.clone(),
            "run_command".into(),
            "{}".into(),
            false,
            0,
            Some(1),
            None,
        );
        after
            .transcript
            .complete_tool(&call, false, "test_session_resume FAILED".into(), 9, None);
        let resolved = resolve_target(&after, &hits[0]).expect("the call id still resolves");
        assert_eq!(resolved.1, Some(0));
        assert_ne!(resolved.0, hits[0].item, "the item moved");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_large_transcript_is_indexed_and_searched_in_bounded_time() {
        let root = temp_root("perf");
        let mut state = state(&root);
        // ~800 turns, each with an answer and a 4 KiB tool preview: several
        // megabytes of transcript without any artifact.
        let preview: String = (0..200)
            .map(|line| {
                if line == 40 {
                    format!("line {line:04} of tool output needle_token_0400\n")
                } else {
                    format!("line {line:04} of tool output\n")
                }
            })
            .collect();
        for turn in 0..800 {
            state
                .transcript
                .push_user(format!("turn {turn} run the suite"));
            let message = MessageId::new(format!("a{turn}"));
            state.transcript.begin_assistant(message.clone());
            state.transcript.append_assistant(
                &message,
                &format!("answer {turn} for the refund workflow with context"),
            );
            state.transcript.finish_assistant(&message);
            let call = ToolCallId::new(format!("c{turn}"));
            state.transcript.push_tool_started(
                call.clone(),
                "run_command".into(),
                "{}".into(),
                false,
                0,
                Some(1),
                None,
            );
            state
                .transcript
                .complete_tool(&call, true, preview.clone(), 1, None);
            state
                .transcript
                .push_turn_end(TurnEndStatus::Completed, 1, 1, None, None);
        }
        let started = std::time::Instant::now();
        let corpus = build_corpus(&state);
        let build = started.elapsed();
        let corpus_bytes: usize = corpus.docs.iter().map(|doc| doc.text.len()).sum();
        let started = std::time::Instant::now();
        let (hits, capped) = search_corpus(&corpus, "needle_token_0400");
        let search = started.elapsed();
        println!(
            "find perf: docs={} corpus_bytes={} build_ms={} search_ms={} hits={}",
            corpus.docs.len(),
            corpus_bytes,
            build.as_millis(),
            search.as_millis(),
            hits.len()
        );
        assert_eq!(hits.len(), MAX_RESULTS, "the result cap bounds the list");
        assert!(capped, "800 matches must be reported as truncated");
        assert!(build.as_secs_f64() < 5.0, "corpus build took {build:?}");
        assert!(search.as_secs_f64() < 5.0, "search took {search:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn stale_generation_results_do_not_replace_a_newer_search() {
        let root = temp_root("stale");
        let mut state = state(&root);
        state.find.generation = 7;
        apply_transcript_results(&mut state, 6, Vec::new(), false);
        assert!(state.find.hits.is_empty());
        state.find.generation = 7;
        apply_transcript_results(&mut state, 7, Vec::new(), false);
        assert!(!state.find.searching);
        let _ = std::fs::remove_dir_all(&root);
    }
}
