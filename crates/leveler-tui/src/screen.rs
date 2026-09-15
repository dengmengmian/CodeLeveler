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
    /// One user shell execution (`!command`): status, runtime, command,
    /// live output; Esc backs out, `x` stops a running one.
    Shell,
    /// First-class Activity Detail (background task or child agent).
    /// Observational: Esc closes the viewer without cancelling the work.
    Activity,
    Help,
    /// Durable runtime observatory (`/trace`).
    Trace,
}

/// Help / popup grouping for slash commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashCategory {
    /// Model, permission, collab, goal — how the agent runs.
    Agent,
    /// Screens and inspection.
    View,
    /// History, export, memory, new.
    Session,
    /// Attachments.
    Input,
    /// Theme, web, remote, help.
    System,
}

/// Whether a command may run while a turn is busy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusyPolicy {
    /// Safe while busy (views, help, most pickers).
    Always,
    /// Axes / destructive session ops — refuse with a notice while busy.
    IdleOnly,
}

/// How a command appears in the `/` popup. Help still uses [`SlashDef::listed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashVisibility {
    /// Shown on a bare `/`.
    Quick,
    /// Hidden until the user types a matching prefix.
    Searchable,
    /// Never listed, never searchable.
    Internal,
}

/// One slash command: single source of truth for menu, aliases, and busy rules.
#[derive(Debug, Clone, Copy)]
pub struct SlashDef {
    /// Canonical name including leading `/` (shown in help and completed by Tab).
    pub name: &'static str,
    /// Alternate tokens including leading `/` (accepted, may be unlisted).
    pub aliases: &'static [&'static str],
    pub category: SlashCategory,
    /// Shown in `/help`. Independent of popup visibility.
    pub listed: bool,
    /// Empty-`/` vs prefix-search vs never.
    pub visibility: SlashVisibility,
    pub busy: BusyPolicy,
}

const fn slash(
    name: &'static str,
    aliases: &'static [&'static str],
    category: SlashCategory,
    visibility: SlashVisibility,
    busy: BusyPolicy,
) -> SlashDef {
    SlashDef {
        name,
        aliases,
        category,
        listed: !matches!(visibility, SlashVisibility::Internal),
        visibility,
        busy,
    }
}

/// Registry order = help order within each category and completion sort order.
pub const SLASH_DEFS: &[SlashDef] = &[
    // Agent
    slash(
        "/model",
        &[],
        SlashCategory::Agent,
        SlashVisibility::Quick,
        BusyPolicy::Always,
    ),
    // Primary name is permission; /mode kept as alias for muscle memory.
    slash(
        "/permission",
        &["/mode"],
        SlashCategory::Agent,
        SlashVisibility::Quick,
        BusyPolicy::Always,
    ),
    slash(
        "/goal",
        &[],
        SlashCategory::Agent,
        SlashVisibility::Quick,
        BusyPolicy::Always,
    ),
    slash(
        "/btw",
        &[],
        SlashCategory::Agent,
        SlashVisibility::Quick,
        BusyPolicy::Always,
    ),
    // Long-goal P3: cut a durable goal checkpoint and show it as a Recap.
    slash(
        "/recap",
        &[],
        SlashCategory::Agent,
        SlashVisibility::Quick,
        BusyPolicy::Always,
    ),
    slash(
        "/work-mode",
        &["/work_mode"],
        SlashCategory::Agent,
        SlashVisibility::Searchable,
        BusyPolicy::IdleOnly,
    ),
    slash(
        "/collab",
        &[],
        SlashCategory::Agent,
        SlashVisibility::Quick,
        BusyPolicy::IdleOnly,
    ),
    // View
    slash(
        "/diff",
        &[],
        SlashCategory::View,
        SlashVisibility::Quick,
        BusyPolicy::Always,
    ),
    slash(
        "/trace",
        &[],
        SlashCategory::View,
        SlashVisibility::Searchable,
        BusyPolicy::Always,
    ),
    slash(
        "/sessions",
        &[],
        SlashCategory::View,
        SlashVisibility::Searchable,
        BusyPolicy::Always,
    ),
    // Session
    slash(
        "/memory",
        &[],
        SlashCategory::Session,
        SlashVisibility::Searchable,
        BusyPolicy::Always,
    ),
    // The user's own direct write. `BusyPolicy::Always` because it touches no
    // turn state: it is a control-plane command, not a message to the model.
    slash(
        "/remember",
        &[],
        SlashCategory::Session,
        SlashVisibility::Searchable,
        BusyPolicy::Always,
    ),
    // `/rewind` is what every other agent CLI calls this.
    slash(
        "/restore",
        &["/checkpoint", "/rewind"],
        SlashCategory::Session,
        SlashVisibility::Searchable,
        BusyPolicy::IdleOnly,
    ),
    slash(
        "/fork",
        &[],
        SlashCategory::Session,
        SlashVisibility::Searchable,
        BusyPolicy::IdleOnly,
    ),
    slash(
        "/compact",
        &[],
        SlashCategory::Session,
        SlashVisibility::Quick,
        BusyPolicy::IdleOnly,
    ),
    slash(
        "/export",
        &["/save"],
        SlashCategory::Session,
        SlashVisibility::Searchable,
        BusyPolicy::Always,
    ),
    slash(
        "/new",
        &[],
        SlashCategory::Session,
        SlashVisibility::Quick,
        BusyPolicy::IdleOnly,
    ),
    // Input
    slash(
        "/attach",
        &["/image"],
        SlashCategory::Input,
        SlashVisibility::Searchable,
        BusyPolicy::Always,
    ),
    // System
    slash(
        "/theme",
        &[],
        SlashCategory::System,
        SlashVisibility::Searchable,
        BusyPolicy::Always,
    ),
    slash(
        "/web",
        &[],
        SlashCategory::System,
        SlashVisibility::Searchable,
        BusyPolicy::Always,
    ),
    slash(
        "/remote",
        &[],
        SlashCategory::System,
        SlashVisibility::Searchable,
        BusyPolicy::Always,
    ),
    slash(
        "/remote-loc",
        &[],
        SlashCategory::System,
        SlashVisibility::Searchable,
        BusyPolicy::Always,
    ),
    slash(
        "/help",
        &[],
        SlashCategory::System,
        SlashVisibility::Quick,
        BusyPolicy::Always,
    ),
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
    slash_copy(name, &t.slash)
}

/// One-line palette label. Help keeps the longer [`slash_description`].
pub fn slash_popup_label(name: &str, t: &UiText) -> &'static str {
    slash_copy(name, &t.slash_brief)
}

fn slash_copy(name: &str, s: &crate::i18n::SlashText) -> &'static str {
    match name {
        "/model" => s.model,
        "/permission" | "/mode" => s.permission,
        "/goal" => s.goal,
        "/btw" => s.btw,
        "/recap" => s.recap,
        "/work-mode" | "/work_mode" => s.work_mode,
        "/collab" => s.collab,
        "/memory" => s.memory,
        "/remember" => s.remember,
        "/diff" => s.diff,
        "/trace" => s.trace,
        "/sessions" => s.sessions,
        "/restore" | "/checkpoint" | "/rewind" => s.restore,
        "/fork" => s.fork,
        "/compact" => s.compact,
        "/export" | "/save" => s.export,
        "/web" => s.web,
        "/remote" => s.remote,
        "/remote-loc" => s.remote_loc,
        "/image" | "/attach" => s.attach,
        "/theme" => s.theme,
        "/new" => s.new,
        "/help" => s.help,
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

/// Whether `def` belongs in the `/` popup for this typed `prefix`.
pub(crate) fn include_in_slash_popup(def: &SlashDef, prefix: &str) -> bool {
    match def.visibility {
        SlashVisibility::Internal => false,
        SlashVisibility::Quick => {
            def.name.starts_with(prefix) || def.aliases.iter().any(|a| a.starts_with(prefix))
        }
        SlashVisibility::Searchable => {
            prefix != "/"
                && (def.name.starts_with(prefix)
                    || def.aliases.iter().any(|a| a.starts_with(prefix)))
        }
    }
}

/// Slash commands whose primary name or alias starts with `prefix` (with `/`).
/// Rows always use the **primary** name so Tab completes the canonical form.
/// A bare `/` is the Quick list only; a longer prefix also searches the rest.
pub fn slash_matches(prefix: &str, t: &UiText) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for d in SLASH_DEFS {
        if include_in_slash_popup(d, prefix) {
            out.push((d.name.to_string(), slash_popup_label(d.name, t).to_string()));
        }
    }
    out
}

/// The completion popup's entries for the current composer `text`, or empty when
/// no popup should show. Single source of truth shared by the reducer (key
/// handling / selection) and the renderer. Skills are not here: they are the
/// `$` namespace ([`visible_skill_popup`]).
pub fn slash_popup(text: &str, t: &UiText) -> Vec<(String, String)> {
    if !text.starts_with('/') || text.contains('\n') {
        return Vec::new();
    }
    let token = text.split_whitespace().next().unwrap_or(text);
    let matches = slash_matches(token, t);
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
    slash_popup(state.composer.text(), state.t())
}

/// The whitespace-delimited token immediately before the composer cursor,
/// stripped of `prefix`, while that token is still being typed. This is the
/// single completion-intent truth shared by `$skill` and `@file` mentions:
/// trailing whitespace means the token is finished (e.g. just completed), so
/// the cursor is no longer inside a mention and this returns `None`.
fn active_prefixed_token_before_cursor(
    state: &crate::state::AppState,
    prefix: char,
) -> Option<&str> {
    let before = state.composer.text_before_cursor();
    if before.ends_with(char::is_whitespace) {
        return None;
    }
    before.split_whitespace().next_back()?.strip_prefix(prefix)
}

/// Active `$skill` query immediately before the composer cursor.
pub fn skill_mention_query(state: &crate::state::AppState) -> Option<&str> {
    active_prefixed_token_before_cursor(state, '$')
}

/// Skills matching the active `$` mention, as `($name, label)` rows. The
/// runtime injects a skill for any `$name` in the message, so completion is
/// the whole interaction — there is no separate command.
pub fn visible_skill_popup(state: &crate::state::AppState) -> Vec<(String, String)> {
    // In a `!command` the `$` belongs to the shell.
    if state.slash_popup_dismissed || state.composer.text().starts_with('!') {
        return Vec::new();
    }
    let Some(query) = skill_mention_query(state) else {
        return Vec::new();
    };
    let t = state.t();
    state
        .skill_catalog
        .iter()
        .filter(|(name, _)| name.starts_with(query))
        .map(|(name, desc)| {
            let label = if name == "feature-dev" {
                // The builtin SKILL.md description is the workflow; the popup
                // only says what the skill is for, in the UI's language.
                t.skill_feature_dev
            } else if desc.is_empty() {
                t.skill_mention
            } else {
                desc
            };
            (format!("${name}"), label.to_string())
        })
        .collect()
}

/// Active `@file` query immediately before the composer cursor.
pub fn file_mention_query(state: &crate::state::AppState) -> Option<&str> {
    active_prefixed_token_before_cursor(state, '@')
}

/// Filter repository paths for the active `@file` mention.
pub fn visible_file_popup(state: &crate::state::AppState) -> Vec<&str> {
    // In a `!command` the `@` belongs to the shell (same rule as `$`).
    if state.slash_popup_dismissed || state.composer.text().starts_with('!') {
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
        "/attach" => (g.path, g.path_spaced),
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
mod surface_tests {
    use super::{SLASH_DEFS, all_slash_tokens};

    /// The closed TUI command surface. Adding or removing a command changes
    /// this list on purpose, never as a side effect.
    #[test]
    fn registry_is_the_closed_command_surface() {
        let names: Vec<_> = SLASH_DEFS.iter().map(|d| d.name).collect();
        assert_eq!(
            names,
            [
                "/model",
                "/permission",
                "/goal",
                "/btw",
                "/recap",
                "/work-mode",
                "/collab",
                "/diff",
                "/trace",
                "/sessions",
                "/memory",
                "/remember",
                "/restore",
                "/fork",
                "/compact",
                "/export",
                "/new",
                "/attach",
                "/theme",
                "/web",
                "/remote",
                "/remote-loc",
                "/help",
            ]
        );
    }

    #[test]
    fn every_token_names_exactly_one_command() {
        let tokens = all_slash_tokens();
        for token in &tokens {
            assert!(token.starts_with('/'), "{token}");
            assert_eq!(
                tokens.iter().filter(|t| *t == token).count(),
                1,
                "{token} is registered twice"
            );
        }
    }

    /// Source text of the item starting at `signature`, up to its closing
    /// top-level brace.
    fn item_body<'a>(src: &'a str, signature: &str) -> &'a str {
        let start = src
            .find(signature)
            .unwrap_or_else(|| panic!("{signature} not found"));
        let rest = &src[start..];
        &rest[..rest.find("\n}\n").expect("item end")]
    }

    /// String literals that open a match arm: `"a" | "b" =>`.
    fn arm_literals(body: &str) -> Vec<&str> {
        body.lines()
            .map(str::trim_start)
            .filter(|l| l.starts_with('"') && l.contains("=>"))
            .flat_map(|l| {
                l.split("=>")
                    .next()
                    .unwrap_or("")
                    .split('|')
                    .map(|p| p.trim().trim_matches('"'))
            })
            .collect()
    }

    fn struct_fields<'a>(src: &'a str, name: &str) -> Vec<&'a str> {
        item_body(src, &format!("pub struct {name} {{"))
            .lines()
            .skip(1)
            .filter_map(|l| l.trim().strip_prefix("pub "))
            .map(|l| l.split(':').next().unwrap_or("").trim())
            .collect()
    }

    /// Registry ↔ copy ↔ dispatch. Copy or a dispatch arm for a command that
    /// is not registered is dead residue (the old `/feature-dev` copy); a
    /// registered command without both is a broken promise.
    #[test]
    fn copy_and_dispatch_cover_exactly_the_registry() {
        let tokens = all_slash_tokens();
        let copy = arm_literals(item_body(include_str!("screen.rs"), "fn slash_copy("));
        for lit in &copy {
            assert!(tokens.contains(lit), "copy for unregistered {lit}");
        }
        for token in &tokens {
            assert!(copy.contains(token), "{token} has no copy");
        }

        let dispatch = arm_literals(item_body(
            include_str!("reducer/submit.rs"),
            "fn handle_slash(",
        ));
        let primaries: Vec<_> = SLASH_DEFS
            .iter()
            .map(|d| d.name.trim_start_matches('/'))
            .collect();
        for lit in &dispatch {
            assert!(primaries.contains(lit), "dispatch for unregistered /{lit}");
        }
        for primary in &primaries {
            assert!(dispatch.contains(primary), "/{primary} is never dispatched");
        }
    }

    /// Every command-copy string is reachable from a command.
    #[test]
    fn every_slash_copy_field_belongs_to_a_command() {
        let i18n = include_str!("i18n.rs");
        let screen = include_str!("screen.rs");
        let copy = item_body(screen, "fn slash_copy(");
        for field in struct_fields(i18n, "SlashText") {
            assert!(
                copy.contains(&format!("s.{field},")),
                "SlashText.{field} is copy for no command"
            );
        }
        let ghost = item_body(screen, "pub fn slash_arg_ghost(");
        for field in struct_fields(i18n, "SlashGhost") {
            assert!(
                ghost.contains(&format!("g.{field},")) || ghost.contains(&format!("g.{field})")),
                "SlashGhost.{field} is never drawn"
            );
        }
    }
}

#[cfg(test)]
mod ghost_tests {
    use super::{slash_arg_ghost, slash_commands};
    use crate::i18n::Locale;

    fn t() -> &'static crate::i18n::UiText {
        Locale::Zh.text()
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
            None,
            "option commands use a picker, not a CLI ghost"
        );
        assert_eq!(slash_arg_ghost("/collab ", zh), None);
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
    fn popup_labels_are_brief_help_stays_long() {
        let t = t();
        assert_eq!(super::slash_popup_label("/model", t), "切换模型");
        assert_eq!(super::slash_popup_label("/goal", t), "设置目标");
        assert_eq!(super::slash_popup_label("/permission", t), "权限审批");
        let help: Vec<_> = slash_commands(t);
        let goal_help = help
            .iter()
            .find(|(n, _)| *n == "/goal")
            .map(|(_, d)| *d)
            .unwrap();
        assert!(
            goal_help.contains("status") || goal_help.contains("任务"),
            "help copy must stay detailed: {goal_help}"
        );
        let matches = super::slash_matches("/", t);
        let model = matches.iter().find(|(n, _)| n == "/model").unwrap();
        assert_eq!(model.1, "切换模型");
    }

    #[test]
    fn empty_slash_lists_only_quick_commands() {
        let rows = super::slash_matches("/", t());
        let names: Vec<_> = rows.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            [
                "/model",
                "/permission",
                "/goal",
                "/btw",
                "/recap",
                "/collab",
                "/diff",
                "/compact",
                "/new",
                "/help",
            ],
            "empty / is the high-frequency core only"
        );
        for hidden in [
            "/work-mode",
            "/trace",
            "/sessions",
            "/memory",
            "/remember",
            "/restore",
            "/fork",
            "/export",
            "/attach",
            "/theme",
            "/web",
            "/remote",
            "/remote-loc",
        ] {
            assert!(!names.contains(&hidden), "searchable leaked on /: {hidden}");
        }
        assert!(
            rows.iter()
                .all(|(_, d)| !d.contains('|') && !d.contains("economy")),
            "popup must not list CLI args: {rows:?}"
        );
    }

    #[test]
    fn prefix_search_surfaces_hidden_searchable_commands() {
        let rows = super::slash_matches("/ses", t());
        assert!(
            rows.iter().any(|(n, _)| n == "/sessions"),
            "/ses must find /sessions: {rows:?}"
        );
        let theme = super::slash_matches("/the", t());
        assert!(
            theme.iter().any(|(n, _)| n == "/theme"),
            "/the must find /theme: {theme:?}"
        );
    }

    #[test]
    fn internal_visibility_never_appears_in_popup() {
        let hidden = super::SlashDef {
            name: "/debug",
            aliases: &[],
            category: super::SlashCategory::System,
            listed: false,
            visibility: super::SlashVisibility::Internal,
            busy: super::BusyPolicy::Always,
        };
        assert!(!super::include_in_slash_popup(&hidden, "/"));
        assert!(!super::include_in_slash_popup(&hidden, "/deb"));
        assert!(!super::include_in_slash_popup(&hidden, "/debug"));
    }

    fn skill_state(locale: Locale, skills: &[(&str, &str)]) -> crate::state::AppState {
        use crate::state::{AppState, Boot};
        use crate::theme::Theme;
        use leveler_client_protocol::SessionId;

        let mut s = AppState::new(
            Theme::no_color(),
            Boot {
                session_id: SessionId::new("vis"),
                user: "u".into(),
                version: "0".into(),
                show_welcome: false,
                draft_path: None,
                history_path: None,
                context_window: 0,
                locale,
                untrusted_config: Vec::new(),
                reasoning_effort: None,
            },
        );
        s.skill_catalog = skills
            .iter()
            .map(|(n, d)| (n.to_string(), d.to_string()))
            .collect();
        s
    }

    #[test]
    fn slash_popup_never_lists_skills() {
        let mut s = skill_state(
            Locale::Zh,
            &[("feature-dev", "分阶段实现"), ("repo-lint", "检查")],
        );
        for text in ["/", "/feat", "/repo"] {
            s.composer.replace(text);
            let rows = super::visible_slash_popup(&s);
            assert!(
                rows.iter()
                    .all(|(n, _)| n != "/feature-dev" && n != "/repo-lint"),
                "{text}: {rows:?}"
            );
        }
    }

    #[test]
    fn bare_dollar_lists_every_skill() {
        let mut s = skill_state(
            Locale::Zh,
            &[("feature-dev", "分阶段实现"), ("repo-lint", "本仓库检查")],
        );
        s.composer.replace("$");
        let names: Vec<_> = super::visible_skill_popup(&s)
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(names, ["$feature-dev", "$repo-lint"]);
        s.composer.replace("run $repo");
        let names: Vec<_> = super::visible_skill_popup(&s)
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(names, ["$repo-lint"]);
    }

    #[test]
    fn builtin_skill_label_is_a_capability_not_a_workflow() {
        let long = "分阶段实现一个新功能：先摸清现状和需求";
        let mut zh = skill_state(
            Locale::Zh,
            &[("feature-dev", long), ("repo-lint", "本仓库的检查脚本")],
        );
        zh.composer.replace("$");
        let rows = super::visible_skill_popup(&zh);
        assert_eq!(
            rows,
            [
                ("$feature-dev".to_string(), "功能实现".to_string()),
                ("$repo-lint".to_string(), "本仓库的检查脚本".to_string()),
            ]
        );
        let mut en = skill_state(Locale::En, &[("feature-dev", long), ("bare", "")]);
        en.composer.replace("$");
        let rows = super::visible_skill_popup(&en);
        assert_eq!(
            rows,
            [
                (
                    "$feature-dev".to_string(),
                    "Implement a feature".to_string()
                ),
                ("$bare".to_string(), "skill".to_string()),
            ]
        );
    }

    #[test]
    fn every_listed_slash_has_zh_and_en_popup_and_help() {
        for d in super::SLASH_DEFS.iter().filter(|d| d.listed) {
            let zh_brief = super::slash_popup_label(d.name, Locale::Zh.text());
            let en_brief = super::slash_popup_label(d.name, Locale::En.text());
            let zh_help = super::slash_description(d.name, Locale::Zh.text());
            let en_help = super::slash_description(d.name, Locale::En.text());
            assert!(
                !zh_brief.is_empty() && !en_brief.is_empty(),
                "{} missing popup copy zh={zh_brief:?} en={en_brief:?}",
                d.name
            );
            assert!(
                !zh_help.is_empty() && !en_help.is_empty(),
                "{} missing help copy zh={zh_help:?} en={en_help:?}",
                d.name
            );
            assert!(
                !zh_brief.contains("分阶段")
                    && !en_brief.to_ascii_lowercase().contains("step-by-step"),
                "{} popup still has workflow internals: zh={zh_brief:?} en={en_brief:?}",
                d.name
            );
        }
    }

    #[test]
    fn popup_briefs_match_command_capability() {
        let zh = Locale::Zh.text();
        let en = Locale::En.text();
        let pairs = [
            ("/model", "切换模型", "Switch model"),
            ("/permission", "权限审批", "permission approval"),
            ("/goal", "设置目标", "set goal"),
            ("/btw", "临时提问", "quick question"),
            ("/work-mode", "执行策略", "work strategy"),
            ("/collab", "协作模式", "collaboration mode"),
            ("/diff", "查看改动", "view changes"),
        ];
        for (name, zh_want, en_want) in pairs {
            assert_eq!(super::slash_popup_label(name, zh), zh_want, "{name} zh");
            assert_eq!(super::slash_popup_label(name, en), en_want, "{name} en");
        }
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
