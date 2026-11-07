//! The single streaming walker shared by index, digest, read, and search.
//!
//! Every public operation re-reads the file. There is no cache to go stale
//! while another agent is still writing its session.

use std::collections::HashMap;

use crate::model::*;

/// Hard cap on indexed events per call. Beyond this the index reports
/// `truncated`; the caller can still read the tail by searching.
pub const MAX_INDEX_EVENTS: usize = 50_000;
/// A single source line larger than this is read as a truncated prefix.
pub const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;
/// Text retained per event in `read`/`search`, before per-call clipping.
pub const MAX_EVENT_TEXT_BYTES: usize = 16 * 1024;
/// Search stops after this many hits in one call.
pub const MAX_SEARCH_HITS: usize = 20;
/// A picker lists at most this many sessions per source.
pub const MAX_SESSIONS_PER_SOURCE: usize = 50;
/// Discovery ignores sessions older than this.
pub const MAX_SESSION_AGE_DAYS: u64 = 30;

const PREVIEW_CHARS: usize = 240;
const FIRST_USER_CAP: usize = 4 * 1024;
const FOLLOW_UP_CAP: usize = 2 * 1024;
const LAST_ASSISTANT_CAP: usize = 8 * 1024;
const ERROR_CAP: usize = 4 * 1024;
const RECENT_EVENTS: usize = 24;
const MAX_OPEN_CALLS: usize = 64;
const READ_LIMIT: usize = 50;
const READ_TOTAL_BYTES: usize = 64 * 1024;
pub(crate) const COMPACTION_MARKER: &str = "Context compaction boundary.";
const ABORTED_MARKER: &str = "Turn aborted before completion.";

/// One event as produced by an adapter, before indexing.
#[derive(Debug, Clone)]
pub(crate) struct WalkedEvent {
    pub kind: ForeignEventKind,
    pub text: String,
    pub call_id: Option<String>,
    pub tool_name: Option<String>,
    pub is_error: bool,
    pub timestamp: Option<String>,
    pub sidechain: bool,
    /// 1-based source line; stamped by the walker.
    pub line: u64,
}

impl WalkedEvent {
    pub(crate) fn new(kind: ForeignEventKind, text: impl Into<String>) -> Self {
        Self {
            kind,
            text: text.into(),
            call_id: None,
            tool_name: None,
            is_error: false,
            timestamp: None,
            sidechain: false,
            line: 0,
        }
    }

    pub(crate) fn at(mut self, timestamp: Option<String>, sidechain: bool) -> Self {
        self.timestamp = timestamp;
        self.sidechain = sidechain;
        self
    }
}

#[derive(Debug, Default)]
pub(crate) struct WalkReport {
    pub stats: ForeignSessionStats,
    /// Records the adapter recognized, even when they contained no event.
    pub recognized_records: usize,
    pub warnings: Vec<String>,
}

/// What an adapter found in one source line.
pub(crate) enum Interpreted {
    /// Not a record this adapter knows. Counted as skipped, not recognized.
    Ignored,
    /// A known record that contributes zero or more events. Zero is valid:
    /// meta user rows and reasoning records are recognized but not indexed.
    Events(Vec<WalkedEvent>),
}

pub(crate) fn interpret_claude(value: &serde_json::Value) -> Interpreted {
    crate::claude::interpret(value)
}

pub(crate) fn interpret_codex(value: &serde_json::Value) -> Interpreted {
    crate::codex::interpret(value)
}

pub(crate) fn index(
    summary: &ForeignSessionSummary,
) -> Result<ForeignSessionIndex, ForeignSessionError> {
    let mut events = Vec::new();
    let mut truncated = false;
    let mut next = 1u32;
    let report = walk(summary, &mut |event| {
        if events.len() >= MAX_INDEX_EVENTS {
            truncated = true;
            return false;
        }
        events.push(ForeignEventRef {
            index: next,
            line: event.line,
            kind: event.kind,
            preview: preview(&event.text),
            bytes: event.text.len(),
            timestamp: event.timestamp.clone(),
            sidechain: event.sidechain,
        });
        next += 1;
        true
    })?;
    ensure_recognizable(summary, &report)?;
    if events.is_empty() {
        return Err(ForeignSessionError::Incompatible {
            path: summary.source_path.clone(),
            detail: "no conversation events".to_string(),
        });
    }
    Ok(ForeignSessionIndex {
        summary: summary.clone(),
        events,
        stats: report.stats,
        warnings: report.warnings,
        truncated,
    })
}

pub(crate) fn digest(
    summary: &ForeignSessionSummary,
) -> Result<ForeignSessionDigest, ForeignSessionError> {
    let mut first_user = None;
    let mut early_user = Vec::new();
    let mut last_assistant = None;
    let mut recent: Vec<ForeignEventRef> = Vec::new();
    let mut open: Vec<(String, String, u32)> = Vec::new();
    let mut open_position: HashMap<String, usize> = HashMap::new();
    let mut last_error = None;
    let mut aborted = false;
    let mut next = 1u32;

    let report = walk(summary, &mut |event| {
        let index = next;
        next += 1;
        if let ForeignEventKind::User = event.kind
            && first_user.is_none()
        {
            first_user = Some(clip_chars(&event.text, FIRST_USER_CAP).0.to_string());
        }
        if let ForeignEventKind::User = event.kind
            && early_user.len() < 5
        {
            early_user.push(clip_chars(&event.text, FOLLOW_UP_CAP).0.to_string());
        }
        if let ForeignEventKind::Assistant = event.kind
            && !event.text.trim().is_empty()
        {
            last_assistant = Some(clip_chars(&event.text, LAST_ASSISTANT_CAP).0.to_string());
        }
        if event.is_error && !event.text.trim().is_empty() {
            last_error = Some(clip_chars(&event.text, ERROR_CAP).0.to_string());
        }
        if let ForeignEventKind::System = event.kind {
            if event.text.starts_with(COMPACTION_MARKER) {
                // Counted in the walker; nothing else to do here.
            } else if event.text.starts_with(ABORTED_MARKER) {
                aborted = true;
            } else if !event.text.trim().is_empty() {
                last_error = Some(clip_chars(&event.text, ERROR_CAP).0.to_string());
            }
        }
        if let ForeignEventKind::ToolCall = event.kind
            && let Some(call_id) = event.call_id.clone()
            && open.len() < MAX_OPEN_CALLS
            && !open_position.contains_key(&call_id)
        {
            open_position.insert(call_id.clone(), open.len());
            open.push((call_id, event.tool_name.clone().unwrap_or_default(), index));
        }
        if let ForeignEventKind::ToolResult = event.kind
            && let Some(call_id) = event.call_id.as_deref()
            && let Some(position) = open_position.remove(call_id)
        {
            open.remove(position);
            // Positions after the removed row shift down; rebuild the map so a
            // later result still addresses the right open call.
            open_position.clear();
            for (position, (id, _, _)) in open.iter().enumerate() {
                open_position.insert(id.clone(), position);
            }
        }
        recent.push(ForeignEventRef {
            index,
            line: event.line,
            kind: event.kind,
            preview: preview(&event.text),
            bytes: event.text.len(),
            timestamp: event.timestamp.clone(),
            sidechain: event.sidechain,
        });
        if recent.len() > RECENT_EVENTS {
            recent.remove(0);
        }
        true
    })?;
    ensure_recognizable(summary, &report)?;
    if report.stats.user_messages == 0 && report.stats.assistant_messages == 0 {
        return Err(ForeignSessionError::Incompatible {
            path: summary.source_path.clone(),
            detail: "no user or assistant messages".to_string(),
        });
    }
    Ok(ForeignSessionDigest {
        summary: summary.clone(),
        stats: report.stats,
        warnings: report.warnings,
        first_user_message: first_user,
        early_user_messages: early_user,
        last_assistant_message: last_assistant,
        recent,
        open_tool_calls: open
            .into_iter()
            .map(|(call_id, name, index)| ForeignOpenCall {
                call_id,
                name,
                index,
            })
            .collect(),
        last_error,
        turn_aborted: aborted,
    })
}

pub(crate) fn read_events(
    summary: &ForeignSessionSummary,
    start_index: u32,
    limit: usize,
) -> Result<Vec<ForeignEventDetail>, ForeignSessionError> {
    if start_index == 0 {
        return Ok(Vec::new());
    }
    let limit = limit.min(READ_LIMIT);
    if limit == 0 {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let mut total = 0usize;
    let mut next = 1u32;
    walk(summary, &mut |event| {
        if next < start_index {
            next += 1;
            return true;
        }
        let (text, truncated) = clip_bytes(&event.text, MAX_EVENT_TEXT_BYTES);
        let text = if total >= READ_TOTAL_BYTES {
            String::from("[omitted: per-call output budget reached]")
        } else {
            total += text.len();
            text.to_string()
        };
        out.push(ForeignEventDetail {
            index: next,
            line: event.line,
            kind: event.kind,
            text,
            truncated,
            timestamp: event.timestamp.clone(),
            sidechain: event.sidechain,
            is_error: event.is_error,
            call_id: event.call_id.clone(),
            tool_name: event.tool_name.clone(),
        });
        next += 1;
        out.len() < limit
    })?;
    Ok(out)
}

pub(crate) fn search(
    summary: &ForeignSessionSummary,
    needle: &str,
    limit: usize,
) -> Result<Vec<ForeignEventSearchHit>, ForeignSessionError> {
    let needle = needle.trim();
    if needle.is_empty() {
        return Ok(Vec::new());
    }
    let needle_lower = needle.to_lowercase();
    let limit = limit.min(MAX_SEARCH_HITS);
    if limit == 0 {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let mut next = 1u32;
    walk(summary, &mut |event| {
        let index = next;
        next += 1;
        let normalized = normalize(&event.text);
        if let Some(byte_offset) = normalized.to_lowercase().find(&needle_lower) {
            let char_offset = normalized[..byte_offset].chars().count();
            out.push(ForeignEventSearchHit {
                event: ForeignEventRef {
                    index,
                    line: event.line,
                    kind: event.kind,
                    preview: preview(&event.text),
                    bytes: event.text.len(),
                    timestamp: event.timestamp.clone(),
                    sidechain: event.sidechain,
                },
                snippet: snippet(&normalized, char_offset, needle.chars().count()),
            });
        }
        out.len() < limit
    })?;
    Ok(out)
}

fn ensure_recognizable(
    summary: &ForeignSessionSummary,
    report: &WalkReport,
) -> Result<(), ForeignSessionError> {
    if report.recognized_records > 0 {
        return Ok(());
    }
    if report.stats.malformed_lines > 0 && report.stats.records == 0 {
        return Err(ForeignSessionError::Corrupt {
            path: summary.source_path.clone(),
            detail: "no valid JSON records".to_string(),
        });
    }
    Err(ForeignSessionError::Incompatible {
        path: summary.source_path.clone(),
        detail: "no recognized session records".to_string(),
    })
}

/// Stream one source file through its adapter.
fn walk(
    summary: &ForeignSessionSummary,
    visit: &mut dyn FnMut(WalkedEvent) -> bool,
) -> Result<WalkReport, ForeignSessionError> {
    let mut report = WalkReport::default();
    let interpret = match summary.tool {
        ForeignTool::ClaudeCode => interpret_claude,
        ForeignTool::Codex => interpret_codex,
    };
    crate::read::for_each_line(&summary.source_path, |line| {
        if line.truncated {
            report.stats.line_truncated += 1;
            report.stats.skipped_records += 1;
            if report.warnings.len() < 5 {
                report.warnings.push(format!(
                    "line {} exceeds the {} byte limit and was skipped",
                    line.number, MAX_LINE_BYTES
                ));
            }
            return true;
        }
        if line.bytes.iter().all(u8::is_ascii_whitespace) {
            return true;
        }
        let value = match serde_json::from_slice::<serde_json::Value>(&line.bytes) {
            Ok(value) => {
                report.stats.records += 1;
                value
            }
            Err(_) => {
                report.stats.malformed_lines += 1;
                return true;
            }
        };
        let events = match interpret(&value) {
            Interpreted::Ignored => {
                report.stats.skipped_records += 1;
                return true;
            }
            Interpreted::Events(events) => {
                report.recognized_records += 1;
                events
            }
        };
        for mut event in events {
            event.line = line.number;
            match event.kind {
                ForeignEventKind::User => report.stats.user_messages += 1,
                ForeignEventKind::Assistant => report.stats.assistant_messages += 1,
                ForeignEventKind::ToolCall => report.stats.tool_calls += 1,
                ForeignEventKind::ToolResult => report.stats.tool_results += 1,
                ForeignEventKind::System => {}
            }
            if event.is_error {
                report.stats.errors += 1;
            }
            if matches!(event.kind, ForeignEventKind::System)
                && event.text.starts_with(COMPACTION_MARKER)
            {
                report.stats.compactions += 1;
            }
            if event.sidechain {
                report.stats.sidechain_events += 1;
            }
            if !visit(event) {
                return false;
            }
        }
        true
    })?;
    Ok(report)
}

pub(crate) fn preview(text: &str) -> String {
    clip_chars(&normalize(text), PREVIEW_CHARS).0.to_string()
}

pub(crate) fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(crate) fn clip_chars(text: &str, max: usize) -> (&str, bool) {
    match text.char_indices().nth(max) {
        Some((offset, _)) => (&text[..offset], true),
        None => (text, false),
    }
}

fn clip_bytes(text: &str, max: usize) -> (&str, bool) {
    if text.len() <= max {
        return (text, false);
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

fn snippet(text: &str, char_offset: usize, needle_chars: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    let start = char_offset.saturating_sub(80);
    let end = (char_offset + needle_chars + 120).min(chars.len());
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.extend(chars[start..end].iter());
    if end < chars.len() {
        out.push('…');
    }
    out
}

pub(crate) fn compaction_event(timestamp: Option<String>) -> WalkedEvent {
    WalkedEvent::new(ForeignEventKind::System, COMPACTION_MARKER).at(timestamp, false)
}

pub(crate) fn aborted_event(timestamp: Option<String>) -> WalkedEvent {
    WalkedEvent::new(ForeignEventKind::System, ABORTED_MARKER).at(timestamp, false)
}
