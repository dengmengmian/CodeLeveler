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
    Help,
}

/// Slash command names in stable order (descriptions come from [`UiText`]).
pub const SLASH_NAMES: &[&str] = &[
    "/model",
    "/mode",
    "/goal",
    "/btw",
    "/workflow",
    "/work-mode",
    "/collab",
    "/plan",
    "/confirm-plan",
    "/memory",
    "/skill",
    "/steps",
    "/diff",
    "/verify",
    "/tools",
    "/sessions",
    "/context",
    "/agents",
    "/restore",
    "/compact",
    "/image",
    "/attach",
    "/paste",
    "/theme",
    "/clear",
    "/help",
    "/quit",
];

/// Localized slash commands for completion and the Help screen (spec §28).
pub fn slash_commands(t: &UiText) -> Vec<(&'static str, &'static str)> {
    let s = &t.slash;
    vec![
        ("/model", s.model),
        ("/mode", s.mode),
        ("/goal", s.goal),
        ("/btw", s.btw),
        ("/workflow", s.workflow),
        ("/work-mode", s.work_mode),
        ("/collab", s.collab),
        ("/plan", s.plan_collab),
        ("/confirm-plan", s.confirm_plan),
        ("/memory", s.memory),
        ("/skill", s.skill),
        ("/steps", s.steps),
        ("/diff", s.diff),
        ("/verify", s.verify),
        ("/tools", s.tools),
        ("/sessions", s.sessions),
        ("/context", s.context),
        ("/agents", s.agents),
        ("/restore", s.restore),
        ("/compact", s.compact),
        ("/image", s.image),
        ("/attach", s.attach),
        ("/paste", s.paste),
        ("/theme", s.theme),
        ("/clear", s.clear),
        ("/help", s.help),
        ("/quit", s.quit),
    ]
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

/// Slash commands whose name starts with `prefix` (including the leading `/`).
pub fn slash_matches(prefix: &str, t: &UiText) -> Vec<(&'static str, &'static str)> {
    slash_commands(t)
        .into_iter()
        .filter(|(name, _)| name.starts_with(prefix))
        .collect()
}

/// The completion popup's entries for the current composer `text`, or empty when
/// no popup should show. Single source of truth shared by the reducer (key
/// handling / selection) and the renderer.
pub fn slash_popup(text: &str, t: &UiText) -> Vec<(&'static str, &'static str)> {
    if !text.starts_with('/') || text.contains('\n') {
        return Vec::new();
    }
    let token = text.split_whitespace().next().unwrap_or(text);
    // Once a full command + argument is being typed, stop offering the popup.
    if text.contains(' ') && slash_matches(token, t).len() <= 1 {
        return Vec::new();
    }
    slash_matches(token, t)
}

/// Like [`slash_popup`], but respects a user Esc-dismiss so the menu stays
/// hidden until the composer text changes again.
pub fn visible_slash_popup(state: &crate::state::AppState) -> Vec<(&'static str, &'static str)> {
    if state.slash_popup_dismissed {
        return Vec::new();
    }
    slash_popup(state.composer.text(), state.t())
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
    // Partial prefixes (`/bt`) keep the popup; only full command names get a ghost.
    if !SLASH_NAMES.contains(&cmd) {
        return None;
    }
    let g = &t.slash_ghost;
    let (bare, spaced) = match cmd {
        "/btw" => (g.btw, g.btw_spaced),
        "/goal" => (g.goal, g.goal_spaced),
        "/skill" => (g.skill, g.skill_spaced),
        "/image" | "/attach" => (g.path, g.path_spaced),
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
        assert_eq!(slash_arg_ghost("/goal", zh), Some(" <任务目标>"));
        assert_eq!(slash_arg_ghost("/image ", zh), Some("<文件路径>"));
        assert_eq!(slash_arg_ghost("/attach", zh), Some(" <文件路径>"));
    }

    #[test]
    fn no_ghost_once_argument_started_or_unknown() {
        let zh = t();
        assert_eq!(slash_arg_ghost("/btw 你好", zh), None);
        assert_eq!(slash_arg_ghost("/bt", zh), None);
        assert_eq!(slash_arg_ghost("/workflow", zh), None);
        assert_eq!(slash_arg_ghost("hello", zh), None);
        assert_eq!(slash_arg_ghost("", zh), None);
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
