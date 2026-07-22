//! Rendering for tool-call transcript cells and the Tools screen.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders};
use unicode_width::UnicodeWidthStr;

use crate::render::{truncate_display, wrap};
use crate::state::AppState;
use crate::theme::Theme;
use crate::transcript::{ToolCallBlock, ToolStatus};

fn tool_glyph(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Running => "◌",
        ToolStatus::Ok => "✓",
        ToolStatus::Failed => "✗",
    }
}

fn tool_style(theme: &Theme, status: ToolStatus) -> Style {
    match status {
        ToolStatus::Running => Style::default().fg(theme.accent),
        ToolStatus::Ok => Style::default().fg(theme.success),
        ToolStatus::Failed => Style::default().fg(theme.error),
    }
}

/// Tools whose guard rejection is a neutral skip (closeout, loop-guard,
/// skip-complete), not a real failure — shown with ⚠ instead of ✗.
pub(crate) fn is_guard_denied_name(name: &str) -> bool {
    matches!(
        name,
        "update_plan" | "update_goal" | "list_files" | "git_status" | "grep" | "repository_search"
    )
}

/// Localized user-facing note for a guard denial. The runtime's guard text is
/// internal English guidance for the model and must never reach the user.
pub(crate) fn guard_denial_note<'a>(name: &str, t: &'a crate::i18n::UiText) -> &'a str {
    if name == "update_plan" {
        t.plan_update_rejected
    } else {
        t.observe_denied
    }
}

fn tools_footer_hint(width: usize, t: &crate::i18n::UiText) -> String {
    let full = t.tools_footer_hint_full;
    let compact = t.tools_footer_hint_compact;

    if full.width() <= width {
        full.to_string()
    } else if compact.width() <= width {
        compact.to_string()
    } else {
        truncate_display(compact, width)
    }
}

fn format_arg_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Null => "null".to_string(),
        other => serde_json::to_string(other).unwrap_or_else(|_| other.to_string()),
    }
}

fn tool_argument_lines(
    arguments: &str,
    theme: &Theme,
    width: usize,
    t: &crate::i18n::UiText,
) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        t.tool_label_args,
        Style::default().fg(theme.muted),
    ))];
    let inner = width.saturating_sub(2).max(1);
    let Ok(value) = serde_json::from_str::<serde_json::Value>(arguments) else {
        for line in wrap(arguments, inner) {
            lines.push(Line::from(Span::raw(format!("  {line}"))));
        }
        return lines;
    };
    let Some(obj) = value.as_object() else {
        lines.push(Line::from(Span::raw(format!(
            "  {}",
            truncate_display(&format_arg_value(&value), inner)
        ))));
        return lines;
    };
    if obj.is_empty() {
        lines.push(Line::from(Span::raw("  {}")));
        return lines;
    }
    for (key, value) in obj {
        let row = format!("{key}: {}", format_arg_value(value));
        for (i, line) in wrap(&row, inner).into_iter().enumerate() {
            let prefix = if i == 0 { "  " } else { "    " };
            lines.push(Line::from(Span::raw(format!("{prefix}{line}"))));
        }
    }
    lines
}

fn compact_path_for_summary(path: &str) -> String {
    if path.is_empty() || !path.starts_with('/') {
        return path.to_string();
    }
    let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    if parts.is_empty() {
        return path.to_string();
    }
    const PROJECT_MARKERS: &[&str] = &[
        "src",
        "test",
        "tests",
        "crates",
        "packages",
        "web",
        "docweb",
        "docbackend",
    ];
    if let Some(idx) = parts.iter().position(|part| PROJECT_MARKERS.contains(part)) {
        return parts[idx..].join("/");
    }
    let last = parts.last().copied().unwrap_or(path);
    if last.contains('.') || parts.len() == 1 {
        last.to_string()
    } else {
        parts[parts.len().saturating_sub(2)..].join("/")
    }
}

fn patch_touched_files(patch: &str) -> Vec<String> {
    let mut files = Vec::new();
    for raw in patch.lines() {
        let line = raw.trim_start();
        let path = line
            .strip_prefix("*** Update File: ")
            .or_else(|| line.strip_prefix("*** Add File: "))
            .or_else(|| line.strip_prefix("*** Delete File: "))
            .or_else(|| {
                line.strip_prefix("+++ b/")
                    .or_else(|| line.strip_prefix("--- a/"))
            });
        let Some(path) = path else {
            continue;
        };
        if path == "/dev/null" {
            continue;
        }
        let compacted = compact_path_for_summary(path);
        if !files.contains(&compacted) {
            files.push(compacted);
        }
    }
    files
}

fn patch_summary_from_text(patch: &str, t: &crate::i18n::UiText) -> String {
    let files = patch_touched_files(patch);
    if files.is_empty() {
        t.tool_label_patch.to_string()
    } else {
        files.join(", ")
    }
}

fn find_patch_text_value(value: &serde_json::Value) -> Option<&str> {
    match value {
        serde_json::Value::String(s) if s.contains("*** Begin Patch") || s.contains("\n+++ b/") => {
            Some(s)
        }
        serde_json::Value::Array(items) => items.iter().find_map(find_patch_text_value),
        serde_json::Value::Object(obj) => obj.values().find_map(find_patch_text_value),
        _ => None,
    }
}

fn patch_text_from_arguments(arguments: &str) -> String {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(arguments)
        && let Some(patch) = value
            .get("patch")
            .and_then(|patch| patch.as_str())
            .or_else(|| find_patch_text_value(&value))
    {
        return patch.to_string();
    }

    let Some(start) = arguments.find("*** Begin Patch") else {
        return arguments.to_string();
    };
    let tail = &arguments[start..];
    let Some(end) = tail.find("*** End Patch") else {
        return tail.to_string();
    };
    tail[..end + "*** End Patch".len()].to_string()
}

fn first_path_value(value: &serde_json::Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| value.get(key).and_then(|x| x.as_str()))
        .map(compact_path_for_summary)
        .unwrap_or_default()
}

/// Localized one-line target summary, public for Conversation rendering.
pub(crate) fn tool_summary_pub(name: &str, arguments: &str, t: &crate::i18n::UiText) -> String {
    tool_summary_for(name, arguments, t)
}

/// Legacy 2-arg form (Chinese fallback copy) for callers that do not thread
/// [`crate::i18n::UiText`] — e.g. the reducer's busy-activity string.
pub(crate) fn tool_summary(name: &str, arguments: &str) -> String {
    tool_summary_for(name, arguments, crate::i18n::Locale::Zh.text())
}

pub(crate) fn tool_summary_for(name: &str, arguments: &str, t: &crate::i18n::UiText) -> String {
    let v: serde_json::Value = match serde_json::from_str(arguments) {
        Ok(v) => v,
        Err(_) if name == "apply_patch" => {
            return truncate_display(
                &patch_summary_from_text(&patch_text_from_arguments(arguments), t),
                64,
            );
        }
        Err(_) if name == "replace" => return t.tool_label_replace.to_string(),
        // Never dump raw JSON / partial tool args into Conversation activity.
        Err(_) if looks_like_json_object(arguments) => return String::new(),
        Err(_) => return truncate_display(&command_line_summary(arguments), 56),
    };
    let s = |key: &str| {
        v.get(key)
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string()
    };
    let summary = match name {
        "run_command" | "shell_command" => execute_command_summary(name, &v),
        "read_file" => {
            let path = compact_path_for_summary(&s("path"));
            match (
                v.get("start_line").and_then(|x| x.as_u64()),
                v.get("end_line").and_then(|x| x.as_u64()),
            ) {
                (Some(a), Some(b)) => format!("{path}:{a}-{b}"),
                _ => path,
            }
        }
        "grep" => {
            let path = compact_path_for_summary(&s("path"));
            if path.is_empty() {
                format!("\"{}\"", s("pattern"))
            } else {
                format!("\"{}\" in {path}", s("pattern"))
            }
        }
        "list_files" => {
            let p = compact_path_for_summary(&s("path"));
            if p.is_empty() { ".".to_string() } else { p }
        }
        "apply_patch" => {
            let patch = patch_text_from_arguments(arguments);
            patch_summary_from_text(&patch, t)
        }
        "replace" => {
            let path = first_path_value(
                &v,
                &["path", "file", "file_path", "filepath", "target_file"],
            );
            if path.is_empty() {
                t.tool_label_replace.to_string()
            } else {
                path
            }
        }
        "find_symbol" | "read_symbol" | "find_references" => s("symbol"),
        "repository_search" => s("query"),
        "update_plan" => s("explanation"),
        "update_goal" => update_goal_summary_text(&v, t),
        "task" => {
            let description = s("description");
            if description.is_empty() {
                s("prompt")
            } else {
                description
            }
        }
        // Prefer a short human field; never show the raw argument object.
        _ => first_human_field(
            &v,
            &[
                "path",
                "file",
                "file_path",
                "filepath",
                "target_file",
                "query",
                "pattern",
                "symbol",
                "description",
                "prompt",
                "cmd",
                "command",
                "name",
            ],
        ),
    };
    truncate_display(&summary, 64)
}

/// Full `update_goal` closeout line (no width truncate) for expand / body.
///
/// Strips inline markdown so activity lines never leak `**` / backticks.
fn update_goal_summary_text(v: &serde_json::Value, t: &crate::i18n::UiText) -> String {
    let summary = v
        .get("summary")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .replace("**", "")
        .replace('`', "");
    let prefix = match v.get("status").and_then(|x| x.as_str()).unwrap_or("") {
        "blocked" => t.tool_word_blocked,
        _ => t.tool_word_done,
    };
    if summary.is_empty() {
        prefix.to_string()
    } else {
        // Fullwidth colon pairs with CJK copy, ASCII colon with English.
        let sep = if t.unsupported_task_action.starts_with("Delegation") {
            ": "
        } else {
            "："
        };
        format!("{prefix}{sep}{summary}")
    }
}

fn update_goal_summary_from_arguments(arguments: &str, t: &crate::i18n::UiText) -> String {
    match serde_json::from_str::<serde_json::Value>(arguments) {
        Ok(v) => update_goal_summary_text(&v, t),
        Err(_) => String::new(),
    }
}

/// Human-readable command line for shell / run tools (no JSON, no `cd` noise).
fn execute_command_summary(name: &str, v: &serde_json::Value) -> String {
    // shell_command uses a single `cmd` string.
    if name == "shell_command" || v.get("cmd").is_some() {
        let cmd = v
            .get("cmd")
            .or_else(|| v.get("command"))
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .trim();
        if !cmd.is_empty() {
            return command_line_summary(cmd);
        }
    }

    let program = v
        .get("program")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let mut args = v
        .get("args")
        .and_then(|a| a.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    drop_duplicate_program_arg(&program, &mut args);

    // `bash -c '…'` / `sh -c '…'` → summarize the script body, not the shell wrapper.
    if is_shell_program(&program)
        && let Some(script) = shell_c_script(&args)
    {
        return command_line_summary(script);
    }

    if program.is_empty() && args.is_empty() {
        return String::new();
    }

    let line = format!("{} {}", program, args.join(" ")).trim().to_string();
    command_line_summary(&line)
}

fn is_shell_program(program: &str) -> bool {
    let base = std::path::Path::new(program)
        .file_name()
        .and_then(|p| p.to_str())
        .unwrap_or(program);
    matches!(base, "bash" | "sh" | "zsh" | "dash" | "cmd" | "cmd.exe")
}

fn shell_c_script(args: &[String]) -> Option<&str> {
    let idx = args
        .iter()
        .position(|a| a == "-c" || a == "/C" || a == "/c")?;
    args.get(idx + 1).map(String::as_str)
}

/// Strip leading `cd <path> &&` noise, compact `$HOME` paths, keep the readable core.
fn command_line_summary(cmd: &str) -> String {
    let rest = strip_leading_cd_chain(cmd.trim());
    compact_home_in_text(rest)
}

fn strip_leading_cd_chain(cmd: &str) -> &str {
    let mut rest = cmd;
    loop {
        let trimmed = rest.trim_start();
        let after_cd = if let Some(r) = trimmed.strip_prefix("cd ") {
            r
        } else if let Some(r) = trimmed.strip_prefix("cd\t") {
            r
        } else {
            return trimmed;
        };
        let after_cd = after_cd.trim_start();
        let path_len = shell_token_len(after_cd);
        if path_len == 0 {
            return trimmed;
        }
        let after_path = after_cd[path_len..].trim_start();
        if let Some(r) = after_path.strip_prefix("&&") {
            rest = r;
            continue;
        }
        if let Some(r) = after_path.strip_prefix(';') {
            rest = r;
            continue;
        }
        // Bare `cd path` (no follow-on): show a compact form of the path.
        return trimmed;
    }
}

/// Length of the next shell token (quoted or unquoted).
fn shell_token_len(s: &str) -> usize {
    let bytes = s.as_bytes();
    if bytes.is_empty() {
        return 0;
    }
    match bytes[0] {
        b'\'' => {
            if let Some(end) = s[1..].find('\'') {
                end + 2
            } else {
                s.len()
            }
        }
        b'"' => {
            let mut i = 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                if bytes[i] == b'"' {
                    return i + 1;
                }
                i += 1;
            }
            s.len()
        }
        _ => s
            .find(|c: char| c.is_whitespace() || c == ';' || c == '&' || c == '|')
            .unwrap_or(s.len()),
    }
}

fn compact_home_in_text(text: &str) -> String {
    let Some(home) = leveler_core::environment().var_os("HOME") else {
        return text.to_string();
    };
    let hs = home.to_string_lossy();
    if hs.is_empty() || !text.contains(hs.as_ref()) {
        return text.to_string();
    }
    text.replace(hs.as_ref(), "~")
}

fn looks_like_json_object(s: &str) -> bool {
    let t = s.trim_start();
    t.starts_with('{') || t.starts_with('[')
}

fn first_human_field(v: &serde_json::Value, keys: &[&str]) -> String {
    for key in keys {
        if let Some(s) = v
            .get(*key)
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
        {
            return if key.contains("path") || *key == "file" || *key == "filepath" {
                compact_path_for_summary(s)
            } else {
                compact_home_in_text(s)
            };
        }
    }
    String::new()
}

fn drop_duplicate_program_arg(program: &str, args: &mut Vec<String>) {
    let Some(first) = args.first() else {
        return;
    };
    let program_name = std::path::Path::new(program)
        .file_name()
        .and_then(|p| p.to_str())
        .unwrap_or(program);
    if first == program || first == program_name {
        args.remove(0);
    }
}

/// Localized presentation label for a tool (taxonomy). Defaults to Chinese for
/// callers that do not yet thread locale (status chrome uses [`tool_action_label_for`]).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn tool_action_label(name: &str) -> String {
    tool_action_label_for(name, crate::i18n::Locale::Zh)
}

/// Localized presentation label for a tool name.
pub(crate) fn tool_action_label_for(name: &str, locale: crate::i18n::Locale) -> String {
    crate::tool_taxonomy::presentation_label(name, locale)
}

/// Compact one-line tool heading text (glyph + presentation + summary).
/// Used by unit tests and as the canonical compact-row contract.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn compact_tool_heading(
    status: ToolStatus,
    name: &str,
    arguments: &str,
    locale: crate::i18n::Locale,
) -> String {
    let t = locale.text();
    crate::tool_taxonomy::compact_tool_line(
        tool_glyph(status),
        name,
        arguments,
        locale,
        |name, arguments| tool_summary_for(name, arguments, t),
    )
}

fn noisy_success_tool(name: &str) -> bool {
    matches!(
        name,
        "read_file"
            | "list_files"
            | "grep"
            | "repository_search"
            | "find_symbol"
            | "read_symbol"
            | "find_references"
            | "git_status"
            | "git_diff"
            | "update_plan"
            | "update_goal"
    )
}

pub(crate) fn tool_lines(
    block: &ToolCallBlock,
    theme: &Theme,
    width: usize,
    tools_expanded: bool,
    t: &crate::i18n::UiText,
    out: &mut Vec<Line<'static>>,
) {
    // A guard turning down a read-only/bookkeeping call (closeout, loop-guard,
    // skip-complete) is NOT a tool failure — the agent is correctly
    // constraining itself. The model gets English guidance; the user should
    // see a neutral "skipped", not a red error with internal English text.
    let guard_denial = block.status == ToolStatus::Failed && is_guard_denied_name(&block.name);
    let glyph = if guard_denial {
        "⚠"
    } else {
        tool_glyph(block.status)
    };
    let glyph_style = if guard_denial {
        Style::default().fg(theme.warning)
    } else {
        tool_style(theme, block.status)
    };
    // Prefer taxonomy presentation; task keeps the localized unsupported label.
    let action = if block.name == "task" {
        t.unsupported_task_action.to_string()
    } else {
        // Infer locale from the chrome language of the unsupported-task string:
        // callers always pass the active locale's UiText; map via presentation for both.
        let locale = if t.unsupported_task_action.starts_with("Delegation") {
            crate::i18n::Locale::En
        } else {
            crate::i18n::Locale::Zh
        };
        tool_action_label_for(&block.name, locale)
    };

    // Compact default row: glyph + presentation + one-line summary.
    // Only the glyph carries a status color; the action name is theme.tool.
    let mut head = vec![
        Span::styled(format!("{glyph} "), glyph_style),
        Span::styled(action.clone(), Style::default().fg(theme.tool)),
    ];
    let target = tool_summary_for(&block.name, &block.arguments, t);
    if !target.is_empty() && target != "{}" {
        let used = 2 + UnicodeWidthStr::width(action.as_str()) + 1;
        head.push(Span::raw("  "));
        head.push(Span::styled(
            truncate_display(&target, width.saturating_sub(used + 8).max(8)),
            Style::default().fg(theme.text),
        ));
    }
    match block.status {
        ToolStatus::Running => {
            head.push(Span::styled(
                " …".to_string(),
                Style::default().fg(theme.dim),
            ));
        }
        _ => {
            if let Some(ms) = block.duration_ms.filter(|ms| *ms >= 1000) {
                head.push(Span::styled(
                    format!(" · {:.1}s", ms as f64 / 1000.0),
                    Style::default().fg(theme.dim),
                ));
            }
        }
    }
    out.push(Line::from(head));

    if block.name == "apply_patch" && block.status != ToolStatus::Failed {
        inline_diff_lines(&block.arguments, theme, width, tools_expanded, t, out);
        return;
    }
    // `replace` is the same edit action with old/new arguments instead of a
    // patch — synthesize the patch body so it renders the same inline diff.
    // Failures keep the plain preview path: the error line is more useful
    // than a diff that never landed.
    if block.name == "replace"
        && block.status != ToolStatus::Failed
        && let Some(patch) = replace_patch_from_arguments(&block.arguments)
    {
        inline_diff_lines(&patch, theme, width, tools_expanded, t, out);
        return;
    }

    // Prefer the structured goal summary over the runtime's internal preview
    // ("Goal resolved.") so expand shows what the model actually wrote.
    let goal_body = if block.name == "update_goal" {
        update_goal_summary_from_arguments(&block.arguments, t)
    } else {
        String::new()
    };
    let Some(raw_preview) = (if !goal_body.is_empty() {
        Some(goal_body.as_str())
    } else {
        block.preview.as_deref().filter(|p| !p.trim().is_empty())
    }) else {
        return;
    };
    let actionable_preview;
    let preview = if guard_denial {
        // Guard denials carry internal English guidance for the model
        // (skip-complete, closeout, loop-guard). Users just need to know the
        // action was turned down, not the raw guard text.
        guard_denial_note(&block.name, t)
    } else if block.name == "task"
        && (raw_preview.contains("unknown tool") || raw_preview.contains("spawn_agent"))
    {
        actionable_preview = t.unsupported_task_hint.to_string();
        actionable_preview.as_str()
    } else {
        raw_preview
    };
    // Everything collapses by default — only a user Ctrl+O expands. A failure is
    // no exception: its collapsed first line is usually the error itself (a
    // benign wrong-flag failure that dumps a whole help page must not flood the
    // view or bloat the live footer); Ctrl+O reveals the rest.
    // update_goal stays in noisy_success_tool: collapsed = one head line only;
    // expanded = full summary body (the … on the head is just the compact clip).
    let expand = tools_expanded;
    if !expand && block.status == ToolStatus::Ok && noisy_success_tool(&block.name) {
        return;
    }
    let inner = width.saturating_sub(4).max(1);
    let lines = wrap(preview, inner);
    // Long tool output (logs, help dumps): treat many lines as "long" even when
    // expanded so the footer cannot grow without bound.
    const MAX_EXPANDED_PREVIEW: usize = 24;
    const LONG_THRESHOLD: usize = 8;
    if expand {
        let shown = lines.len().min(MAX_EXPANDED_PREVIEW);
        for (i, line) in lines.iter().take(shown).enumerate() {
            let lead = if i == 0 { "  └ " } else { "    " };
            out.push(Line::from(vec![
                Span::styled(lead, Style::default().fg(theme.dim)),
                Span::styled(line.clone(), Style::default().fg(theme.muted)),
            ]));
        }
        if lines.len() > shown {
            out.push(Line::from(Span::styled(
                format!(
                    "    {}",
                    t.fold_more_lines
                        .replace("{}", &(lines.len() - shown).to_string())
                ),
                Style::default().fg(theme.dim),
            )));
        }
    } else {
        // Collapsed: exactly one preview row (truncated) + fold hint when long.
        let first = lines.first().map(|s| s.as_str()).unwrap_or("");
        let preview_w = inner.saturating_sub(18).max(12);
        let one = truncate_display(first, preview_w);
        let more = lines.len().saturating_sub(1);
        let long = lines.len() >= LONG_THRESHOLD || more > 0;
        let hint = if long {
            if more > 0 {
                format!("  {}", t.fold_more_lines_short.replace("{}", &more.to_string()))
            } else {
                "  (Ctrl+O)".to_string()
            }
        } else {
            String::new()
        };
        out.push(Line::from(vec![
            Span::styled("  └ ", Style::default().fg(theme.dim)),
            Span::styled(one, Style::default().fg(theme.muted)),
            Span::styled(hint, Style::default().fg(theme.dim)),
        ]));
    }
}

/// Synthesize an apply_patch-style body from `replace` arguments (`path` /
/// `old` / `new`) so replace edits share [`inline_diff_lines`]. `None` when
/// the arguments don't parse or carry no text on either side.
fn replace_patch_from_arguments(arguments: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(arguments).ok()?;
    let path = value.get("path")?.as_str()?;
    let old = value.get("old")?.as_str()?;
    let new = value.get("new")?.as_str()?;
    if old.is_empty() && new.is_empty() {
        return None;
    }
    let mut patch = format!("*** Update File: {path}\n");
    for line in old.lines() {
        patch.push('-');
        patch.push_str(line);
        patch.push('\n');
    }
    for line in new.lines() {
        patch.push('+');
        patch.push_str(line);
        patch.push('\n');
    }
    Some(patch)
}

fn inline_diff_lines(
    arguments: &str,
    theme: &Theme,
    width: usize,
    tools_expanded: bool,
    t: &crate::i18n::UiText,
    out: &mut Vec<Line<'static>>,
) {
    const DIFF_FOLD_ROWS: usize = 12;
    let patch = patch_text_from_arguments(arguments);

    let rows: Vec<&str> = patch
        .lines()
        .filter(|l| {
            !l.starts_with("*** Begin Patch") && !l.starts_with("*** End Patch") && *l != "@@"
        })
        .collect();
    if rows.is_empty() {
        return;
    }

    let cap = if tools_expanded { 40 } else { DIFF_FOLD_ROWS };
    let shown = rows.len().min(cap);
    let inner = width.saturating_sub(4).max(8);
    for (i, raw) in rows.iter().take(shown).enumerate() {
        let style = if let Some(file) = raw.strip_prefix("*** Update File: ") {
            let lead = if i == 0 { "  └ " } else { "    " };
            out.push(Line::from(vec![
                Span::styled(lead, Style::default().fg(theme.dim)),
                Span::styled(
                    truncate_display(file, inner),
                    Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
                ),
            ]));
            continue;
        } else if raw.starts_with("*** ") {
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD)
        } else if raw.starts_with('+') {
            Style::default().fg(theme.diff_add)
        } else if raw.starts_with('-') {
            Style::default().fg(theme.diff_remove)
        } else {
            Style::default().fg(theme.muted)
        };
        let lead = if i == 0 { "  └ " } else { "    " };
        out.push(Line::from(vec![
            Span::styled(lead, Style::default().fg(theme.dim)),
            Span::styled(truncate_display(raw, inner), style),
        ]));
    }
    if rows.len() > shown {
        let hint = if tools_expanded {
            format!(
                "    {}",
                t.fold_more_lines
                    .replace("{}", &(rows.len() - shown).to_string())
            )
        } else {
            format!("    {}", t.fold_full_diff)
        };
        out.push(Line::from(Span::styled(hint, Style::default().fg(theme.dim))));
    }
}

/// Merge identity for consecutive same-file edits: the patch's touched files.
pub(crate) fn patch_files_key(arguments: &str) -> String {
    patch_touched_files(&patch_text_from_arguments(arguments)).join("\u{1}")
}

/// Hunk count and +/- line totals over one patch argument string.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PatchStats {
    pub hunks: usize,
    pub added: usize,
    pub removed: usize,
}

pub(crate) fn patch_stats(arguments: &str) -> PatchStats {
    let patch = patch_text_from_arguments(arguments);
    let mut stats = PatchStats::default();
    for line in patch.lines() {
        let line = line.trim_start();
        if line.starts_with("@@") {
            stats.hunks += 1;
        } else if line.starts_with('+') && !line.starts_with("+++") {
            stats.added += 1;
        } else if line.starts_with('-') && !line.starts_with("---") {
            stats.removed += 1;
        }
    }
    stats
}

/// Combined inline diff rows for one or more same-file patches (merged edit
/// node in the activity stream). File-marker rows are skipped — the unit's
/// argument line already shows the file. Capped, with a fold hint.
pub(crate) fn merged_diff_rows(
    calls: &[&ToolCallBlock],
    theme: &Theme,
    width: usize,
    expanded: bool,
    t: &crate::i18n::UiText,
    out: &mut Vec<Line<'static>>,
) {
    const DIFF_FOLD_ROWS: usize = 12;
    let mut rows: Vec<String> = Vec::new();
    for call in calls {
        let patch = patch_text_from_arguments(&call.arguments);
        for line in patch.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("***") || trimmed == "@@" {
                continue;
            }
            if trimmed.starts_with("@@") {
                rows.push(trimmed.to_string());
            } else {
                rows.push(line.to_string());
            }
        }
    }
    if rows.is_empty() {
        return;
    }
    let cap = if expanded { 40 } else { DIFF_FOLD_ROWS };
    let shown = rows.len().min(cap);
    let inner = width.saturating_sub(4).max(8);
    for raw in rows.iter().take(shown) {
        let style = if raw.starts_with("@@") {
            Style::default().fg(theme.dim)
        } else if raw.starts_with('+') {
            Style::default().fg(theme.diff_add)
        } else if raw.starts_with('-') {
            Style::default().fg(theme.diff_remove)
        } else {
            Style::default().fg(theme.muted)
        };
        out.push(Line::from(vec![
            Span::styled("    ", Style::default()),
            Span::styled(truncate_display(raw, inner), style),
        ]));
    }
    if rows.len() > shown {
        let hint = if expanded {
            format!(
                "    {}",
                t.fold_more_lines
                    .replace("{}", &(rows.len() - shown).to_string())
            )
        } else {
            format!("    {}", t.fold_full_diff)
        };
        out.push(Line::from(Span::styled(hint, Style::default().fg(theme.dim))));
    }
}

#[cfg(test)]
mod m1_tests {
    use super::*;
    use crate::i18n::Locale;
    use crate::transcript::ToolStatus;

    #[test]
    fn compact_heading_read_edit_run() {
        let read = compact_tool_heading(
            ToolStatus::Ok,
            "read_file",
            r#"{"path":"src/lib.rs","start_line":1,"end_line":10}"#,
            Locale::En,
        );
        assert!(read.starts_with('✓'), "{read}");
        assert!(read.contains("Read"), "{read}");
        assert!(read.contains("src/lib.rs"), "{read}");

        let edit = compact_tool_heading(
            ToolStatus::Ok,
            "apply_patch",
            r#"{"patch":"*** Begin Patch\n*** Update File: crates/leveler-tui/src/theme.rs\n*** End Patch"}"#,
            Locale::En,
        );
        assert!(edit.contains("Edit"), "{edit}");
        assert!(
            edit.contains("theme.rs") || edit.contains("leveler-tui"),
            "{edit}"
        );

        let run = compact_tool_heading(
            ToolStatus::Running,
            "run_command",
            r#"{"program":"cargo","args":["test","-p","leveler-tui"]}"#,
            Locale::En,
        );
        assert!(run.starts_with('◌'), "{run}");
        assert!(run.contains("Run command"), "{run}");
        assert!(run.contains("cargo test"), "{run}");
    }

    #[test]
    fn presentation_labels_match_taxonomy() {
        assert_eq!(tool_action_label("read_file"), "读取文件");
        assert_eq!(
            tool_action_label_for("run_command", Locale::En),
            "Run command"
        );
        assert_eq!(tool_action_label_for("run_command", Locale::Zh), "执行命令");
        assert_eq!(tool_action_label_for("apply_patch", Locale::Zh), "编辑文件");
    }
}

pub(crate) fn render_tools_screen(frame: &mut Frame, area: Rect, state: &AppState) {
    let theme = &state.theme;
    let t = state.t();
    let filter = state.tools_screen.filter;
    let calls: Vec<&ToolCallBlock> = state
        .transcript
        .tool_calls()
        .into_iter()
        .filter(|b| filter.matches(b))
        .collect();

    let [list_area, detail_area] =
        Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)]).areas(area);

    let selected = state
        .tools_screen
        .selected
        .min(calls.len().saturating_sub(1));
    let mut rows: Vec<Line> = Vec::new();
    if calls.is_empty() {
        rows.push(Line::from(Span::styled(
            t.tools_none,
            Style::default().fg(theme.muted),
        )));
    }
    let list_width = list_area.width.saturating_sub(2).max(1) as usize;
    for (i, block) in calls.iter().enumerate() {
        let cursor = if i == selected { "› " } else { "  " };
        let dur = block
            .duration_ms
            .map(|ms| format!("{ms}ms"))
            .unwrap_or_default();
        let target = tool_summary_for(&block.name, &block.arguments, t);
        let label = if target.is_empty() || target == "{}" {
            block.name.clone()
        } else {
            format!("{} · {target}", block.name)
        };
        let dur_width = if dur.is_empty() {
            0
        } else {
            UnicodeWidthStr::width(dur.as_str()) + 2
        };
        let label_width = list_width.saturating_sub(4 + dur_width).max(8);
        rows.push(Line::from(vec![
            Span::styled(cursor, Style::default().fg(theme.accent)),
            Span::styled(
                format!("{} ", tool_glyph(block.status)),
                tool_style(theme, block.status),
            ),
            Span::raw(truncate_display(&label, label_width)),
            Span::styled(format!("  {dur}"), Style::default().fg(theme.dim)),
        ]));
    }
    let list_block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(Style::default().fg(theme.border))
        .title(Span::styled(
            format!(" {} · {} ", t.tools_col_tool, filter.label()),
            Style::default().fg(theme.muted),
        ));
    let list_inner = list_block.inner(list_area);
    frame.render_widget(list_block, list_area);
    crate::render::render_list_focused(frame, list_inner, rows, selected);

    let mut detail: Vec<Line> = Vec::new();
    if let Some(block) = calls.get(selected) {
        detail.push(Line::from(vec![
            Span::styled(
                format!("{}  ", t.tools_col_tool),
                Style::default().fg(theme.muted),
            ),
            Span::raw(block.name.clone()),
        ]));
        detail.extend(tool_argument_lines(
            &block.arguments,
            theme,
            detail_area.width.saturating_sub(1).max(1) as usize,
            t,
        ));
        let status = match block.status {
            ToolStatus::Running => t.tools_status_running,
            ToolStatus::Ok => t.tools_status_ok,
            ToolStatus::Failed => t.tools_status_attention,
        };
        detail.push(Line::from(vec![
            Span::styled(
                format!("{}  ", t.tools_col_status),
                Style::default().fg(theme.muted),
            ),
            Span::styled(status, tool_style(theme, block.status)),
        ]));
        if let Some(ms) = block.duration_ms {
            detail.push(Line::from(vec![
                Span::styled(
                    format!("{}  ", t.tools_col_duration),
                    Style::default().fg(theme.muted),
                ),
                Span::styled(format!("{ms}ms"), Style::default().fg(theme.dim)),
            ]));
        }
        if let Some(preview) = &block.preview {
            detail.push(Line::from(""));
            detail.push(Line::from(Span::styled(
                t.tools_output,
                Style::default().fg(theme.muted),
            )));
            for line in wrap(preview, detail_area.width.saturating_sub(1).max(1) as usize) {
                detail.push(Line::from(Span::raw(line)));
            }
        }
    }
    detail.push(Line::from(""));
    let footer = tools_footer_hint(detail_area.width.saturating_sub(1).max(1) as usize, t);
    detail.push(Line::from(Span::styled(
        footer,
        Style::default().fg(theme.muted),
    )));
    crate::render::render_scrolled(frame, detail_area, state, detail);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_footer_hint_fits_available_width() {
        let t = crate::i18n::Locale::Zh.text();
        for width in [8, 16, 24, 32, 48, 80] {
            let hint = tools_footer_hint(width, t);
            assert!(
                hint.width() <= width,
                "footer `{hint}` should fit in {width} columns"
            );
        }

        assert!(tools_footer_hint(32, t).contains("Esc 返回"));

        let en = crate::i18n::Locale::En.text();
        assert!(tools_footer_hint(80, en).contains("Esc back"));
    }

    fn failed_block(preview: &str) -> ToolCallBlock {
        ToolCallBlock {
            id: leveler_client_protocol::ToolCallId::new("c1"),
            name: "run_command".to_string(),
            arguments: "{}".to_string(),
            status: ToolStatus::Failed,
            preview: Some(preview.to_string()),
            duration_ms: None,
            parallel: false,
        }
    }

    #[test]
    fn a_failed_command_collapses_by_default_showing_only_its_first_line() {
        // Default is collapsed for everything, failures included: show the first
        // line (usually the error) + a Ctrl+O hint, never the whole dump.
        let preview: String = (0..40)
            .map(|i| format!("usage line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut out = Vec::new();
        tool_lines(
            &failed_block(&preview),
            &Theme::no_color(),
            80,
            false,
            crate::i18n::Locale::Zh.text(),
            &mut out,
        );
        let text: Vec<String> = out.iter().map(|l| l.to_string()).collect();
        let body = text.iter().filter(|l| l.contains("usage line")).count();
        assert_eq!(body, 1, "collapsed shows only the first line: {text:?}");
        assert!(
            text.iter()
                .any(|l| l.contains("+39 行") && l.contains("Ctrl+O")),
            "must hint how to expand: {text:?}"
        );
    }

    /// `replace` is an edit tool like `apply_patch`; its cell must render the
    /// same inline colored diff (file header + −old/+new rows), not just a
    /// collapsed preview line. The diff is synthesized from the old/new
    /// arguments since replace carries no patch text.
    #[test]
    fn replace_edit_renders_an_inline_diff_like_apply_patch() {
        let block = ToolCallBlock {
            id: leveler_client_protocol::ToolCallId::new("r1"),
            name: "replace".to_string(),
            arguments: r#"{"path":"src/lib.rs","old":"fn old() {}","new":"fn renamed() {}\nfn extra() {}"}"#
                .to_string(),
            status: ToolStatus::Ok,
            preview: Some("replaced 1 occurrence".to_string()),
            duration_ms: None,
            parallel: false,
        };
        let mut out = Vec::new();
        tool_lines(
            &block,
            &Theme::no_color(),
            80,
            false,
            crate::i18n::Locale::Zh.text(),
            &mut out,
        );
        let text: Vec<String> = out.iter().map(|l| l.to_string()).collect();
        assert!(
            text.iter().any(|l| l.contains("src/lib.rs")),
            "diff must name the file: {text:?}"
        );
        assert!(
            text.iter().any(|l| l.contains("-fn old() {}")),
            "old text must render as removed rows: {text:?}"
        );
        assert!(
            text.iter().any(|l| l.contains("+fn renamed() {}"))
                && text.iter().any(|l| l.contains("+fn extra() {}")),
            "new text must render as added rows: {text:?}"
        );
    }

    /// A failed replace keeps the plain preview path — the error line is more
    /// useful than a diff that never landed.
    #[test]
    fn failed_replace_shows_the_error_not_a_diff() {
        let block = ToolCallBlock {
            id: leveler_client_protocol::ToolCallId::new("r2"),
            name: "replace".to_string(),
            arguments: r#"{"path":"src/lib.rs","old":"zzz","new":"q"}"#.to_string(),
            status: ToolStatus::Failed,
            preview: Some("old text not found in src/lib.rs".to_string()),
            duration_ms: None,
            parallel: false,
        };
        let mut out = Vec::new();
        tool_lines(
            &block,
            &Theme::no_color(),
            80,
            false,
            crate::i18n::Locale::Zh.text(),
            &mut out,
        );
        let text: Vec<String> = out.iter().map(|l| l.to_string()).collect();
        assert!(
            text.iter().any(|l| l.contains("old text not found")),
            "the failure preview must render: {text:?}"
        );
        assert!(
            !text.iter().any(|l| l.contains("+q")),
            "no diff rows for an edit that never landed: {text:?}"
        );
    }

    #[test]
    fn rejected_update_plan_hides_the_internal_validation_text() {
        // A skip-complete rejection returns an English guard message meant for
        // the model. The user sees a short localized "plan unchanged" line, not
        // the raw `— plan step "…" cannot …` internals.
        let block = ToolCallBlock {
            id: leveler_client_protocol::ToolCallId::new("p1"),
            name: "update_plan".to_string(),
            arguments: r#"{"explanation":"现在开始第3步"}"#.to_string(),
            status: ToolStatus::Failed,
            preview: Some(
                "— plan step \"创建项目结构\" cannot be completed while step 1 is in_progress"
                    .to_string(),
            ),
            duration_ms: None,
            parallel: false,
        };
        let mut out = Vec::new();
        tool_lines(
            &block,
            &Theme::no_color(),
            80,
            false,
            crate::i18n::Locale::Zh.text(),
            &mut out,
        );
        let text = out
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !text.contains("plan step") && !text.contains("cannot"),
            "internal English validation text must not reach the user: {text}"
        );
        assert!(
            text.contains("计划未更新"),
            "a short localized 'plan unchanged' note must show instead: {text}"
        );
    }

    #[test]
    fn closeout_denied_list_files_reads_as_a_neutral_skip_not_a_red_error() {
        // After the plan completes, a repeat list_files is turned down with an
        // English nudge. The user must see a neutral "skipped" (⚠, not ✗) with
        // a localized note — never the internal English guard text.
        let block = ToolCallBlock {
            id: leveler_client_protocol::ToolCallId::new("l1"),
            name: "list_files".to_string(),
            arguments: r#"{"path":"backend"}"#.to_string(),
            status: ToolStatus::Failed,
            preview: Some(
                "Plan steps are complete. Do not re-check git status, re-list files, or \
                 re-audit prior questions — reply with a final summary only."
                    .to_string(),
            ),
            duration_ms: None,
            parallel: false,
        };
        let mut out = Vec::new();
        tool_lines(
            &block,
            &Theme::no_color(),
            80,
            false,
            crate::i18n::Locale::Zh.text(),
            &mut out,
        );
        let text = out
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !text.contains("Plan steps are complete") && !text.contains("re-check"),
            "internal English guard text must not reach the user: {text}"
        );
        assert!(
            text.contains("已跳过"),
            "a localized 'skipped' note must show instead: {text}"
        );
        assert!(
            text.contains('⚠'),
            "a guard denial reads as a warning glyph, not the failure marker: {text}"
        );
        assert!(
            !text.contains('✗'),
            "a guard denial must not use the failure glyph: {text}"
        );
    }

    #[test]
    fn user_ctrl_o_expand_shows_the_full_tail() {
        let preview: String = (0..40)
            .map(|i| format!("usage line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut out = Vec::new();
        tool_lines(
            &failed_block(&preview),
            &Theme::no_color(),
            80,
            true,
            crate::i18n::Locale::Zh.text(),
            &mut out,
        );
        let body = out
            .iter()
            .filter(|l| l.to_string().contains("usage line"))
            .count();
        // Expanded is hard-capped so runaway logs cannot flood the footer.
        assert_eq!(body, 24, "user expand shows up to the footer cap: {body}");
        assert!(
            out.iter()
                .any(|l| l.to_string().contains("还有") && l.to_string().contains("16")),
            "must note remaining lines: {out:?}"
        );
    }

    #[test]
    fn tool_argument_lines_pretty_prints_json_object() {
        let theme = Theme::no_color();
        let lines = tool_argument_lines(
            r#"{"end_line":1,"path":"package.json","start_line":1}"#,
            &theme,
            80,
            crate::i18n::Locale::Zh.text(),
        );
        let rendered: Vec<String> = lines.into_iter().map(|line| line.to_string()).collect();

        assert_eq!(rendered[0], "参数");
        assert!(rendered.iter().any(|line| line == "  path: package.json"));
        assert!(rendered.iter().any(|line| line == "  start_line: 1"));
        assert!(rendered.iter().any(|line| line == "  end_line: 1"));
    }

    #[test]
    fn unsupported_task_call_is_actionable_instead_of_raw() {
        let block = ToolCallBlock {
            id: leveler_client_protocol::ToolCallId::new("task-1"),
            name: "task".into(),
            arguments: r#"{"description":"Explore provider architecture","prompt":"Read the core client files"}"#.into(),
            status: ToolStatus::Failed,
            preview: Some("tool error: unknown tool `task`".into()),
            duration_ms: None,
            parallel: false,
        };
        let mut out = Vec::new();
        tool_lines(
            &block,
            &Theme::no_color(),
            80,
            false,
            crate::i18n::Locale::Zh.text(),
            &mut out,
        );
        let text = out
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            text.contains("委派（不支持）") && text.contains("Explore provider architecture"),
            "{text}"
        );
        assert!(text.contains("不支持 task，请改用 spawn_agent"), "{text}");
        assert!(!text.contains("{\"description\""), "{text}");
        assert!(!text.contains("unknown tool"), "{text}");
    }

    #[test]
    fn tool_summary_compacts_absolute_project_paths() {
        let read = tool_summary(
            "read_file",
            r#"{"path":"/Users/example/projects/sample-project/src/upstream/openaiCompatClient.ts","start_line":1,"end_line":20}"#,
        );
        assert_eq!(read, "src/upstream/openaiCompatClient.ts:1-20");

        let grep = tool_summary(
            "grep",
            r#"{"pattern":"retry","path":"/Users/example/projects/sample-project/test/upstream.retry.test.ts"}"#,
        );
        assert_eq!(grep, r#""retry" in test/upstream.retry.test.ts"#);

        let root_file = tool_summary(
            "read_file",
            r#"{"path":"/Users/example/projects/sample-project/package.json"}"#,
        );
        assert_eq!(root_file, "package.json");
    }

    #[test]
    fn shell_command_shows_cmd_not_raw_json() {
        let s = tool_summary("shell_command", r#"{"cmd":"cargo test --workspace"}"#);
        assert_eq!(s, "cargo test --workspace");
        assert!(!s.contains('{'), "{s}");
        assert!(!s.contains("cmd"), "{s}");
    }

    #[test]
    fn shell_command_strips_leading_cd_and_compacts_home() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/example".into());
        let args =
            format!(r#"{{"cmd":"cd {home}/Develop/app/codeleveler && make run-admin-local"}}"#);
        let s = tool_summary("shell_command", &args);
        assert_eq!(s, "make run-admin-local");
        assert!(!s.contains(&home), "must not leak absolute home path: {s}");
        assert!(!s.contains('{'), "{s}");
    }

    #[test]
    fn run_command_unwraps_bash_c_and_strips_cd() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/example".into());
        let args = format!(
            r#"{{"program":"bash","args":["-c","cd {home}/Develop/app/codeleveler && cargo test -q"]}}"#
        );
        let s = tool_summary("run_command", &args);
        assert_eq!(s, "cargo test -q");
    }

    #[test]
    fn unknown_tool_never_dumps_raw_json_arguments() {
        let s = tool_summary(
            "custom_mcp_tool",
            r#"{"path":"/Users/example/projects/sample-project/src/lib.rs","verbose":true}"#,
        );
        assert!(!s.contains('{'), "raw JSON leaked: {s}");
        assert!(s.contains("lib.rs") || s.contains("src/"), "{s}");
    }

    #[test]
    fn replace_summary_prefers_target_path_over_raw_json() {
        let summary = tool_summary(
            "replace",
            r#"{"path":"/Users/example/projects/sample-project/test/doctor.test.ts","old":"before","new":"after"}"#,
        );
        assert_eq!(summary, "test/doctor.test.ts");
    }

    #[test]
    fn replace_summary_without_path_stays_human_readable() {
        let summary = tool_summary(
            "replace",
            r#"{"new":"    expect(provItem.message).toContain(\"key OK\")","old":"    expect(provItem.message).toContain(\"ok\")"}"#,
        );
        assert_eq!(summary, "文本替换");
    }

    #[test]
    fn replace_summary_with_invalid_json_stays_human_readable() {
        let summary = tool_summary(
            "replace",
            "{\"old\":\"before\",\"new\":\"after with\nraw newline\"}",
        );
        assert_eq!(summary, "文本替换");
    }

    #[test]
    fn apply_patch_summary_without_file_stays_human_readable() {
        let summary = tool_summary("apply_patch", "*** Begin Patch\nnot a patch\n*** End Patch");
        assert_eq!(summary, "补丁");
    }

    #[test]
    fn apply_patch_summary_handles_indented_patch_headers() {
        let summary = tool_summary(
            "apply_patch",
            r#"{"patch":"*** Begin Patch\n  *** Update File: /Users/example/projects/sample-project/src/doctor.ts\n@@\n-old\n+new\n*** End Patch"}"#,
        );
        assert_eq!(summary, "src/doctor.ts");
    }

    #[test]
    fn apply_patch_summary_handles_unified_diff_paths() {
        let summary = tool_summary(
            "apply_patch",
            r#"{"patch":"diff --git a/src/doctor.ts b/src/doctor.ts\n--- a/src/doctor.ts\n+++ b/src/doctor.ts\n@@\n-old\n+new"}"#,
        );
        assert_eq!(summary, "src/doctor.ts");
    }

    #[test]
    fn apply_patch_summary_scans_json_string_values_for_patch_text() {
        let summary = tool_summary(
            "apply_patch",
            r#"{"input":"*** Begin Patch\n*** Update File: test/doctor.test.ts\n@@\n-old\n+new\n*** End Patch"}"#,
        );
        assert_eq!(summary, "test/doctor.test.ts");
    }

    #[test]
    fn apply_patch_summary_scans_nested_json_string_values_for_patch_text() {
        let summary = tool_summary(
            "apply_patch",
            r#"{"input":{"patch":"*** Begin Patch\n*** Update File: src/cli.ts\n@@\n-old\n+new\n*** End Patch"}}"#,
        );
        assert_eq!(summary, "src/cli.ts");
    }
}
