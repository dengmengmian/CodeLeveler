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
    Diff,
    Sessions,
    /// `/resume-claude` / `/resume-codex`: the locally stored sessions of one
    /// other coding agent. Enter continues the selected source's work in a NEW
    /// native session; the page never resumes that agent's runtime.
    ForeignSessions,
    /// First-class Activity Detail (background task or child agent).
    /// Observational: Esc closes the viewer without cancelling the work.
    Activity,
    /// The background-jobs list: running and recently finished tasks. Opened
    /// from the state strip's background cell. Enter moves between the list and
    /// the selected task's live log — the page never leaves itself — and `x`
    /// stops the selected running task.
    ActivityList,
    /// Every child agent of the current session, opened from the roster's
    /// overflow row. Enter on a row opens its [`Screen::Activity`] detail.
    /// Observational like the roster itself: nothing here cancels work.
    Team,
    /// Active issues, opened from the state strip's issue cell: a failed turn
    /// and the failed background tasks the projection still holds. A row is one
    /// interaction — Enter or a click selects it and toggles its expansion, and
    /// the expanded row's detail entry (or `→`) opens its [`Screen::Activity`]
    /// page. Nothing here cancels work or clears a failure record.
    Issue,
    /// Full active plan, opened from the workbench's one-line plan summary.
    Plan,
    /// Full active objective, opened from the bounded header summary.
    Goal,
    Help,
    /// Local terminal capability diagnostics; never a runtime health claim.
    Doctor,
    /// Durable runtime observatory (`/trace`).
    Trace,
    /// The Context & Usage page. `/context` and `/usage` are two entry points
    /// into this one page; neither has a body of its own. See
    /// [`Screen::context_section`] for which section a command reveals.
    Context,
    /// `/clean`: reclaim CodeLeveler-owned local storage. A page over the host's
    /// cleanup plan; scan and cleanup run off-thread.
    Clean,
    /// `/schedules`: scheduled automation. A view over the runtime's schedule
    /// management API — list, detail, create/edit, pause, run now, history,
    /// grant and OS wakeup. The runtime owns every definition and run.
    Schedules,
    /// Complete output of a finished local command. File-backed and windowed:
    /// the page paints only the visible rows, so a 50 MiB result costs the
    /// same per frame as a 1 KiB one.
    Output,
}

/// Which section of the Context & Usage page an entry point reveals.
///
/// The page is single: `/context` and `/usage` differ only in where it starts
/// the reader, never in what it renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StatsSection {
    #[default]
    Context,
    Usage,
}

/// Help / popup grouping for slash commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashCategory {
    /// Model, permission, collab, goal — how the agent runs.
    Agent,
    /// Screens and inspection.
    View,
    /// History, export, new.
    Session,
    /// Attachments.
    Input,
    /// Theme, web, help.
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
    // The session's Thinking Level. `Always` because it changes the next
    // request's reasoning, not the shape of the turn.
    slash(
        "/thinking",
        &[],
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
        "/collab",
        &[],
        SlashCategory::Agent,
        SlashVisibility::Quick,
        BusyPolicy::IdleOnly,
    ),
    // Read-only view of the agent registry: `/agents` lists, `/agents <name>`
    // inspects. Creating and editing agents is a conversation or Web task.
    slash(
        "/agents",
        &[],
        SlashCategory::Agent,
        SlashVisibility::Searchable,
        BusyPolicy::Always,
    ),
    // Read-only view of the resolved skill registry: `/skills` lists,
    // `/skills <name>` inspects. Creating and editing skills is a conversation
    // (skill-creator + save_skill/delete_skill) or Web task.
    slash(
        "/skills",
        &[],
        SlashCategory::Agent,
        SlashVisibility::Searchable,
        BusyPolicy::Always,
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
    // The same page as `/context`, positioned at its Usage section. Both are
    // read-only, so both answer while a turn runs.
    slash(
        "/context",
        &[],
        SlashCategory::View,
        SlashVisibility::Searchable,
        BusyPolicy::Always,
    ),
    slash(
        "/usage",
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
    // The crash-recovery gate: review the interrupted calls the runtime could
    // not finish, then explicitly acknowledge that their outcome is unknown.
    // Never a model message, and never safe to do while a turn is running.
    slash(
        "/recover",
        &[],
        SlashCategory::Session,
        SlashVisibility::Searchable,
        BusyPolicy::IdleOnly,
    ),
    slash(
        "/copy",
        &[],
        SlashCategory::Session,
        SlashVisibility::Searchable,
        BusyPolicy::Always,
    ),
    slash(
        "/output",
        &[],
        SlashCategory::Session,
        SlashVisibility::Searchable,
        BusyPolicy::Always,
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
    // Cross-agent continuation: read another coding agent's local session and
    // continue it in a NEW native session. Omitting the id opens that source's
    // picker. `Always`: the current turn keeps running; the import owns a new
    // session of its own.
    slash(
        "/resume-claude",
        &[],
        SlashCategory::Session,
        SlashVisibility::Searchable,
        BusyPolicy::Always,
    ),
    slash(
        "/resume-codex",
        &[],
        SlashCategory::Session,
        SlashVisibility::Searchable,
        BusyPolicy::Always,
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
        "/help",
        &[],
        SlashCategory::System,
        SlashVisibility::Quick,
        BusyPolicy::Always,
    ),
    slash(
        "/doctor",
        &[],
        SlashCategory::System,
        SlashVisibility::Searchable,
        BusyPolicy::Always,
    ),
    // Reclaim CodeLeveler's own local storage. Safe while a turn runs: the scan
    // and cleanup happen off-thread, and only provably-safe items are removed.
    slash(
        "/clean",
        &[],
        SlashCategory::System,
        SlashVisibility::Searchable,
        BusyPolicy::Always,
    ),
    // Scheduled automation. Management is the runtime's; the page never
    // schedules anything itself.
    slash(
        "/schedules",
        &[],
        SlashCategory::Session,
        SlashVisibility::Searchable,
        BusyPolicy::Always,
    ),
    // Self-update. `IdleOnly`: replacing the running binary under an active
    // task would strand it, so the busy gate refuses first.
    slash(
        "/update",
        &["/upgrade"],
        SlashCategory::System,
        SlashVisibility::Searchable,
        BusyPolicy::IdleOnly,
    ),
    // Re-read the configured sources into the RUNNING runtime. Allowed while
    // a turn runs: that turn keeps its generation, the next one uses the new
    // one. Never a message to the model.
    slash(
        "/reload",
        &[],
        SlashCategory::System,
        SlashVisibility::Searchable,
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

/// Byte range of the leading slash-command token in `text` that the composer
/// should paint in the command ink.
///
/// Only the first line is considered, and the token must start the line apart
/// from leading whitespace. A token qualifies when it is a registered primary name
/// or alias, or a prefix of exactly one registered command — `/he` is heading
/// to `/help`, while `/mo` is ambiguous between `/model` and `/mode`. A bare
/// `/` qualifies: it opens the command surface.
///
/// This is presentation only: execution still requires an exact token, so a
/// highlighted prefix is a hint about where the typing is going, not a claim
/// that the typed text already runs. The registry is read in place; no command
/// list is rebuilt per keystroke.
pub fn highlighted_slash_command(text: &str) -> Option<std::ops::Range<usize>> {
    let first_line = text.split('\n').next()?;
    let start = first_line.len() - first_line.trim_start().len();
    let rest = &first_line[start..];
    if !rest.starts_with('/') {
        return None;
    }
    let token = rest.split_whitespace().next()?;
    let range = start..start + token.len();
    if token == "/" {
        // A bare `/` is the command surface itself, but only when nothing
        // follows it: in ` / model` the slash is punctuation between words,
        // not a command.
        return (rest.trim_end() == "/").then_some(range);
    }
    if SLASH_DEFS
        .iter()
        .any(|d| d.name == token || d.aliases.contains(&token))
    {
        return Some(range);
    }
    let mut unique = None;
    for def in SLASH_DEFS {
        let matches =
            def.name.starts_with(token) || def.aliases.iter().any(|alias| alias.starts_with(token));
        if matches {
            if unique.is_some() {
                return None;
            }
            unique = Some(def);
        }
    }
    unique.map(|_| range)
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
    slash_copy(name, t, false)
}

/// One-line palette label. Help keeps the longer [`slash_description`].
pub fn slash_popup_label(name: &str, t: &UiText) -> &'static str {
    slash_copy(name, t, true)
}

fn slash_copy(name: &str, t: &UiText, brief: bool) -> &'static str {
    let s = if brief { &t.slash_brief } else { &t.slash };
    match name {
        // `/recover` is owned by the crash-recovery contract, whose copy sits
        // under `recovery_*` rather than the general slash table: the two must
        // not drift apart when the wording changes.
        "/recover" => t.recovery_command_desc,
        "/model" => s.model,
        "/thinking" => s.thinking,
        "/permission" | "/mode" => s.permission,
        "/goal" => s.goal,
        "/btw" => s.btw,
        "/recap" => s.recap,
        "/collab" => s.collab,
        "/agents" => s.agents,
        "/skills" => s.skills,
        "/diff" => s.diff,
        "/trace" => s.trace,
        "/context" => s.context,
        "/usage" => s.usage,
        "/sessions" => s.sessions,
        "/resume-claude" => s.resume_claude,
        "/resume-codex" => s.resume_codex,
        "/restore" | "/checkpoint" | "/rewind" => s.restore,
        "/fork" => s.fork,
        "/compact" => s.compact,
        "/copy" => s.copy,
        "/output" => s.output,
        "/export" | "/save" => s.export,
        "/web" => s.web,
        "/image" | "/attach" => s.attach,
        "/theme" => s.theme,
        "/new" => s.new,
        "/help" => s.help,
        "/doctor" => s.doctor,
        "/clean" => s.clean,
        "/schedules" => s.schedules,
        "/update" | "/upgrade" => s.update,
        "/reload" => s.reload,
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
    // Shell Mode owns the composer's completion surface.
    if state.input_mode == crate::shell_mode::InputMode::Shell || state.slash_popup_dismissed {
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
    // In Shell Mode the `$` belongs to the shell.
    if state.input_mode == crate::shell_mode::InputMode::Shell || state.slash_popup_dismissed {
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
            let label = if desc.is_empty() {
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
    // In Shell Mode the `@` belongs to the shell (same rule as `$`).
    if state.input_mode == crate::shell_mode::InputMode::Shell || state.slash_popup_dismissed {
        return Vec::new();
    }
    let Some(query) = file_mention_query(state) else {
        return Vec::new();
    };
    let query = query.to_ascii_lowercase();
    // One source: the paths the composer asked the runtime for. The old
    // compiler's candidate list was never filled, so the fallback it backed was
    // a branch that could not be taken.
    let source = &state.file_candidates;
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
    fn retired_work_mode_is_absent_from_all_completion_tokens() {
        let tokens = all_slash_tokens();
        for retired in ["/work-mode", "/work_mode", "/work-profile"] {
            assert!(
                !tokens.contains(&retired),
                "{retired} is still discoverable"
            );
        }
    }

    #[test]
    fn memory_is_absent_from_parse_completion_help_and_aliases() {
        for token in ["/memory", "/remember", "/forget", "/consolidate_memory"] {
            assert!(!all_slash_tokens().contains(&token));
            assert!(!super::is_known_slash_token(token));
            assert!(super::slash_primary(token).is_none());
            assert!(super::slash_def(token).is_none());
            for locale in [crate::i18n::Locale::Zh, crate::i18n::Locale::En] {
                assert!(super::slash_matches(token, locale.text()).is_empty());
                assert!(
                    !super::slash_commands(locale.text())
                        .iter()
                        .any(|(name, _)| *name == token)
                );
            }
        }
    }

    #[test]
    fn remember_is_absent_from_all_completion_tokens() {
        assert!(!all_slash_tokens().contains(&"/remember"));
    }

    /// `/develop` was removed outright. It must not survive as a completion,
    /// a help row, or an alias that quietly starts a workflow.
    #[test]
    fn develop_is_absent_from_every_command_surface() {
        let tokens = all_slash_tokens();
        for retired in ["/develop", "/dev", "develop"] {
            assert!(
                !tokens.contains(&retired),
                "{retired} is still discoverable"
            );
        }
        assert!(!super::is_known_slash_token("develop"));
        assert!(super::slash_def("/develop").is_none());
        assert!(super::slash_primary("/develop").is_none());
    }

    #[test]
    fn registry_is_the_closed_command_surface() {
        let names: Vec<_> = SLASH_DEFS.iter().map(|d| d.name).collect();
        assert_eq!(
            names,
            [
                "/model",
                "/permission",
                "/thinking",
                "/goal",
                "/btw",
                "/recap",
                "/collab",
                "/agents",
                "/skills",
                "/diff",
                "/trace",
                "/context",
                "/usage",
                "/sessions",
                "/restore",
                "/fork",
                "/compact",
                "/recover",
                "/copy",
                "/output",
                "/export",
                "/new",
                "/resume-claude",
                "/resume-codex",
                "/attach",
                "/theme",
                "/web",
                "/help",
                "/doctor",
                "/clean",
                "/schedules",
                "/update",
                "/reload",
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
    /// is not registered is dead residue; a registered command without both is
    /// a broken promise.
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
            "retired commands have no argument hint"
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
                "/thinking",
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
            "/agents",
            "/skills",
            "/trace",
            "/sessions",
            "/restore",
            "/fork",
            "/export",
            "/attach",
            "/theme",
            "/web",
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
                model_notice: None,
                thinking: None,
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
            &[("release", "分阶段发布"), ("repo-lint", "检查")],
        );
        for text in ["/", "/feat", "/repo"] {
            s.composer.replace(text);
            let rows = super::visible_slash_popup(&s);
            assert!(
                rows.iter()
                    .all(|(n, _)| n != "/release" && n != "/repo-lint"),
                "{text}: {rows:?}"
            );
        }
    }

    #[test]
    fn bare_dollar_lists_every_skill() {
        let mut s = skill_state(
            Locale::Zh,
            &[("release", "分阶段发布"), ("repo-lint", "本仓库检查")],
        );
        s.composer.replace("$");
        let names: Vec<_> = super::visible_skill_popup(&s)
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(names, ["$release", "$repo-lint"]);
        s.composer.replace("run $repo");
        let names: Vec<_> = super::visible_skill_popup(&s)
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(names, ["$repo-lint"]);
    }

    #[test]
    fn skill_descriptions_are_not_rewritten() {
        let long = "分阶段实现一个新功能：先摸清现状和需求";
        let mut zh = skill_state(
            Locale::Zh,
            &[("release", long), ("repo-lint", "本仓库的检查脚本")],
        );
        zh.composer.replace("$");
        let rows = super::visible_skill_popup(&zh);
        assert_eq!(
            rows,
            [
                ("$release".to_string(), long.to_string()),
                ("$repo-lint".to_string(), "本仓库的检查脚本".to_string()),
            ]
        );
        let mut en = skill_state(Locale::En, &[("release", long), ("bare", "")]);
        en.composer.replace("$");
        let rows = super::visible_skill_popup(&en);
        assert_eq!(
            rows,
            [
                ("$release".to_string(), long.to_string()),
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

    pub fn label(self, t: &crate::i18n::UiText) -> &'static str {
        match self {
            ToolFilter::All => t.tool_filter_all,
            ToolFilter::Read => t.tool_filter_read,
            ToolFilter::Write => t.tool_filter_write,
            ToolFilter::Shell => t.tool_filter_shell,
            ToolFilter::Failed => t.tool_filter_failed,
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

#[cfg(test)]
mod reachability_tests {
    use super::Screen;

    /// The production keystroke, command or runtime event that opens a screen.
    ///
    /// The match is exhaustive on purpose: a new `Screen` variant cannot be
    /// added without declaring how a user reaches it. `None` means the variant
    /// is declared and rendered but nothing outside a test can open it — a
    /// dead surface, which this crate does not keep.
    fn production_opener(screen: Screen) -> Option<&'static str> {
        match screen {
            Screen::Conversation => Some("the default screen"),
            Screen::Tools => Some("Ctrl+T — reducer/mod.rs toggle_screen"),
            Screen::Diff => Some("Ctrl+D / `/diff` — screen_nav::open_diff_screen"),
            Screen::Sessions => Some("`/sessions` — screen_nav::open_sessions_screen"),
            Screen::ForeignSessions => {
                Some("`/resume-claude` / `/resume-codex` — reducer/submit.rs")
            }
            Screen::Activity => Some("clicking an activity row — activity.rs"),
            Screen::ActivityList => Some(
                "clicking the strip's background cell or Enter under its focus — reducer/mod.rs",
            ),
            Screen::Issue => Some("clicking the strip's issue cell — reducer/mod.rs"),
            Screen::Team => Some("the roster's overflow row — reducer/mod.rs"),
            Screen::Plan => Some("Enter or click on the plan summary — reducer/mod.rs"),
            Screen::Goal => Some("Enter or click on the header goal — reducer/mod.rs"),
            Screen::Help => Some("Ctrl+? / `/help` — toggle_screen"),
            Screen::Doctor => Some("`/doctor` — toggle_screen"),
            Screen::Trace => Some("`/trace` — screen_nav::open_trace"),
            Screen::Clean => Some("`/clean` — screen_nav::open_clean"),
            Screen::Schedules => Some("`/schedules` — reducer/submit.rs"),
            Screen::Context => Some("`/context` / `/usage` — screen_nav::open_stats"),
            Screen::Output => Some("Enter/click on a finished local command — reducer/mod.rs"),
        }
    }

    #[test]
    fn every_declared_screen_has_a_production_entry() {
        let all = [
            Screen::Conversation,
            Screen::Tools,
            Screen::Diff,
            Screen::Sessions,
            Screen::ForeignSessions,
            Screen::Activity,
            Screen::ActivityList,
            Screen::Team,
            Screen::Plan,
            Screen::Goal,
            Screen::Help,
            Screen::Doctor,
            Screen::Trace,
            Screen::Context,
            Screen::Schedules,
            Screen::Output,
        ];
        for screen in all {
            assert!(
                production_opener(screen).is_some(),
                "{screen:?} is declared and rendered but no production path opens it"
            );
        }
    }
}

#[cfg(test)]
mod highlight_tests {
    use super::highlighted_slash_command;

    fn range(text: &str) -> Option<(usize, usize)> {
        highlighted_slash_command(text).map(|r| (r.start, r.end))
    }

    #[test]
    fn a_registered_command_is_highlighted_up_to_the_argument() {
        assert_eq!(range("/model"), Some((0, 6)));
        assert_eq!(range("/model deepseek-v4.1"), Some((0, 6)));
        assert_eq!(range("/mode"), Some((0, 5)), "alias is a command too");
        assert_eq!(range("/model\nnext line"), Some((0, 6)));
        assert_eq!(range("/model\targ"), Some((0, 6)));
        assert_eq!(range("  /model arg"), Some((2, 8)), "leading spaces");
    }

    #[test]
    fn a_unique_prefix_is_highlighted_but_an_ambiguous_one_is_not() {
        assert_eq!(range("/he"), Some((0, 3)), "only /help starts with /he");
        assert_eq!(range("/check"), Some((0, 6)), "only the /checkpoint alias");
        assert_eq!(range("/mo"), None, "/model and /mode both continue /mo");
        assert_eq!(range("/mod"), None);
    }

    #[test]
    fn a_bare_slash_is_the_command_surface() {
        assert_eq!(range("/"), Some((0, 1)));
        assert_eq!(range("  /"), Some((2, 3)));
    }

    #[test]
    fn unknown_tokens_and_ordinary_text_stay_plain() {
        for text in [
            "",
            "这个项目怎么本地跑起来？",
            "/unknown-command",
            "/hlep",
            "/modelt",
            "/Users/mengmian/project",
            "hello /model",
            "\n/model",
            " / model",
        ] {
            assert_eq!(range(text), None, "{text:?} must not look like a command");
        }
    }
}
