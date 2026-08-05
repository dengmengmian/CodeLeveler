//! Full-screen views layered over the conversation. Includes the
//! Tools screen; later phases add Plan/Diff/Verification/Sessions/etc. Esc
//! returns to the conversation.

use crate::i18n::UiText;
use crate::transcript::{ToolCallBlock, ToolStatus};

/// Which screen is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Screen {
    #[default]
    Conversation,
    Tools,
    Plan,
    Diff,
    Verification,
    Sessions,
    Context,
    Agents,
    /// The `/remote` invite: a QR to scan, and the fingerprint to compare.
    Remote,
    Help,
}

/// Help / popup grouping for slash commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashCategory {
    /// Model, permission, collab, goal — how the agent runs.
    Agent,
    /// Screens and inspection.
    View,
    /// History, export, memory, clear.
    Session,
    /// Attachments, skills, paste.
    Input,
    /// Theme, web, remote, help, quit.
    System,
}

/// Whether a command may run while a turn is busy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusyPolicy {
    /// Safe while busy (views, help, quit, most pickers).
    Always,
    /// Axes / destructive session ops — refuse with a notice while busy.
    IdleOnly,
}

/// One slash command: single source of truth for menu, aliases, and busy rules.
#[derive(Debug, Clone, Copy)]
pub struct SlashDef {
    /// Canonical name including leading `/` (shown in help and completed by Tab).
    pub name: &'static str,
    /// Alternate tokens including leading `/` (accepted, may be unlisted).
    pub aliases: &'static [&'static str],
    pub category: SlashCategory,
    /// Shown in help and the `/` popup when listed.
    pub listed: bool,
    pub busy: BusyPolicy,
}

/// Registry order = help order within each category and completion sort order.
pub const SLASH_DEFS: &[SlashDef] = &[
    // Agent
    SlashDef {
        name: "/model",
        aliases: &[],
        category: SlashCategory::Agent,
        listed: true,
        busy: BusyPolicy::Always,
    },
    // Primary name is permission; /mode kept as alias for muscle memory.
    SlashDef {
        name: "/permission",
        aliases: &["/mode"],
        category: SlashCategory::Agent,
        listed: true,
        busy: BusyPolicy::Always,
    },
    SlashDef {
        name: "/goal",
        aliases: &[],
        category: SlashCategory::Agent,
        listed: true,
        busy: BusyPolicy::Always, // steers when busy
    },
    SlashDef {
        name: "/btw",
        aliases: &[],
        category: SlashCategory::Agent,
        listed: true,
        busy: BusyPolicy::Always,
    },
    SlashDef {
        name: "/work-mode",
        aliases: &["/work_mode"],
        category: SlashCategory::Agent,
        listed: true,
        busy: BusyPolicy::IdleOnly,
    },
    SlashDef {
        name: "/collab",
        aliases: &[],
        category: SlashCategory::Agent,
        listed: true,
        busy: BusyPolicy::IdleOnly,
    },
    SlashDef {
        name: "/plan",
        aliases: &[],
        category: SlashCategory::Agent,
        listed: true,
        busy: BusyPolicy::IdleOnly,
    },
    // View
    SlashDef {
        name: "/diff",
        aliases: &[],
        category: SlashCategory::View,
        listed: true,
        busy: BusyPolicy::Always,
    },
    SlashDef {
        name: "/tools",
        aliases: &[],
        category: SlashCategory::View,
        listed: true,
        busy: BusyPolicy::Always,
    },
    SlashDef {
        name: "/sessions",
        aliases: &[],
        category: SlashCategory::View,
        listed: true,
        busy: BusyPolicy::Always,
    },
    // Session
    SlashDef {
        name: "/memory",
        aliases: &[],
        category: SlashCategory::Session,
        listed: true,
        busy: BusyPolicy::Always,
    },
    SlashDef {
        // `/rewind` is what every other agent CLI calls this, so it is accepted
        // as a first-class alias for muscle memory.
        name: "/restore",
        aliases: &["/checkpoint", "/rewind"],
        category: SlashCategory::Session,
        listed: true,
        busy: BusyPolicy::IdleOnly,
    },
    SlashDef {
        // Copying a transcript mid-turn would capture half a turn, so this
        // waits for idle like the other session-shaping commands.
        name: "/fork",
        aliases: &[],
        category: SlashCategory::Session,
        listed: true,
        busy: BusyPolicy::IdleOnly,
    },
    SlashDef {
        name: "/compact",
        aliases: &[],
        category: SlashCategory::Session,
        listed: true,
        busy: BusyPolicy::IdleOnly,
    },
    SlashDef {
        name: "/export",
        aliases: &["/save"],
        category: SlashCategory::Session,
        listed: true,
        busy: BusyPolicy::Always,
    },
    SlashDef {
        name: "/clear",
        aliases: &["/new"],
        category: SlashCategory::Session,
        listed: true,
        busy: BusyPolicy::IdleOnly,
    },
    // Input
    SlashDef {
        name: "/skill",
        aliases: &[],
        category: SlashCategory::Input,
        listed: true,
        busy: BusyPolicy::Always, // steers when busy
    },
    SlashDef {
        name: "/attach",
        aliases: &["/image"],
        category: SlashCategory::Input,
        listed: true,
        busy: BusyPolicy::Always,
    },
    SlashDef {
        name: "/paste",
        aliases: &[],
        category: SlashCategory::Input,
        listed: true,
        busy: BusyPolicy::Always,
    },
    SlashDef {
        name: "/editor",
        aliases: &[],
        category: SlashCategory::Input,
        listed: true,
        busy: BusyPolicy::Always,
    },
    // System
    SlashDef {
        name: "/theme",
        aliases: &[],
        category: SlashCategory::System,
        listed: true,
        busy: BusyPolicy::Always,
    },
    SlashDef {
        name: "/web",
        aliases: &[],
        category: SlashCategory::System,
        listed: true,
        busy: BusyPolicy::Always,
    },
    SlashDef {
        name: "/remote",
        aliases: &[],
        category: SlashCategory::System,
        listed: true,
        busy: BusyPolicy::Always,
    },
    // Same-network path: listed but copy marks it as local/testing.
    SlashDef {
        name: "/remote-loc",
        aliases: &[],
        category: SlashCategory::System,
        listed: true,
        busy: BusyPolicy::Always,
    },
    SlashDef {
        name: "/doctor",
        aliases: &[],
        category: SlashCategory::System,
        listed: true,
        busy: BusyPolicy::Always,
    },
    SlashDef {
        name: "/help",
        aliases: &[],
        category: SlashCategory::System,
        listed: true,
        busy: BusyPolicy::Always,
    },
    SlashDef {
        name: "/quit",
        aliases: &["/q"],
        category: SlashCategory::System,
        listed: true,
        busy: BusyPolicy::Always,
    },
];

/// All tokens (primary + aliases) that parse as local slash commands.
pub fn all_slash_tokens() -> Vec<&'static str> {
    let mut out = Vec::with_capacity(SLASH_DEFS.len() * 2);
    for d in SLASH_DEFS {
        out.push(d.name);
        out.extend_from_slice(d.aliases);
    }
    out
}

/// Stable listed names (primary only) — same role as the old `SLASH_NAMES`.
pub const SLASH_NAMES: &[&str] = &[
    "/model",
    "/permission",
    "/goal",
    "/btw",
    "/work-mode",
    "/collab",
    "/plan",
    "/diff",
    "/tools",
    "/sessions",
    "/memory",
    "/restore",
    "/fork",
    "/compact",
    "/export",
    "/clear",
    "/skill",
    "/attach",
    "/paste",
    "/editor",
    "/theme",
    "/web",
    "/remote",
    "/remote-loc",
    "/doctor",
    "/help",
    "/quit",
];

/// `name` is without the leading `/`. Empty string is `/` → help.
pub fn is_known_slash_token(name: &str) -> bool {
    if name.is_empty() {
        return true;
    }
    let token = format!("/{name}");
    SLASH_DEFS
        .iter()
        .any(|d| d.name == token.as_str() || d.aliases.contains(&token.as_str()))
}

/// Canonical primary name (with `/`) for a typed token (with or without `/`).
pub fn slash_primary(token: &str) -> Option<&'static str> {
    // Bare `/` is help.
    if token.is_empty() || token == "/" {
        return Some("/help");
    }
    let owned;
    let t = if token.starts_with('/') {
        token
    } else {
        owned = format!("/{token}");
        owned.as_str()
    };
    SLASH_DEFS
        .iter()
        .find(|d| d.name == t || d.aliases.contains(&t))
        .map(|d| d.name)
}

pub fn slash_def(token: &str) -> Option<&'static SlashDef> {
    let t = if token.starts_with('/') {
        token.to_string()
    } else {
        format!("/{token}")
    };
    SLASH_DEFS
        .iter()
        .find(|d| d.name == t.as_str() || d.aliases.contains(&t.as_str()))
}

fn slash_description(name: &str, t: &UiText) -> &'static str {
    let s = &t.slash;
    match name {
        "/model" => s.model,
        "/permission" | "/mode" => s.permission,
        "/goal" => s.goal,
        "/btw" => s.btw,
        "/work-mode" | "/work_mode" => s.work_mode,
        "/collab" => s.collab,
        "/plan" => s.plan_collab,
        "/memory" => s.memory,
        "/skill" => s.skill,
        "/diff" => s.diff,
        "/tools" => s.tools,
        "/sessions" => s.sessions,
        "/restore" | "/checkpoint" | "/rewind" => s.restore,
        "/fork" => s.fork,
        "/compact" => s.compact,
        "/export" | "/save" => s.export,
        "/web" => s.web,
        "/remote" => s.remote,
        "/remote-loc" => s.remote_loc,
        "/image" | "/attach" => s.attach,
        "/paste" => s.paste,
        "/editor" => s.editor,
        "/theme" => s.theme,
        "/clear" | "/new" => s.clear,
        "/doctor" => s.doctor,
        "/help" => s.help,
        "/quit" | "/q" => s.quit,
        _ => "",
    }
}

/// Localized listed slash commands for completion and the Help screen.
pub fn slash_commands(t: &UiText) -> Vec<(&'static str, &'static str)> {
    SLASH_DEFS
        .iter()
        .filter(|d| d.listed)
        .map(|d| (d.name, slash_description(d.name, t)))
        .collect()
}

/// Help entries grouped by category (category label key applied by renderer).
pub fn slash_commands_grouped(
    t: &UiText,
) -> Vec<(SlashCategory, Vec<(&'static str, &'static str)>)> {
    use SlashCategory::*;
    let order = [Agent, View, Session, Input, System];
    order
        .into_iter()
        .filter_map(|cat| {
            let items: Vec<_> = SLASH_DEFS
                .iter()
                .filter(|d| d.listed && d.category == cat)
                .map(|d| (d.name, slash_description(d.name, t)))
                .collect();
            if items.is_empty() {
                None
            } else {
                Some((cat, items))
            }
        })
        .collect()
}

pub fn category_label(cat: SlashCategory, t: &UiText) -> &'static str {
    match cat {
        SlashCategory::Agent => t.help_group_agent,
        SlashCategory::View => t.help_group_view,
        SlashCategory::Session => t.help_group_session,
        SlashCategory::Input => t.help_group_input,
        SlashCategory::System => t.help_group_system,
    }
}

/// Build a user message that names a skill for turn injection (same path as `$name`).
pub fn skill_mention_message(skill_name: &str, rest: &str) -> String {
    let name = skill_name.trim().trim_start_matches('$');
    let rest = rest.trim();
    if rest.is_empty() {
        format!("${name}")
    } else {
        format!("${name} {rest}")
    }
}

/// Slash commands whose primary name or alias starts with `prefix` (with `/`).
/// Rows always use the **primary** name so Tab completes the canonical form.
pub fn slash_matches(prefix: &str, t: &UiText) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for d in SLASH_DEFS.iter().filter(|d| d.listed) {
        let hit = d.name.starts_with(prefix) || d.aliases.iter().any(|a| a.starts_with(prefix));
        if hit {
            out.push((d.name.to_string(), slash_description(d.name, t).to_string()));
        }
    }
    out
}

/// Skills that may appear as `/name` (and do not collide with builtins).
pub fn skill_slash_matches(state: &crate::state::AppState, prefix: &str) -> Vec<(String, String)> {
    let prefix_name = prefix.trim_start_matches('/');
    state
        .skill_catalog
        .iter()
        .filter(|(name, _)| name.starts_with(prefix_name) || format!("/{name}").starts_with(prefix))
        .map(|(name, desc)| {
            let label = if desc.is_empty() {
                state.t().slash.skill_entry.to_string()
            } else {
                desc.clone()
            };
            (format!("/{name}"), label)
        })
        .collect()
}

/// The completion popup's entries for the current composer `text`, or empty when
/// no popup should show. Single source of truth shared by the reducer (key
/// handling / selection) and the renderer.
///
/// Includes discovered project/user skills as `/skill-name` so they feel like
/// first-class commands (Claude Code / Grok-style).
pub fn slash_popup(
    text: &str,
    t: &UiText,
    state: &crate::state::AppState,
) -> Vec<(String, String)> {
    if !text.starts_with('/') || text.contains('\n') {
        return Vec::new();
    }
    let token = text.split_whitespace().next().unwrap_or(text);
    let mut matches = slash_matches(token, t);
    matches.extend(skill_slash_matches(state, token));
    // Once a full command + argument is being typed, stop offering the popup.
    if text.contains(' ') && matches.len() <= 1 {
        return Vec::new();
    }
    matches
}

/// True when `token` (with leading `/`) is a primary name or alias.
pub fn is_exact_slash_token(token: &str) -> bool {
    all_slash_tokens().contains(&token)
}

/// Like [`slash_popup`], but respects a user Esc-dismiss so the menu stays
/// hidden until the composer text changes again.
pub fn visible_slash_popup(state: &crate::state::AppState) -> Vec<(String, String)> {
    if state.slash_popup_dismissed {
        return Vec::new();
    }
    slash_popup(state.composer.text(), state.t(), state)
}

/// Whether `name` (no leading `/`) is a skill that can be invoked as `/name`.
pub fn is_skill_slash_token(state: &crate::state::AppState, name: &str) -> bool {
    !is_known_slash_token(name)
        && state
            .skill_catalog
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case(name))
}

/// Active `@file` query immediately before the composer cursor.
pub fn file_mention_query(state: &crate::state::AppState) -> Option<&str> {
    let token = state
        .composer
        .text_before_cursor()
        .split_whitespace()
        .next_back()?;
    token.strip_prefix('@')
}

/// Filter repository paths for the active `@file` mention.
pub fn visible_file_popup(state: &crate::state::AppState) -> Vec<&str> {
    if state.slash_popup_dismissed {
        return Vec::new();
    }
    let Some(query) = file_mention_query(state) else {
        return Vec::new();
    };
    let query = query.to_ascii_lowercase();
    let source = if state.file_candidates.is_empty() {
        &state.context_files
    } else {
        &state.file_candidates
    };
    let mut matches: Vec<&str> = source
        .iter()
        .map(String::as_str)
        .filter(|path| path.to_ascii_lowercase().contains(&query))
        .collect();
    matches.sort_by_key(|path| {
        let lower = path.to_ascii_lowercase();
        (!lower.starts_with(&query), lower.len())
    });
    matches.truncate(50);
    matches
}

/// Ghost argument hint drawn after the cursor (not inserted into the buffer).
/// Shown only when the composer is exactly a known command that needs an
/// argument (optional trailing spaces). The caret must also be at end of input
/// (checked by the renderer).
///
/// Returns the full ghost string including a leading space when the buffer has
/// none yet (`/btw` → ` <问题>`), or just the placeholder when a space is
/// already present (`/btw ` → `<问题>`).
pub fn slash_arg_ghost(text: &str, t: &UiText) -> Option<&'static str> {
    if text.is_empty() || !text.starts_with('/') || text.contains('\n') {
        return None;
    }
    // Any non-space content after the command name means the user started the arg.
    let mut parts = text.splitn(2, char::is_whitespace);
    let cmd = parts.next()?;
    let rest = parts.next().unwrap_or("");
    if !rest.trim().is_empty() {
        return None;
    }
    // Accept primary or alias; ghost uses the canonical primary.
    let primary = slash_primary(cmd)?;
    let g = &t.slash_ghost;
    let (bare, spaced) = match primary {
        "/btw" => (g.btw, g.btw_spaced),
        "/goal" => (g.goal, g.goal_spaced),
        "/skill" => (g.skill, g.skill_spaced),
        "/attach" => (g.path, g.path_spaced),
        "/work-mode" => (g.work_mode, g.work_mode_spaced),
        "/collab" => (g.collab, g.collab_spaced),
        _ => return None,
    };
    if text.ends_with(|c: char| c.is_whitespace()) {
        Some(bare)
    } else {
        Some(spaced)
    }
}

/// Filters for the Tools screen .
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolFilter {
    #[default]
    All,
    Read,
    Write,
    Shell,
    Failed,
}

#[cfg(test)]
mod ghost_tests {
    use super::{skill_mention_message, slash_arg_ghost, slash_commands};
    use crate::i18n::Locale;

    fn t() -> &'static crate::i18n::UiText {
        Locale::Zh.text()
    }

    #[test]
    fn skill_is_in_slash_commands() {
        let names: Vec<_> = slash_commands(t()).into_iter().map(|(n, _)| n).collect();
        assert!(names.contains(&"/skill"), "{names:?}");
    }

    #[test]
    fn skill_mention_message_matches_dollar_form() {
        assert_eq!(skill_mention_message("demo", ""), "$demo");
        assert_eq!(
            skill_mention_message("$demo", "please ship"),
            "$demo please ship"
        );
        assert_eq!(skill_mention_message(" deploy ", "  x  "), "$deploy x");
    }

    #[test]
    fn ghost_for_btw_and_goal_and_path_commands() {
        let zh = t();
        assert_eq!(slash_arg_ghost("/btw", zh), Some(" <问题>"));
        assert_eq!(slash_arg_ghost("/btw ", zh), Some("<问题>"));
        assert_eq!(
            slash_arg_ghost("/goal", zh),
            Some(" <任务目标> | status | clear")
        );
        assert_eq!(slash_arg_ghost("/image ", zh), Some("<文件路径>"));
        assert_eq!(slash_arg_ghost("/attach", zh), Some(" <文件路径>"));
        assert_eq!(
            slash_arg_ghost("/work-mode", zh),
            Some(" economy|balanced|delivery")
        );
        assert_eq!(slash_arg_ghost("/collab ", zh), Some("chat|plan|goal"));
    }

    #[test]
    fn no_ghost_once_argument_started_or_unknown() {
        let zh = t();
        assert_eq!(slash_arg_ghost("/btw 你好", zh), None);
        assert_eq!(slash_arg_ghost("/bt", zh), None);
        assert_eq!(slash_arg_ghost("/collab chat", zh), None);
        assert_eq!(slash_arg_ghost("hello", zh), None);
        assert_eq!(slash_arg_ghost("", zh), None);
    }

    #[test]
    fn permission_is_primary_mode_is_alias() {
        assert_eq!(super::slash_primary("/mode"), Some("/permission"));
        assert_eq!(super::slash_primary("permission"), Some("/permission"));
        assert!(super::is_known_slash_token("mode"));
        assert!(super::is_known_slash_token("permission"));
        let names: Vec<_> = slash_commands(t()).into_iter().map(|(n, _)| n).collect();
        assert!(names.contains(&"/permission"));
        assert!(
            !names.contains(&"/mode"),
            "alias must not double-list: {names:?}"
        );
        assert!(
            !names.contains(&"/image"),
            "image is attach alias: {names:?}"
        );
    }
}

impl ToolFilter {
    /// Cycle to the next filter (Tab on the Tools screen).
    pub fn next(self) -> Self {
        match self {
            ToolFilter::All => ToolFilter::Read,
            ToolFilter::Read => ToolFilter::Write,
            ToolFilter::Write => ToolFilter::Shell,
            ToolFilter::Shell => ToolFilter::Failed,
            ToolFilter::Failed => ToolFilter::All,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ToolFilter::All => "全部",
            ToolFilter::Read => "读取",
            ToolFilter::Write => "写入",
            ToolFilter::Shell => "Shell",
            ToolFilter::Failed => "需调整",
        }
    }

    /// Whether a tool block passes this filter.
    pub fn matches(self, block: &ToolCallBlock) -> bool {
        match self {
            ToolFilter::All => true,
            ToolFilter::Failed => block.status == ToolStatus::Failed,
            ToolFilter::Read => tool_category(&block.name) == Category::Read,
            ToolFilter::Write => tool_category(&block.name) == Category::Write,
            ToolFilter::Shell => tool_category(&block.name) == Category::Shell,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Category {
    Read,
    Write,
    Shell,
    Other,
}

/// Classify a tool by name into a coarse category for filtering.
fn tool_category(name: &str) -> Category {
    let n = name.to_lowercase();
    if n == "run_command" {
        Category::Shell
    } else if ["read", "grep", "search", "find", "list", "symbol"]
        .iter()
        .any(|k| n.contains(k))
    {
        Category::Read
    } else if ["write", "patch", "edit", "apply", "create", "delete"]
        .iter()
        .any(|k| n.contains(k))
    {
        Category::Write
    } else {
        Category::Other
    }
}

/// Tools screen navigation state: which row is selected and the active filter.
#[derive(Debug, Default, Clone)]
pub struct ToolsScreenState {
    pub selected: usize,
    pub filter: ToolFilter,
}

impl ToolsScreenState {
    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn move_down(&mut self, len: usize) {
        if len > 0 && self.selected + 1 < len {
            self.selected += 1;
        }
    }

    pub fn cycle_filter(&mut self) {
        self.filter = self.filter.next();
        self.selected = 0;
    }
}
