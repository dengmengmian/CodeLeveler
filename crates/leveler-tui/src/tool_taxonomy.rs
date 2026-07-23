//! Harness-independent tool presentation vocabulary for the TUI.
//!
//! Every built-in tool name maps to a kind, bilingual presentation labels, and
//! a read-only default. Presentation is display-only; the model still sees the
//! real tool name.

use crate::i18n::Locale;

/// How loudly a tool call should appear in the Conversation activity stream.
///
/// Conversation is a product surface, not a raw tool trace: hide internal
/// exploration noise, keep short normal progress, and emphasize writes/runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityVisibility {
    /// Do not enter Conversation (file scans, path enumeration, existence probes).
    Silent,
    /// Compact per-call progress unit (reads/searches) — one three-line unit each.
    Normal,
    /// Always show: edits, shell runs, plans, user prompts.
    /// (Goal bookkeeping is Silent — not a product activity line.)
    Important,
}

/// Semantic tool kind used for presentation grouping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    Read,
    Edit,
    Write,
    ListDir,
    Search,
    Lsp,
    Execute,
    Plan,
    WebSearch,
    Skill,
    Memory,
    Checkpoint,
    Media,
    Goal,
    AskUser,
    Other,
}

/// One taxonomy entry.
#[derive(Debug, Clone, Copy)]
pub struct ToolTaxonomyEntry {
    pub name: &'static str,
    pub kind: ToolKind,
    pub presentation_en: &'static str,
    pub presentation_zh: &'static str,
    pub read_only_default: bool,
    pub visibility: ActivityVisibility,
}

/// Built-in + virtual agent tools known to the TUI.
///
/// Completeness against `leveler_tools::full_registry()` is asserted in tests.
pub static BUILTIN_TAXONOMY: &[ToolTaxonomyEntry] = &[
    ToolTaxonomyEntry {
        name: "read_file",
        kind: ToolKind::Read,
        presentation_en: "Read file",
        presentation_zh: "读取文件",
        read_only_default: true,
        visibility: ActivityVisibility::Normal,
    },
    ToolTaxonomyEntry {
        name: "list_files",
        kind: ToolKind::ListDir,
        presentation_en: "List Files",
        presentation_zh: "列目录",
        read_only_default: true,
        visibility: ActivityVisibility::Silent,
    },
    ToolTaxonomyEntry {
        name: "grep",
        kind: ToolKind::Search,
        presentation_en: "Search code",
        presentation_zh: "搜索代码",
        read_only_default: true,
        visibility: ActivityVisibility::Normal,
    },
    ToolTaxonomyEntry {
        name: "find_files",
        kind: ToolKind::Search,
        presentation_en: "Find files",
        presentation_zh: "查找文件",
        read_only_default: true,
        visibility: ActivityVisibility::Normal,
    },
    ToolTaxonomyEntry {
        name: "find_symbol",
        kind: ToolKind::Lsp,
        presentation_en: "Code Intelligence",
        presentation_zh: "读符号",
        read_only_default: true,
        visibility: ActivityVisibility::Normal,
    },
    ToolTaxonomyEntry {
        name: "read_symbol",
        kind: ToolKind::Lsp,
        presentation_en: "Code Intelligence",
        presentation_zh: "读符号",
        read_only_default: true,
        visibility: ActivityVisibility::Normal,
    },
    ToolTaxonomyEntry {
        name: "find_references",
        kind: ToolKind::Lsp,
        presentation_en: "Code Intelligence",
        presentation_zh: "查引用",
        read_only_default: true,
        visibility: ActivityVisibility::Normal,
    },
    ToolTaxonomyEntry {
        name: "diagnostics",
        kind: ToolKind::Lsp,
        presentation_en: "Diagnostics",
        presentation_zh: "诊断",
        read_only_default: true,
        visibility: ActivityVisibility::Normal,
    },
    ToolTaxonomyEntry {
        name: "blast_radius",
        kind: ToolKind::Lsp,
        presentation_en: "Code Intelligence",
        presentation_zh: "影响面",
        read_only_default: true,
        visibility: ActivityVisibility::Normal,
    },
    ToolTaxonomyEntry {
        name: "apply_patch",
        kind: ToolKind::Edit,
        presentation_en: "Edit file",
        presentation_zh: "编辑文件",
        read_only_default: false,
        visibility: ActivityVisibility::Important,
    },
    ToolTaxonomyEntry {
        name: "replace",
        kind: ToolKind::Edit,
        presentation_en: "Edit file",
        presentation_zh: "编辑文件",
        read_only_default: false,
        visibility: ActivityVisibility::Important,
    },
    ToolTaxonomyEntry {
        name: "run_command",
        kind: ToolKind::Execute,
        presentation_en: "Run command",
        presentation_zh: "执行命令",
        read_only_default: false,
        // Shell may still demote to Silent for ls/tree/find/pwd probes.
        visibility: ActivityVisibility::Important,
    },
    ToolTaxonomyEntry {
        name: "shell_command",
        kind: ToolKind::Execute,
        presentation_en: "Run command",
        presentation_zh: "执行命令",
        read_only_default: false,
        visibility: ActivityVisibility::Important,
    },
    ToolTaxonomyEntry {
        name: "update_plan",
        kind: ToolKind::Plan,
        presentation_en: "Plan",
        presentation_zh: "更新计划",
        read_only_default: true,
        visibility: ActivityVisibility::Important,
    },
    ToolTaxonomyEntry {
        name: "web_search",
        kind: ToolKind::WebSearch,
        presentation_en: "Web Search",
        presentation_zh: "联网搜索",
        read_only_default: true,
        visibility: ActivityVisibility::Normal,
    },
    ToolTaxonomyEntry {
        name: "web_fetch",
        kind: ToolKind::WebSearch,
        presentation_en: "Fetch URL",
        presentation_zh: "拉取网页",
        read_only_default: true,
        visibility: ActivityVisibility::Normal,
    },
    ToolTaxonomyEntry {
        name: "get_task",
        kind: ToolKind::Execute,
        presentation_en: "Get Task",
        presentation_zh: "任务状态",
        read_only_default: true,
        visibility: ActivityVisibility::Normal,
    },
    ToolTaxonomyEntry {
        name: "wait_task",
        kind: ToolKind::Execute,
        presentation_en: "Wait Task",
        presentation_zh: "等待任务",
        read_only_default: true,
        visibility: ActivityVisibility::Important,
    },
    ToolTaxonomyEntry {
        name: "kill_task",
        kind: ToolKind::Execute,
        presentation_en: "Kill Task",
        presentation_zh: "终止任务",
        read_only_default: false,
        visibility: ActivityVisibility::Important,
    },
    ToolTaxonomyEntry {
        name: "view_image",
        kind: ToolKind::Media,
        presentation_en: "View Image",
        presentation_zh: "查看图片",
        read_only_default: true,
        visibility: ActivityVisibility::Normal,
    },
    ToolTaxonomyEntry {
        name: "git_status",
        kind: ToolKind::Search,
        presentation_en: "Git Status",
        presentation_zh: "git 状态",
        read_only_default: true,
        visibility: ActivityVisibility::Silent,
    },
    ToolTaxonomyEntry {
        name: "git_diff",
        kind: ToolKind::Search,
        presentation_en: "Git Diff",
        presentation_zh: "git diff",
        read_only_default: true,
        visibility: ActivityVisibility::Normal,
    },
    ToolTaxonomyEntry {
        name: "create_checkpoint",
        kind: ToolKind::Checkpoint,
        presentation_en: "Checkpoint",
        presentation_zh: "创建检查点",
        read_only_default: true,
        visibility: ActivityVisibility::Silent,
    },
    ToolTaxonomyEntry {
        name: "restore_checkpoint",
        kind: ToolKind::Checkpoint,
        presentation_en: "Restore Checkpoint",
        presentation_zh: "恢复检查点",
        read_only_default: false,
        visibility: ActivityVisibility::Important,
    },
    ToolTaxonomyEntry {
        name: "load_skill",
        kind: ToolKind::Skill,
        presentation_en: "Skill",
        presentation_zh: "加载技能",
        read_only_default: true,
        visibility: ActivityVisibility::Normal,
    },
    ToolTaxonomyEntry {
        name: "create_skill",
        kind: ToolKind::Skill,
        presentation_en: "Create Skill",
        presentation_zh: "创建技能",
        read_only_default: false,
        visibility: ActivityVisibility::Important,
    },
    ToolTaxonomyEntry {
        name: "expand_tools",
        kind: ToolKind::Other,
        presentation_en: "Expand Tools",
        presentation_zh: "扩展工具",
        read_only_default: true,
        visibility: ActivityVisibility::Silent,
    },
    ToolTaxonomyEntry {
        name: "memory",
        kind: ToolKind::Memory,
        presentation_en: "Memory",
        presentation_zh: "记忆检索",
        read_only_default: true,
        visibility: ActivityVisibility::Silent,
    },
    ToolTaxonomyEntry {
        name: "remember",
        kind: ToolKind::Memory,
        presentation_en: "Remember",
        presentation_zh: "记住",
        read_only_default: false,
        visibility: ActivityVisibility::Important,
    },
    ToolTaxonomyEntry {
        name: "forget",
        kind: ToolKind::Memory,
        presentation_en: "Forget",
        presentation_zh: "遗忘",
        read_only_default: false,
        visibility: ActivityVisibility::Important,
    },
    ToolTaxonomyEntry {
        name: "consolidate_memory",
        kind: ToolKind::Memory,
        presentation_en: "Consolidate Memory",
        presentation_zh: "整理记忆",
        read_only_default: false,
        visibility: ActivityVisibility::Silent,
    },
    // Virtual / injected agent tools (not always in the tool registry).
    ToolTaxonomyEntry {
        name: "update_goal",
        kind: ToolKind::Goal,
        presentation_en: "Update Goal",
        presentation_zh: "目标收尾",
        read_only_default: true,
        // Bookkeeping for Goal mode — not a user-facing activity row. Footer /
        // recap still use the tool result; Conversation stays free of "结案" noise.
        visibility: ActivityVisibility::Silent,
    },
    ToolTaxonomyEntry {
        name: "request_user_input",
        kind: ToolKind::AskUser,
        presentation_en: "Ask User",
        presentation_zh: "询问",
        read_only_default: true,
        visibility: ActivityVisibility::Important,
    },
    ToolTaxonomyEntry {
        name: "ask_user",
        kind: ToolKind::AskUser,
        presentation_en: "Ask User",
        presentation_zh: "询问",
        read_only_default: true,
        visibility: ActivityVisibility::Important,
    },
    ToolTaxonomyEntry {
        name: "request_permissions",
        kind: ToolKind::Other,
        presentation_en: "Request Permissions",
        presentation_zh: "请求权限",
        read_only_default: true,
        visibility: ActivityVisibility::Important,
    },
    ToolTaxonomyEntry {
        name: "spawn_agent",
        kind: ToolKind::Other,
        presentation_en: "Subagent",
        presentation_zh: "子 Agent",
        read_only_default: false,
        visibility: ActivityVisibility::Important,
    },
    ToolTaxonomyEntry {
        name: "task",
        kind: ToolKind::Other,
        presentation_en: "Delegation (unsupported)",
        presentation_zh: "委派（不支持）",
        read_only_default: false,
        visibility: ActivityVisibility::Important,
    },
];

/// Look up a taxonomy entry by exact tool name.
pub fn lookup(name: &str) -> Option<&'static ToolTaxonomyEntry> {
    BUILTIN_TAXONOMY.iter().find(|e| e.name == name)
}

/// Resolve Conversation visibility for a tool call.
///
/// Shell probes (`ls`/`tree`/`find`/`pwd`/file-exists tests) demote to
/// [`ActivityVisibility::Silent`] even though `run_command` is Important by default.
///
/// `update_goal(complete)` is Silent bookkeeping; `update_goal(blocked)` is
/// Important so the user sees why work stopped.
pub fn activity_visibility(name: &str, arguments: &str) -> ActivityVisibility {
    if is_silent_shell_probe(name, arguments) {
        return ActivityVisibility::Silent;
    }
    if name == "update_goal" {
        return if update_goal_is_blocked(arguments) {
            ActivityVisibility::Important
        } else {
            ActivityVisibility::Silent
        };
    }
    lookup(name)
        .map(|e| e.visibility)
        .unwrap_or(ActivityVisibility::Normal)
}

fn update_goal_is_blocked(arguments: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(arguments)
        .ok()
        .and_then(|v| {
            v.get("status")
                .and_then(|s| s.as_str())
                .map(|s| s == "blocked")
        })
        .unwrap_or(false)
}

/// Whether this call is pure workspace exploration that should not clutter Conversation.
pub fn is_exploration_tool(name: &str) -> bool {
    matches!(
        lookup(name).map(|e| e.kind),
        Some(
            ToolKind::Read
                | ToolKind::ListDir
                | ToolKind::Search
                | ToolKind::Lsp
                | ToolKind::WebSearch
                | ToolKind::Media
        )
    ) || name == "read_file"
        || name == "list_files"
        || name == "grep"
        || name == "find_files"
}

fn is_silent_shell_probe(name: &str, arguments: &str) -> bool {
    if name != "run_command" && name != "shell_command" {
        return false;
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(arguments) else {
        return is_silent_program_line(arguments);
    };
    // shell_command uses a single `cmd` string.
    if let Some(cmd) = v
        .get("cmd")
        .or_else(|| v.get("command"))
        .and_then(|c| c.as_str())
    {
        return is_silent_program_line(cmd);
    }
    let program = v
        .get("program")
        .and_then(|p| p.as_str())
        .unwrap_or("")
        .trim();
    let args: Vec<&str> = v
        .get("args")
        .and_then(|a| a.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
        .unwrap_or_default();
    is_silent_program(program, &args)
}

fn is_silent_program_line(raw: &str) -> bool {
    let mut parts = raw.split_whitespace();
    let Some(program) = parts.next() else {
        return false;
    };
    let args: Vec<&str> = parts.collect();
    is_silent_program(program, &args)
}

fn is_silent_program(program: &str, args: &[&str]) -> bool {
    let base = std::path::Path::new(program)
        .file_name()
        .and_then(|p| p.to_str())
        .unwrap_or(program);
    match base {
        "ls" | "tree" | "find" | "pwd" | "stat" | "dirname" | "basename" | "realpath"
        | "readlink" => true,
        "test" | "[" => true, // file existence / type probes
        "which" | "command" | "type" => {
            // `which cargo` style lookups are internal harness noise.
            true
        }
        "bash" | "sh" | "zsh" | "dash" => {
            // Only demote trivial one-shot probes: `bash -c 'ls'`, `sh -c pwd`.
            shell_c_probe(args)
        }
        _ => false,
    }
}

fn shell_c_probe(args: &[&str]) -> bool {
    let Some(idx) = args.iter().position(|a| *a == "-c") else {
        return false;
    };
    let Some(script) = args.get(idx + 1).copied() else {
        return false;
    };
    let first = script
        .split(|c: char| c.is_whitespace() || c == ';' || c == '|' || c == '&')
        .find(|s| !s.is_empty())
        .unwrap_or("");
    matches!(
        first,
        "ls" | "tree" | "find" | "pwd" | "stat" | "test" | "which" | "[" | "dirname" | "basename"
    )
}

/// Localized presentation label for a tool name.
///
/// MCP tools (`mcp__server__tool`) get a short `server/tool` label. Unknown
/// names fall back to the raw tool name.
pub fn presentation_label(name: &str, locale: Locale) -> String {
    if let Some(entry) = lookup(name) {
        return match locale {
            Locale::Zh => entry.presentation_zh,
            Locale::En => entry.presentation_en,
        }
        .to_string();
    }
    if let Some(rest) = name.strip_prefix("mcp__") {
        let mut parts = rest.splitn(2, "__");
        let server = parts.next().unwrap_or("mcp");
        let tool = parts.next().unwrap_or(rest);
        return format!("{server}/{tool}");
    }
    name.to_string()
}

/// Compact one-line tool row: `glyph presentation summary` (no duration/ellipsis).
pub fn compact_tool_line(
    status_glyph: &str,
    name: &str,
    arguments: &str,
    locale: Locale,
    summary_fn: impl FnOnce(&str, &str) -> String,
) -> String {
    let label = presentation_label(name, locale);
    let summary = summary_fn(name, arguments);
    if summary.is_empty() || summary == "{}" {
        format!("{status_glyph}  {label}")
    } else {
        format!("{status_glyph}  {label}  {summary}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn taxonomy_covers_full_registry_tools() {
        let reg = leveler_tools::full_registry();
        let names: Vec<String> = reg.definitions().into_iter().map(|d| d.name).collect();
        assert!(!names.is_empty());
        for name in &names {
            assert!(
                lookup(name).is_some(),
                "built-in tool `{name}` missing from TUI taxonomy"
            );
            let label_zh = presentation_label(name, Locale::Zh);
            let label_en = presentation_label(name, Locale::En);
            assert!(!label_zh.is_empty(), "{name}");
            assert!(!label_en.is_empty(), "{name}");
        }
    }

    #[test]
    fn taxonomy_entries_have_stable_non_empty_labels() {
        let mut seen = std::collections::BTreeSet::new();
        for entry in BUILTIN_TAXONOMY {
            assert!(
                seen.insert(entry.name),
                "duplicate taxonomy name {}",
                entry.name
            );
            assert!(!entry.presentation_en.is_empty());
            assert!(!entry.presentation_zh.is_empty());
        }
    }

    #[test]
    fn mcp_and_unknown_fallback() {
        assert_eq!(
            presentation_label("mcp__fs__read_file", Locale::En),
            "fs/read_file"
        );
        assert_eq!(
            presentation_label("custom_tool_x", Locale::En),
            "custom_tool_x"
        );
    }

    #[test]
    fn compact_line_includes_presentation_and_summary() {
        let line = compact_tool_line(
            "✓",
            "read_file",
            r#"{"path":"src/lib.rs"}"#,
            Locale::En,
            |_n, _a| "src/lib.rs".to_string(),
        );
        assert_eq!(line, "✓  Read file  src/lib.rs");
        let run = compact_tool_line(
            "●",
            "run_command",
            r#"{"program":"cargo"}"#,
            Locale::En,
            |_n, _a| "cargo test".to_string(),
        );
        assert_eq!(run, "●  Run command  cargo test");
        let edit = compact_tool_line("!", "apply_patch", "{}", Locale::Zh, |_n, _a| {
            "src/a.rs".to_string()
        });
        assert_eq!(edit, "!  编辑文件  src/a.rs");
    }

    #[test]
    fn visibility_hides_list_and_promotes_writes() {
        assert_eq!(
            activity_visibility("list_files", r#"{"path":"."}"#),
            ActivityVisibility::Silent
        );
        assert_eq!(
            activity_visibility("read_file", r#"{"path":"a.rs"}"#),
            ActivityVisibility::Normal
        );
        assert_eq!(
            activity_visibility(
                "apply_patch",
                r#"{"patch":"*** Begin Patch\n*** Update File: a.rs\n*** End Patch"}"#
            ),
            ActivityVisibility::Important
        );
        assert_eq!(
            activity_visibility("run_command", r#"{"program":"cargo","args":["test"]}"#),
            ActivityVisibility::Important
        );
        assert_eq!(
            activity_visibility("run_command", r#"{"program":"ls","args":["-la"]}"#),
            ActivityVisibility::Silent
        );
        assert_eq!(
            activity_visibility(
                "run_command",
                r#"{"program":"find","args":[".","-name","*.go"]}"#
            ),
            ActivityVisibility::Silent
        );
        assert_eq!(
            activity_visibility(
                "run_command",
                r#"{"program":"bash","args":["-c","ls internal/admin"]}"#
            ),
            ActivityVisibility::Silent
        );
        assert_eq!(
            activity_visibility("shell_command", r#"{"cmd":"ls -la"}"#),
            ActivityVisibility::Silent
        );
        assert_eq!(
            activity_visibility("shell_command", r#"{"cmd":"cargo test"}"#),
            ActivityVisibility::Important
        );
        // Goal complete is bookkeeping; blocked stays visible.
        assert_eq!(
            activity_visibility(
                "update_goal",
                r#"{"status":"complete","summary":"你好已回复"}"#
            ),
            ActivityVisibility::Silent
        );
        assert_eq!(
            activity_visibility("update_goal", r#"{"status":"blocked","summary":"缺密钥"}"#),
            ActivityVisibility::Important
        );
    }
}
