//! Codex rollout JSONL adapter.
//!
//! Layout: `<root>/sessions/YYYY/MM/DD/rollout-<timestamp>-<session-id>.jsonl`.
//! Older stores may place rollouts directly under `sessions/`; that shape is
//! accepted for exact-id lookup but not scanned by discovery.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::model::*;
use crate::walk::{
    Interpreted, MAX_SESSION_AGE_DAYS, MAX_SESSIONS_PER_SOURCE, WalkedEvent, aborted_event,
    compaction_event,
};

const HEAD_BYTES: u64 = 256 * 1024;
const HEAD_RECORDS: usize = 400;
const MAX_DAY_DIRS: usize = 4096;
const TITLE_CHARS: usize = 200;

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
    let sessions = sessions_root(root);
    if !sessions.exists() {
        return Ok(Vec::new());
    }
    let limit = limit.clamp(1, MAX_SESSIONS_PER_SOURCE);
    let cutoff = now
        .checked_sub(Duration::from_secs(MAX_SESSION_AGE_DAYS * 86_400))
        .unwrap_or(UNIX_EPOCH);
    let cutoff_date = crate::system_time_rfc3339(cutoff)[..10].to_string();
    let mut candidates = Vec::new();
    let mut visited = 0usize;
    for (date, dir) in day_dirs(&sessions) {
        if date.as_str() < cutoff_date.as_str() {
            break;
        }
        visited += 1;
        if visited > MAX_DAY_DIRS {
            break;
        }
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
            let Some(native_id) = id_from_filename(&name) else {
                continue;
            };
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
    let sessions = sessions_root(root);
    if !sessions.exists() {
        return Err(ForeignSessionError::NotFound {
            tool: "codex",
            id: native_id.to_string(),
        });
    }
    let wanted = format!("{native_id}.jsonl");
    let wanted_zst = format!("{native_id}.jsonl.zst");
    let mut compressed: Option<PathBuf> = None;
    let mut visited = 0usize;
    for (_, dir) in day_dirs(&sessions) {
        visited += 1;
        if visited > MAX_DAY_DIRS {
            break;
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if name == wanted || (name.starts_with("rollout-") && name.ends_with(&wanted)) {
                let Ok(metadata) = entry.metadata() else {
                    continue;
                };
                let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
                return summarize(&entry.path(), native_id, modified, metadata.len());
            }
            if compressed.is_none()
                && (name == wanted_zst
                    || (name.starts_with("rollout-") && name.ends_with(&wanted_zst)))
            {
                compressed = Some(entry.path());
            }
        }
    }
    // A flat `sessions/rollout-*.jsonl` store is still an exact, bounded lookup.
    if let Ok(entries) = fs::read_dir(&sessions) {
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if name == wanted || (name.starts_with("rollout-") && name.ends_with(&wanted)) {
                let Ok(metadata) = entry.metadata() else {
                    continue;
                };
                let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
                return summarize(&entry.path(), native_id, modified, metadata.len());
            }
        }
    }
    match compressed {
        Some(path) => Err(ForeignSessionError::UnsupportedFormat {
            path,
            detail: "zstd-compressed Codex rollouts are not supported".to_string(),
        }),
        None => Err(ForeignSessionError::NotFound {
            tool: "codex",
            id: native_id.to_string(),
        }),
    }
}

pub(crate) fn sessions_root(root: &Path) -> PathBuf {
    root.join("sessions")
}

fn day_dirs(sessions: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let Ok(years) = fs::read_dir(sessions) else {
        return out;
    };
    for year in years.flatten() {
        if !year.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let year_name = year.file_name().to_string_lossy().to_string();
        if year_name.len() != 4 || !year_name.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let Ok(months) = fs::read_dir(year.path()) else {
            continue;
        };
        for month in months.flatten() {
            if !month.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let month_name = month.file_name().to_string_lossy().to_string();
            if month_name.len() != 2 || !month_name.bytes().all(|byte| byte.is_ascii_digit()) {
                continue;
            }
            let Ok(days) = fs::read_dir(month.path()) else {
                continue;
            };
            for day in days.flatten() {
                if !day.file_type().is_ok_and(|kind| kind.is_dir()) {
                    continue;
                }
                let day_name = day.file_name().to_string_lossy().to_string();
                if day_name.len() != 2 || !day_name.bytes().all(|byte| byte.is_ascii_digit()) {
                    continue;
                }
                out.push((format!("{year_name}-{month_name}-{day_name}"), day.path()));
            }
        }
    }
    out.sort_by(|left, right| right.0.cmp(&left.0));
    out
}

fn id_from_filename(name: &str) -> Option<&str> {
    let stem = name.strip_suffix(".jsonl")?;
    if !stem.starts_with("rollout-") {
        return None;
    }
    let id = stem.get(stem.len().checked_sub(36)?..)?;
    crate::validate_native_id(id).ok()?;
    Some(id)
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
    let mut title = None;
    let mut known = false;
    for (_, record) in &head {
        let Some(kind) = record.get("type").and_then(Value::as_str) else {
            continue;
        };
        if matches!(
            kind,
            "session_meta"
                | "turn_context"
                | "response_item"
                | "event_msg"
                | "compacted"
                | "world_state"
                | "token_usage_record"
                | "inter_agent_communication_metadata"
        ) {
            known = true;
        }
        let payload = record.get("payload").unwrap_or(&Value::Null);
        if cwd.is_none() {
            cwd = payload
                .get("cwd")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(PathBuf::from);
        }
        if branch.is_none() {
            branch = payload
                .get("git_branch")
                .or_else(|| payload.get("git").and_then(|git| git.get("branch")))
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(str::to_string);
        }
        if title.is_none() && kind == "response_item" {
            title = response_user_text(payload);
        }
    }
    if !known {
        return Err(ForeignSessionError::Incompatible {
            path: path.to_path_buf(),
            detail: "no recognized Codex rollout records".to_string(),
        });
    }
    let title = title
        .as_deref()
        .map(crate::walk::normalize)
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| native_id.to_string());
    let title = crate::walk::clip_chars(&title, TITLE_CHARS).0.to_string();
    Ok(ForeignSessionSummary {
        tool: ForeignTool::Codex,
        native_id: native_id.to_string(),
        title,
        cwd,
        branch,
        updated_at: crate::system_time_rfc3339(modified),
        source_path: path.to_path_buf(),
        size_bytes,
    })
}

fn response_user_text(payload: &Value) -> Option<String> {
    if payload.get("type").and_then(Value::as_str) != Some("message")
        || payload.get("role").and_then(Value::as_str) != Some("user")
    {
        return None;
    }
    let text = message_text(payload)?;
    if text.trim().is_empty() || is_injected(&text) {
        return None;
    }
    Some(text)
}

pub(crate) fn interpret(value: &Value) -> Interpreted {
    let Some(kind) = value.get("type").and_then(Value::as_str) else {
        return Interpreted::Ignored;
    };
    match kind {
        "session_meta"
        | "turn_context"
        | "world_state"
        | "token_usage_record"
        | "inter_agent_communication_metadata" => Interpreted::Events(Vec::new()),
        "compacted" => Interpreted::Events(vec![compaction_event(None)]),
        "response_item" => interpret_response_item(value.get("payload").unwrap_or(&Value::Null)),
        "event_msg" => interpret_event_msg(value.get("payload").unwrap_or(&Value::Null)),
        _ => Interpreted::Ignored,
    }
}

fn interpret_response_item(payload: &Value) -> Interpreted {
    let timestamp = payload
        .get("timestamp")
        .and_then(Value::as_str)
        .map(str::to_string);
    match payload.get("type").and_then(Value::as_str) {
        Some("message") => {
            let Some(role) = payload.get("role").and_then(Value::as_str) else {
                return Interpreted::Events(Vec::new());
            };
            let Some(text) = message_text(payload) else {
                return Interpreted::Events(Vec::new());
            };
            if text.trim().is_empty() {
                return Interpreted::Events(Vec::new());
            }
            match role {
                "user" if !is_injected(&text) => {
                    Interpreted::Events(vec![WalkedEvent::new(ForeignEventKind::User, text)])
                }
                "assistant" => {
                    Interpreted::Events(vec![WalkedEvent::new(ForeignEventKind::Assistant, text)])
                }
                _ => Interpreted::Events(Vec::new()),
            }
        }
        Some("function_call" | "custom_tool_call") => {
            let call_id = payload
                .get("call_id")
                .or_else(|| payload.get("id"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let name = payload
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_string();
            let input = payload
                .get("arguments")
                .or_else(|| payload.get("input"))
                .map(|input| match input {
                    Value::String(text) => text.clone(),
                    other => serde_json::to_string(other).unwrap_or_default(),
                })
                .unwrap_or_default();
            let mut event = WalkedEvent::new(ForeignEventKind::ToolCall, input);
            event.call_id = (!call_id.is_empty()).then_some(call_id);
            event.tool_name = Some(name);
            Interpreted::Events(vec![event.at(timestamp, false)])
        }
        Some("function_call_output" | "custom_tool_call_output") => {
            let call_id = payload
                .get("call_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if call_id.is_empty() {
                return Interpreted::Events(Vec::new());
            }
            let output = payload
                .get("output")
                .map(|output| match output {
                    Value::String(text) => text.clone(),
                    other => serde_json::to_string(other).unwrap_or_default(),
                })
                .unwrap_or_default();
            let mut event = WalkedEvent::new(ForeignEventKind::ToolResult, &output);
            event.call_id = Some(call_id);
            event.is_error = looks_like_error(&output);
            Interpreted::Events(vec![event.at(timestamp, false)])
        }
        Some("web_search_call") => {
            let call_id = payload
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let input = payload
                .get("action")
                .and_then(|action| action.get("query"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let mut event = WalkedEvent::new(ForeignEventKind::ToolCall, input);
            event.call_id = (!call_id.is_empty()).then_some(call_id);
            event.tool_name = Some("web_search".to_string());
            Interpreted::Events(vec![event.at(timestamp, false)])
        }
        // Reasoning is encrypted or duplicated; agent messages are inter-agent
        // delivery, not the user's conversation.
        Some("reasoning" | "agent_message" | "additional_tools") => Interpreted::Events(Vec::new()),
        _ => Interpreted::Ignored,
    }
}

fn interpret_event_msg(payload: &Value) -> Interpreted {
    match payload.get("type").and_then(Value::as_str) {
        Some("error") => {
            let text = payload
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| payload.get("error").and_then(Value::as_str))
                .unwrap_or("Codex reported an error")
                .to_string();
            let mut event = WalkedEvent::new(ForeignEventKind::System, text);
            event.is_error = true;
            Interpreted::Events(vec![event])
        }
        Some("turn_aborted") => Interpreted::Events(vec![aborted_event(None)]),
        Some("context_compacted" | "compacted") => {
            Interpreted::Events(vec![compaction_event(None)])
        }
        Some(_) => Interpreted::Events(Vec::new()),
        None => Interpreted::Ignored,
    }
}

fn message_text(payload: &Value) -> Option<String> {
    match payload.get("content")? {
        Value::String(text) => Some(text.clone()),
        Value::Array(blocks) => {
            let texts: Vec<String> = blocks
                .iter()
                .filter_map(|block| {
                    block
                        .get("text")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect();
            (!texts.is_empty()).then(|| texts.join("\n"))
        }
        _ => None,
    }
}

fn is_injected(text: &str) -> bool {
    let text = text.trim_start();
    [
        "# AGENTS.md instructions",
        "# developer instructions",
        "<environment_context",
        "<user_instructions",
        "<external_",
        "<app-context",
        "<collaboration_mode",
        "<permissions instructions",
        "<skills_instructions",
        "<turn_aborted",
        "<user_shell_command",
        "<subagent_notification",
        "<codex_internal_context",
        "<model_switch",
    ]
    .iter()
    .any(|prefix| text.starts_with(prefix))
}

fn looks_like_error(output: &str) -> bool {
    let text = output.trim_start();
    if let Some(rest) = text.strip_prefix("Exit code:") {
        let code = rest.split_whitespace().next().unwrap_or_default();
        return !code.is_empty() && code != "0";
    }
    text.starts_with("Error:")
        || text.starts_with("error:")
        || text.starts_with("Traceback")
        || text.contains("\"success\": false")
        || text.contains("\"success\":false")
}
