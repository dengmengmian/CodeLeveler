//! Claude Code JSONL adapter.
//!
//! Layout: `<root>/projects/<sanitized-cwd>/<session-id>.jsonl`. The adapter
//! reads heads for discovery and full files for content. It never follows a
//! symlinked project directory or session file.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::model::*;
use crate::walk::{
    Interpreted, MAX_SESSION_AGE_DAYS, MAX_SESSIONS_PER_SOURCE, WalkedEvent, compaction_event,
};

const HEAD_BYTES: u64 = 256 * 1024;
const HEAD_RECORDS: usize = 400;
const MAX_PROJECT_DIRS: usize = 512;
const TITLE_CHARS: usize = 200;

/// Claude records that are known but carry no conversation event.
const META_TYPES: &[&str] = &[
    "ai-title",
    "custom-title",
    "summary",
    "mode",
    "permission-mode",
    "atis-latch",
    "last-prompt",
    "attachment",
    "file-history-snapshot",
    "file-history-delta",
    "api-request-shape",
    "api-request-blob",
    "api-request",
    "tools-used",
    "queue-operation",
    "compact-boundary",
];

pub(crate) fn discover(
    root: &Path,
    cwd: Option<&Path>,
    limit: usize,
    now: SystemTime,
) -> Result<Vec<ForeignSessionSummary>, ForeignSessionError> {
    if !root.exists() {
        return Err(ForeignSessionError::RootUnavailable {
            path: root.to_path_buf(),
        });
    }
    let projects = root.join("projects");
    if !projects.exists() {
        return Ok(Vec::new());
    }
    let limit = limit.clamp(1, MAX_SESSIONS_PER_SOURCE);
    let cutoff = now
        .checked_sub(Duration::from_secs(MAX_SESSION_AGE_DAYS * 86_400))
        .unwrap_or(UNIX_EPOCH);
    let dirs = match cwd {
        Some(cwd) => candidate_project_dirs(&projects, cwd),
        None => all_project_dirs(&projects),
    };
    let mut candidates = Vec::new();
    for dir in dirs {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => continue,
            Err(error) => return Err(crate::io_error(&dir, error)),
        };
        for entry in entries {
            let Ok(entry) = entry else { continue };
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(native_id) = name.strip_suffix(".jsonl") else {
                continue;
            };
            if crate::validate_native_id(native_id).is_err() {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            let Ok(modified) = metadata.modified() else {
                continue;
            };
            if modified < cutoff {
                continue;
            }
            candidates.push((
                modified,
                entry.path(),
                native_id.to_string(),
                metadata.len(),
            ));
        }
    }
    candidates.sort_by(|left, right| right.0.cmp(&left.0));
    candidates.truncate(limit);
    let mut out = Vec::new();
    for (modified, path, native_id, size) in candidates {
        let Ok(summary) = summarize(&path, &native_id, modified, size) else {
            continue;
        };
        if let Some(cwd) = cwd
            && let Some(found) = summary.cwd.as_deref()
            && found != cwd
        {
            continue;
        }
        out.push(summary);
    }
    Ok(out)
}

pub(crate) fn find(
    root: &Path,
    native_id: &str,
) -> Result<ForeignSessionSummary, ForeignSessionError> {
    if !root.exists() {
        return Err(ForeignSessionError::RootUnavailable {
            path: root.to_path_buf(),
        });
    }
    let projects = root.join("projects");
    if !projects.exists() {
        return Err(ForeignSessionError::NotFound {
            tool: "claude-code",
            id: native_id.to_string(),
        });
    }
    let mut file_match: Option<(SystemTime, PathBuf, u64)> = None;
    let mut compressed: Option<PathBuf> = None;
    for dir in all_project_dirs(&projects) {
        let plain = dir.join(format!("{native_id}.jsonl"));
        if let Ok(metadata) = fs::symlink_metadata(&plain)
            && metadata.file_type().is_file()
        {
            let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
            if file_match
                .as_ref()
                .is_none_or(|(best, _, _)| modified > *best)
            {
                file_match = Some((modified, plain, metadata.len()));
            }
        }
        let zst = dir.join(format!("{native_id}.jsonl.zst"));
        if compressed.is_none()
            && fs::symlink_metadata(&zst).is_ok_and(|meta| meta.file_type().is_file())
        {
            compressed = Some(zst);
        }
    }
    match file_match {
        Some((modified, path, size)) => summarize(&path, native_id, modified, size),
        None => match compressed {
            Some(path) => Err(ForeignSessionError::UnsupportedFormat {
                path,
                detail: "zstd-compressed Claude Code transcripts are not supported".to_string(),
            }),
            None => Err(ForeignSessionError::NotFound {
                tool: "claude-code",
                id: native_id.to_string(),
            }),
        },
    }
}

fn summarize(
    path: &Path,
    native_id: &str,
    modified: SystemTime,
    size_bytes: u64,
) -> Result<ForeignSessionSummary, ForeignSessionError> {
    let head = crate::read::head_lines(path, HEAD_BYTES, HEAD_RECORDS)?;
    if head.is_empty() {
        return Err(if size_bytes == 0 {
            ForeignSessionError::Corrupt {
                path: path.to_path_buf(),
                detail: "file is empty".to_string(),
            }
        } else {
            ForeignSessionError::Corrupt {
                path: path.to_path_buf(),
                detail: "no valid JSON records in the file head".to_string(),
            }
        });
    }
    let mut cwd = None;
    let mut branch = None;
    let mut explicit_title = None;
    let mut first_user_text = None;
    let mut known: Option<bool> = None;
    for (_, value) in &head {
        let Some(kind) = value.get("type").and_then(Value::as_str) else {
            continue;
        };
        let is_known =
            matches!(kind, "user" | "assistant" | "system") || META_TYPES.contains(&kind);
        known = Some(known.unwrap_or(false) || is_known);
        if cwd.is_none() {
            cwd = value
                .get("cwd")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(PathBuf::from);
        }
        if branch.is_none() {
            branch = value
                .get("gitBranch")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(str::to_string);
        }
        if explicit_title.is_none() {
            explicit_title = value
                .get("aiTitle")
                .or_else(|| value.get("customTitle"))
                .or_else(|| value.get("summary"))
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        if first_user_text.is_none()
            && matches!(kind, "user")
            && value.get("isMeta").and_then(Value::as_bool) != Some(true)
            && let Some(text) = user_text(value)
            && !is_injected(&text)
            && !is_continuation_summary(&text)
        {
            first_user_text = Some(text);
        }
    }
    let title = explicit_title.or(first_user_text);
    if known != Some(true) {
        return Err(ForeignSessionError::Incompatible {
            path: path.to_path_buf(),
            detail: "no recognized Claude Code records".to_string(),
        });
    }
    let title = title
        .as_deref()
        .map(crate::walk::normalize)
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| native_id.to_string());
    let title = crate::walk::clip_chars(&title, TITLE_CHARS).0.to_string();
    Ok(ForeignSessionSummary {
        tool: ForeignTool::ClaudeCode,
        native_id: native_id.to_string(),
        title,
        cwd,
        branch,
        updated_at: crate::system_time_rfc3339(modified),
        source_path: path.to_path_buf(),
        size_bytes,
    })
}

fn user_text(value: &Value) -> Option<String> {
    let content = value.get("message")?.get("content")?;
    match content {
        Value::String(text) => Some(text.clone()),
        Value::Array(blocks) => blocks
            .iter()
            .find_map(|block| block.get("text").and_then(Value::as_str))
            .map(str::to_string),
        _ => None,
    }
}

/// Candidate project dirs for a working directory, matching case-insensitively
/// against what is actually on disk.
fn candidate_project_dirs(projects: &Path, cwd: &Path) -> Vec<PathBuf> {
    let wanted = project_dir_names(cwd);
    let mut exact = Vec::new();
    let mut lower: Vec<(String, PathBuf)> = Vec::new();
    if let Ok(entries) = fs::read_dir(projects) {
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            lower.push((name.to_lowercase(), entry.path()));
            if wanted.contains(&name) {
                exact.push(entry.path());
            }
        }
    }
    let mut out = Vec::new();
    for path in exact {
        if !out.contains(&path) {
            out.push(path);
        }
    }
    for (name_lower, path) in lower {
        if wanted.iter().any(|name| name.to_lowercase() == name_lower) && !out.contains(&path) {
            out.push(path);
        }
    }
    out.truncate(4);
    out
}

fn all_project_dirs(projects: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(projects) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                out.push(entry.path());
            }
        }
    }
    out.sort();
    out.truncate(MAX_PROJECT_DIRS);
    out
}

/// Candidate project directory names for a working directory. Public within
/// the crate so tests can prove both observed casings are accepted.
pub(crate) fn project_dir_names(cwd: &Path) -> Vec<String> {
    let raw = cwd.to_string_lossy();
    let sanitized: String = raw
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect();
    let mut names = vec![sanitized.clone(), sanitized.to_ascii_lowercase()];
    names.sort();
    names.dedup();
    names
}

pub(crate) fn interpret(value: &Value) -> Interpreted {
    let Some(kind) = value.get("type").and_then(Value::as_str) else {
        return Interpreted::Ignored;
    };
    match kind {
        "user" => interpret_user(value),
        "assistant" => interpret_assistant(value),
        "system" => interpret_system(value),
        other if META_TYPES.contains(&other) => Interpreted::Events(Vec::new()),
        _ => Interpreted::Ignored,
    }
}

fn interpret_user(value: &Value) -> Interpreted {
    if value.get("isMeta").and_then(Value::as_bool) == Some(true) {
        return Interpreted::Events(Vec::new());
    }
    let timestamp = timestamp(value);
    let sidechain = sidechain(value);
    let Some(content) = value
        .get("message")
        .and_then(|message| message.get("content"))
    else {
        return Interpreted::Events(Vec::new());
    };
    let mut events = Vec::new();
    match content {
        Value::String(text) => {
            if let Some(event) = user_text_event(text, timestamp.clone(), sidechain) {
                events.push(event);
            }
        }
        Value::Array(blocks) => {
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(text) = block.get("text").and_then(Value::as_str)
                            && let Some(event) = user_text_event(text, timestamp.clone(), sidechain)
                        {
                            events.push(event);
                        }
                    }
                    Some("tool_result") => {
                        let call_id = block
                            .get("tool_use_id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        if call_id.is_empty() {
                            continue;
                        }
                        let mut event = WalkedEvent::new(
                            ForeignEventKind::ToolResult,
                            flatten_tool_content(block.get("content")),
                        );
                        event.call_id = Some(call_id);
                        event.is_error =
                            block.get("is_error").and_then(Value::as_bool) == Some(true);
                        events.push(event.at(timestamp.clone(), sidechain));
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    Interpreted::Events(events)
}

fn interpret_assistant(value: &Value) -> Interpreted {
    let Some(content) = value
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
    else {
        return Interpreted::Events(Vec::new());
    };
    let timestamp = timestamp(value);
    let sidechain = sidechain(value);
    let mut events = Vec::new();
    for block in content {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str)
                    && !text.trim().is_empty()
                {
                    events.push(
                        WalkedEvent::new(ForeignEventKind::Assistant, text)
                            .at(timestamp.clone(), sidechain),
                    );
                }
            }
            Some("tool_use") => {
                let call_id = block
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("tool")
                    .to_string();
                let input = block
                    .get("input")
                    .map(|input| match input {
                        Value::String(text) => text.clone(),
                        other => serde_json::to_string(other).unwrap_or_default(),
                    })
                    .unwrap_or_default();
                let mut event = WalkedEvent::new(ForeignEventKind::ToolCall, input);
                event.call_id = (!call_id.is_empty()).then_some(call_id);
                event.tool_name = Some(name);
                events.push(event.at(timestamp.clone(), sidechain));
            }
            _ => {}
        }
    }
    Interpreted::Events(events)
}

fn interpret_system(value: &Value) -> Interpreted {
    let subtype = value.get("subtype").and_then(Value::as_str);
    let compact = value.get("isCompactSummary").and_then(Value::as_bool) == Some(true)
        || matches!(
            subtype,
            Some("compact_boundary" | "compact_summary" | "context_compact")
        );
    if compact {
        return Interpreted::Events(vec![compaction_event(timestamp(value))]);
    }
    if value.get("level").and_then(Value::as_str) == Some("error") {
        let text = value
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("system error")
            .to_string();
        let mut event =
            WalkedEvent::new(ForeignEventKind::System, text).at(timestamp(value), false);
        event.is_error = true;
        return Interpreted::Events(vec![event]);
    }
    Interpreted::Events(Vec::new())
}

fn user_text_event(text: &str, timestamp: Option<String>, sidechain: bool) -> Option<WalkedEvent> {
    if text.trim().is_empty() {
        return None;
    }
    if is_continuation_summary(text) {
        return Some(
            WalkedEvent::new(
                ForeignEventKind::System,
                format!("{} {}", crate::walk::COMPACTION_MARKER, text),
            )
            .at(timestamp, sidechain),
        );
    }
    if is_injected(text) {
        return None;
    }
    Some(WalkedEvent::new(ForeignEventKind::User, text).at(timestamp, sidechain))
}

fn is_continuation_summary(text: &str) -> bool {
    text.trim_start()
        .starts_with("This session is being continued from a previous conversation")
}

fn is_injected(text: &str) -> bool {
    let text = text.trim_start();
    [
        "<local-command-caveat>",
        "<local-command-stdout>",
        "<local-command-stderr>",
        "<command-name>",
        "<command-message>",
        "<command-args>",
        "<bash-input>",
        "<bash-stdout>",
        "<bash-stderr>",
        "<system-reminder>",
        "[Request interrupted by user]",
    ]
    .iter()
    .any(|prefix| text.starts_with(prefix))
}

fn flatten_tool_content(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| {
                block
                    .get("text")
                    .and_then(Value::as_str)
                    .or_else(|| block.get("content").and_then(Value::as_str))
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(Value::Object(object)) => object
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    }
}

fn timestamp(value: &Value) -> Option<String> {
    value
        .get("timestamp")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn sidechain(value: &Value) -> bool {
    value.get("isSidechain").and_then(Value::as_bool) == Some(true)
}
