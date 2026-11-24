//! `/skills` — the Skills management page.
//!
//! One read model over the SAME [`leveler_skills::SkillRegistry`] the runtime,
//! the `$` popup and `load_skill` use. The page adds presentation and one
//! durable action: enabling or disabling a definition through
//! `state/skills.toml`. It never discovers packages itself, never edits a
//! `SKILL.md`, and never approves an untrusted package — that stays `/trust`.
//!
//! Every row is built from registry facts. Disabled, invalid, untrusted and
//! scan-problem rows are kept visible because they are exactly what a user
//! opens this page to diagnose.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use leveler_definition_trust::DefinitionTrustView;
use leveler_skills::{
    PreferencesState, SkillRegistry, SkillRoots, SkillScope, SkillSource, SkillState,
    is_mentionable, is_reserved_slash_name,
};

use crate::action::Effect;
use crate::i18n::UiText;
use crate::screen::Screen;
use crate::state::AppState;
use crate::theme::Theme;

/// Which rows the page shows. `All` also reveals scan problems; the other
/// filters are the four per-skill states the registry distinguishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SkillFilter {
    #[default]
    All,
    Available,
    Disabled,
    Untrusted,
    Invalid,
}

impl SkillFilter {
    fn next(self) -> Self {
        match self {
            Self::All => Self::Available,
            Self::Available => Self::Disabled,
            Self::Disabled => Self::Untrusted,
            Self::Untrusted => Self::Invalid,
            Self::Invalid => Self::All,
        }
    }

    fn label(self, t: &UiText) -> &'static str {
        match self {
            Self::All => t.skills.filter_all,
            Self::Available => t.skills.filter_available,
            Self::Disabled => t.skills.filter_disabled,
            Self::Untrusted => t.skills.filter_untrusted,
            Self::Invalid => t.skills.filter_invalid,
        }
    }

    fn matches(self, section: SkillSection) -> bool {
        match self {
            Self::All => true,
            Self::Available => section == SkillSection::Available,
            Self::Disabled => section == SkillSection::Disabled,
            Self::Untrusted => section == SkillSection::Untrusted,
            Self::Invalid => section == SkillSection::Invalid,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillSection {
    Available,
    Disabled,
    Untrusted,
    Invalid,
    Problem,
}

impl SkillSection {
    fn label(self, t: &UiText) -> &'static str {
        match self {
            Self::Available => t.skills.section_available,
            Self::Disabled => t.skills.section_disabled,
            Self::Untrusted => t.skills.section_untrusted,
            Self::Invalid => t.skills.section_invalid,
            Self::Problem => t.skills.section_problems,
        }
    }

    fn marker(self) -> &'static str {
        match self {
            Self::Available => "●",
            Self::Disabled => "◌",
            Self::Untrusted => "○",
            Self::Invalid => "!",
            Self::Problem => "?",
        }
    }

    /// Display order. Registry order is by precedence and source, so the same
    /// state can appear in more than one run; grouping by state keeps each
    /// header contiguous.
    fn order(self) -> u8 {
        match self {
            Self::Available => 0,
            Self::Disabled => 1,
            Self::Untrusted => 2,
            Self::Invalid => 3,
            Self::Problem => 4,
        }
    }
}

/// One display row. Entries come from the resolved registry; untrusted rows
/// come from the trust refusal list; problem rows come from scan failures.
#[derive(Debug, Clone)]
pub struct SkillRow {
    /// Stable selection identity, independent of filtering or position.
    pub id: String,
    pub section: SkillSection,
    pub name: String,
    pub scope: Option<SkillScope>,
    pub source: Option<SkillSource>,
    pub description: Option<String>,
    pub argument_hint: Option<String>,
    /// The disabled/invalid reason or the trust state, when unusable.
    pub detail: Option<String>,
    pub advertised: String,
    pub canonical: String,
    pub location: Option<PathBuf>,
    pub user_invocable: bool,
    pub disable_model_invocation: bool,
    pub mentionable: bool,
    /// `scope:name` + source of every definition this winner shadows.
    pub shadowed: Vec<String>,
    /// A definition exists and the page may toggle it.
    pub toggleable: bool,
}

impl SkillRow {
    fn is_disabled(&self) -> bool {
        self.section == SkillSection::Disabled
    }
}

#[derive(Debug, Default)]
pub struct SkillsState {
    pub rows: Vec<SkillRow>,
    pub selected: usize,
    pub filter: SkillFilter,
    pub query: String,
    pub searching: bool,
    pub detail_open: bool,
    pub notice: Option<String>,
    /// File-level preferences error: every Skill is fail-closed until fixed.
    pub error: Option<String>,
    pub project: Option<PathBuf>,
    pub home: Option<PathBuf>,
    /// Test seam: read skills and preferences from this home instead of the
    /// process environment. Production leaves this `None`.
    pub home_override: Option<PathBuf>,
}

impl SkillsState {
    fn reset_view(&mut self) {
        self.selected = 0;
        self.detail_open = false;
        self.searching = false;
        self.query.clear();
        self.notice = None;
        self.error = None;
    }
}

fn safe(text: &str) -> String {
    leveler_definition_trust::sanitize_line(text)
}

pub(crate) fn scope_label(scope: SkillScope, t: &UiText) -> &'static str {
    match scope {
        SkillScope::Project => t.skills.scope_project,
        SkillScope::User => t.skills.scope_user,
        SkillScope::Builtin => t.skills.scope_builtin,
    }
}

fn advertised(entry: &leveler_skills::SkillEntry) -> String {
    if is_reserved_slash_name(&entry.name) {
        format!("{}:{}", entry.scope.as_str(), entry.name)
    } else {
        entry.name.clone()
    }
}

/// Project one loaded registry into page rows. Pure: the page never scans.
pub fn rows(registry: &SkillRegistry) -> Vec<SkillRow> {
    let mut rows = Vec::new();
    for entry in registry.entries() {
        let (section, detail) = match &entry.state {
            SkillState::Available => (SkillSection::Available, None),
            SkillState::Disabled { reason } => (SkillSection::Disabled, Some(reason.clone())),
            SkillState::Invalid(error) => (SkillSection::Invalid, Some(error.clone())),
        };
        rows.push(SkillRow {
            id: format!("skill:{}:{}", entry.scope.as_str(), entry.name),
            section,
            name: entry.name.clone(),
            scope: Some(entry.scope),
            source: Some(entry.source),
            description: entry.description.clone(),
            argument_hint: entry.argument_hint.clone(),
            detail,
            advertised: advertised(entry),
            canonical: format!("{}:{}", entry.scope.as_str(), entry.name),
            location: entry.location.clone(),
            user_invocable: entry.user_invocable,
            disable_model_invocation: entry.disable_model_invocation,
            mentionable: is_mentionable(&entry.name),
            shadowed: entry
                .shadowed
                .iter()
                .map(|shadowed| {
                    format!(
                        "{}:{} ({}){}",
                        shadowed.scope.as_str(),
                        entry.name,
                        shadowed.source.label(),
                        shadowed
                            .error
                            .as_ref()
                            .map(|error| format!(" — {error}"))
                            .unwrap_or_default()
                    )
                })
                .collect(),
            toggleable: true,
        });
    }
    for untrusted in registry.untrusted_skills() {
        if registry.get(&untrusted.name).is_some() {
            continue;
        }
        rows.push(SkillRow {
            id: format!("untrusted:{}:{}", untrusted.scope.as_str(), untrusted.name),
            section: SkillSection::Untrusted,
            name: untrusted.name.clone(),
            scope: Some(untrusted.scope),
            source: Some(untrusted.source),
            description: None,
            argument_hint: None,
            detail: Some(untrusted.state().to_string()),
            advertised: untrusted.name.clone(),
            canonical: format!("{}:{}", untrusted.scope.as_str(), untrusted.name),
            location: Some(untrusted.location.clone()),
            user_invocable: true,
            disable_model_invocation: false,
            mentionable: is_mentionable(&untrusted.name),
            shadowed: Vec::new(),
            toggleable: false,
        });
    }
    for (index, problem) in registry.problems().iter().enumerate() {
        // An untrusted package is a real definition with its own section and
        // its own next step. The registry also reports it as "not loaded" so
        // no caller mistakes it for available; the page must not show the
        // same directory twice as if trust were a scan failure.
        if registry
            .untrusted_skills()
            .iter()
            .any(|untrusted| untrusted.location == problem.location)
        {
            continue;
        }
        rows.push(SkillRow {
            id: format!("problem:{index}"),
            section: SkillSection::Problem,
            name: leveler_core::path::display_path(&problem.location).into_owned(),
            scope: Some(problem.scope),
            source: Some(problem.source),
            description: None,
            argument_hint: None,
            detail: Some(problem.error.clone()),
            advertised: String::new(),
            canonical: String::new(),
            location: Some(problem.location.clone()),
            user_invocable: false,
            disable_model_invocation: false,
            mentionable: false,
            shadowed: Vec::new(),
            toggleable: false,
        });
    }
    group_by_section(&mut rows);
    rows
}

/// Group the page by state without disturbing the registry order inside a
/// section. The sort is stable, so project/user/builtin order survives.
fn group_by_section(rows: &mut [SkillRow]) {
    rows.sort_by_key(|row| row.section.order());
}

/// The project root this client is attached to, with a `~/…` display path
/// expanded. `None` before the runtime reports a repository.
pub(crate) fn repository_root(state: &AppState) -> Option<PathBuf> {
    let raw = state.repository.as_deref()?;
    if let Some(rest) = raw.strip_prefix("~/")
        && let Some(home) = leveler_core::environment()
            .var_os("HOME")
            .or_else(|| leveler_core::environment().var_os("USERPROFILE"))
    {
        return Some(PathBuf::from(home).join(rest));
    }
    Some(PathBuf::from(raw))
}

fn project_root(state: &AppState) -> PathBuf {
    repository_root(state)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default()
}

pub(crate) fn home_root(state: &AppState) -> PathBuf {
    state.skills_page.home_override.clone().unwrap_or_else(|| {
        leveler_core::LevelerHome::resolve(leveler_core::environment())
            .root()
            .to_path_buf()
    })
}

/// Build the registry from this page's stated roots.
///
/// Production reads the live environment exactly like `leveler_skills::describe`.
/// A test home overrides both `LEVELER_HOME` and `HOME` so no real user store
/// is touched.
pub(crate) fn registry_at(root: &Path, home: &Path, home_override: Option<&Path>) -> SkillRegistry {
    let lookup = |key: &str| -> Option<OsString> {
        match (home_override, key) {
            (Some(home), "LEVELER_HOME") => Some(home.as_os_str().to_os_string()),
            (Some(home), "HOME" | "USERPROFILE") => home
                .parent()
                .map(|parent| parent.as_os_str().to_os_string()),
            _ => leveler_core::environment().var_os(key),
        }
    };
    let base = leveler_core::LevelerHome::from_root(home);
    SkillRegistry::load(
        &SkillRoots::for_project_in(root, &lookup),
        &DefinitionTrustView::load(base.root(), Some(root)),
        &PreferencesState::load(&base, Some(root)),
    )
}

/// The live registry for this client's project, or `None` before a repository
/// is known. Every client-side reader — the page, `$` completion and the `/`
/// preflight — goes through here, so they cannot disagree about what is
/// available.
pub(crate) fn registry_for(state: &AppState) -> Option<SkillRegistry> {
    let root = repository_root(state)?;
    let home = home_root(state);
    Some(registry_at(
        &root,
        &home,
        state.skills_page.home_override.as_deref(),
    ))
}

/// How a composer message relates to the Skill slash namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SlashSkillRequest {
    /// A known, user-invocable Skill: send the message as typed; the runtime
    /// resolves and injects the procedure.
    Usable,
    /// A known definition that cannot run: never send it as a normal turn.
    Refused(leveler_skills::SkillCommandError),
    /// Not a Skill name (prose, path, built-in, or unknown typo).
    NotASkill,
}

/// Classify a `/name` request against a FRESH registry, so a stale completion
/// cache can never authorize a load.
pub(crate) fn classify_slash_skill_request(state: &AppState, text: &str) -> SlashSkillRequest {
    let Some(token) = leveler_skills::leading_skill_token(text) else {
        return SlashSkillRequest::NotASkill;
    };
    let Some(registry) = registry_for(state) else {
        return SlashSkillRequest::NotASkill;
    };
    match registry.resolve_command(&token, leveler_skills::RESERVED_SLASH_NAMES) {
        Ok(_) => SlashSkillRequest::Usable,
        // An untrusted package is the one refusal the runtime can turn into
        // a concrete authorization decision. Send the explicit request so the
        // host can ask about these exact bytes; a refusal there rejects the
        // submission and the draft comes back. Every other refusal is final
        // for this message.
        Err(leveler_skills::SkillCommandError::Untrusted { .. }) => SlashSkillRequest::Usable,
        Err(
            leveler_skills::SkillCommandError::NotFound { .. }
            | leveler_skills::SkillCommandError::Reserved { .. },
        ) => SlashSkillRequest::NotASkill,
        Err(error) => SlashSkillRequest::Refused(error),
    }
}

/// A Skill request a running turn cannot deliver: steering appends plain text
/// at the next model round and never rebuilds the skill injection, so the
/// procedure would be silently lost. Returns the request as the user wrote it
/// (`/deploy` or `$deploy`) for the notice.
pub(crate) fn mid_turn_skill_request(state: &AppState, text: &str) -> Option<String> {
    let registry = registry_for(state)?;
    if let Some(token) = leveler_skills::leading_skill_token(text) {
        match registry.resolve_command(&token, leveler_skills::RESERVED_SLASH_NAMES) {
            Ok(command) => return Some(format!("/{}", command.advertised)),
            // Untrusted is still a Skill request: it needs a new-turn load
            // and an authorization decision, neither of which a steer can
            // carry.
            Err(leveler_skills::SkillCommandError::Untrusted { name, .. }) => {
                return Some(format!("/{name}"));
            }
            // Unknown and reserved tokens are ordinary prose here; disabled
            // and model-only requests were refused before the busy branch.
            Err(_) => {}
        }
    }
    leveler_skills::parse_skill_mentions(text)
        .into_iter()
        .find(|name| registry.get(name).is_some() || registry.untrusted_skill(name).is_some())
        .map(|name| format!("${name}"))
}

/// Open the page. `/skills <query>` opens it with the query applied; repeating
/// a bare `/skills` while it is already active closes it.
pub fn open(state: &mut AppState, query: &str) -> Vec<Effect> {
    let query = query.trim();
    if state.active_screen == Screen::Skills && query.is_empty() {
        close(state);
        return Vec::new();
    }
    state.active_screen = Screen::Skills;
    state.screen_scroll = 0;
    if query.is_empty() {
        state.skills_page.reset_view();
    } else {
        state.skills_page.detail_open = false;
        state.skills_page.searching = false;
        state.skills_page.query = query.to_string();
        state.skills_page.selected = 0;
        state.skills_page.notice = None;
    }
    refresh(state);
    Vec::new()
}

pub fn close(state: &mut AppState) {
    state.active_screen = Screen::Conversation;
    state.screen_scroll = 0;
    state.skills_page.detail_open = false;
    state.skills_page.searching = false;
    state.skills_page.notice = None;
    state.skills_page.error = None;
}

/// One level back: detail → list → conversation.
pub fn back(state: &mut AppState) -> Vec<Effect> {
    if state.skills_page.detail_open {
        state.skills_page.detail_open = false;
        state.screen_scroll = 0;
        return Vec::new();
    }
    if state.skills_page.searching {
        state.skills_page.searching = false;
        return Vec::new();
    }
    close(state);
    Vec::new()
}

/// Re-read the registry and rebuild rows, preserving the selected identity
/// and the page's notice when the identity still exists.
pub fn refresh(state: &mut AppState) {
    let root = project_root(state);
    let home = home_root(state);
    let override_home = state.skills_page.home_override.clone();
    let registry = registry_at(&root, &home, override_home.as_deref());
    let selected = selected_id(state);
    state.skills_page.project = Some(root);
    state.skills_page.home = Some(home);
    state.skills_page.error = registry.preferences().error().map(str::to_string);
    refresh_from(state, &registry);
    if let Some(id) = selected
        && let Some(index) = visible_indices(state)
            .into_iter()
            .position(|row| state.skills_page.rows[row].id == id)
    {
        state.skills_page.selected = index;
    }
    clamp_selection(state);
}

/// Rebuild rows from an already-loaded registry. Tests use it to prove the
/// projection without touching a trust store; production calls [`refresh`].
pub fn refresh_from(state: &mut AppState, registry: &SkillRegistry) {
    state.skills_page.rows = rows(registry);
    state.skills_page.detail_open = false;
    state.screen_scroll = 0;
    clamp_selection(state);
}

fn visible_indices(state: &AppState) -> Vec<usize> {
    let page = &state.skills_page;
    let needle = page.query.trim().to_lowercase();
    page.rows
        .iter()
        .enumerate()
        .filter(|(_, row)| page.filter.matches(row.section))
        .filter(|(_, row)| {
            needle.is_empty()
                || row.name.to_lowercase().contains(&needle)
                || row
                    .description
                    .as_deref()
                    .is_some_and(|description| description.to_lowercase().contains(&needle))
                || row
                    .argument_hint
                    .as_deref()
                    .is_some_and(|hint| hint.to_lowercase().contains(&needle))
                || row.canonical.to_lowercase().contains(&needle)
        })
        .map(|(index, _)| index)
        .collect()
}

fn clamp_selection(state: &mut AppState) {
    let len = visible_indices(state).len();
    state.skills_page.selected = state.skills_page.selected.min(len.saturating_sub(1));
}

fn selected_id(state: &AppState) -> Option<String> {
    let indices = visible_indices(state);
    indices
        .get(state.skills_page.selected)
        .map(|&index| state.skills_page.rows[index].id.clone())
}

fn selected_row(state: &AppState) -> Option<&SkillRow> {
    let indices = visible_indices(state);
    indices
        .get(state.skills_page.selected)
        .map(|&index| &state.skills_page.rows[index])
}

fn move_selection(state: &mut AppState, delta: isize) {
    let len = visible_indices(state).len();
    if len == 0 {
        return;
    }
    let current = state.skills_page.selected.min(len - 1);
    state.skills_page.selected = if delta < 0 {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        (current + delta as usize).min(len - 1)
    };
}

/// Enable or disable the selected definition.
///
/// Writes only through `leveler_skills::SkillPreferences`; the registry is
/// re-read after a successful save, so the page never shows a state that is
/// not on disk. A running turn keeps the procedure it already received, so a
/// toggle while busy is refused instead of pretending it took effect.
pub fn toggle_selected(state: &mut AppState) {
    let Some(row) = selected_row(state).cloned() else {
        return;
    };
    let t = state.t();
    if !row.toggleable {
        let message = if row.section == SkillSection::Untrusted {
            t.skills.untrusted_hint.to_string()
        } else {
            t.skills.cannot_toggle.to_string()
        };
        state.skills_page.notice = Some(message);
        return;
    }
    if state.is_busy() {
        state.skills_page.notice = Some(t.skills.busy_refuse.to_string());
        return;
    }
    let (Some(scope), name) = (row.scope, row.name.clone()) else {
        return;
    };
    let project = project_root(state);
    let home = home_root(state);
    let file = leveler_core::LevelerHome::from_root(&home).skills_preferences_file();
    let mut preferences = match PreferencesState::load_path(&file, Some(&project)) {
        PreferencesState::Loaded { preferences, .. } => preferences,
        PreferencesState::Unavailable { error, path, .. } => {
            state.skills_page.error = Some(format!(
                "{}: {error}",
                leveler_core::path::display_path(&path)
            ));
            return;
        }
    };
    let disable = !row.is_disabled();
    let key = leveler_skills::project_key(&project);
    if let Err(error) = preferences.set_disabled(scope, &name, Some(&key), disable) {
        state.skills_page.error = Some(error);
        return;
    }
    if let Err(error) = preferences.save(&file) {
        state.skills_page.error = Some(format!(
            "{}: {error}",
            leveler_core::path::display_path(&file)
        ));
        return;
    }
    // The `$` popup caches by root; a stale cache would offer a name the
    // registry now refuses. The runtime re-reads preferences every turn.
    state.skill_catalog_root = None;
    let id = row.id.clone();
    refresh(state);
    state.skills_page.notice = Some(
        if disable {
            t.skills.disabled_written
        } else {
            t.skills.enabled_written
        }
        .replace("{}", &format!("/{}", row.advertised)),
    );
    if let Some(index) = visible_indices(state)
        .into_iter()
        .position(|row| state.skills_page.rows[row].id == id)
    {
        state.skills_page.selected = index;
    }
}

pub fn handle_key(state: &mut AppState, key: KeyEvent) -> Vec<Effect> {
    if state.skills_page.searching {
        match key.code {
            KeyCode::Esc | KeyCode::Enter => state.skills_page.searching = false,
            KeyCode::Backspace => {
                state.skills_page.query.pop();
                state.skills_page.selected = 0;
                clamp_selection(state);
            }
            KeyCode::Up | KeyCode::Char('k') => move_selection(state, -1),
            KeyCode::Down | KeyCode::Char('j') => move_selection(state, 1),
            KeyCode::Char(c) if !c.is_control() => {
                if state.skills_page.query.chars().count() < 120 {
                    state.skills_page.query.push(c);
                }
                state.skills_page.selected = 0;
                clamp_selection(state);
            }
            _ => {}
        }
        return Vec::new();
    }
    if state.skills_page.detail_open {
        match key.code {
            KeyCode::Esc => {
                back(state);
            }
            KeyCode::Up | KeyCode::Char('k') => {
                state.screen_scroll = state.screen_scroll.saturating_sub(1)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                state.screen_scroll = state.screen_scroll.saturating_add(1)
            }
            KeyCode::PageUp => state.screen_scroll = state.screen_scroll.saturating_sub(10),
            KeyCode::PageDown => state.screen_scroll = state.screen_scroll.saturating_add(10),
            KeyCode::Char(' ') => toggle_selected(state),
            _ => {}
        }
        return Vec::new();
    }
    match key.code {
        KeyCode::Esc => {
            back(state);
        }
        KeyCode::Up | KeyCode::Char('k') => move_selection(state, -1),
        KeyCode::Down | KeyCode::Char('j') => move_selection(state, 1),
        KeyCode::PageUp => move_selection(state, -10),
        KeyCode::PageDown => move_selection(state, 10),
        KeyCode::Enter => {
            if selected_row(state).is_some() {
                state.skills_page.detail_open = true;
                state.screen_scroll = 0;
            }
        }
        KeyCode::Char(' ') => toggle_selected(state),
        KeyCode::Char('f') | KeyCode::Tab => {
            state.skills_page.filter = state.skills_page.filter.next();
            state.skills_page.selected = 0;
            clamp_selection(state);
        }
        KeyCode::Char('s') | KeyCode::Char('/') => {
            state.skills_page.searching = true;
            state.skills_page.detail_open = false;
            state.screen_scroll = 0;
        }
        KeyCode::Char('r') => {
            let notice = state.skills_page.notice.take();
            refresh(state);
            state.skills_page.notice = notice;
        }
        _ => {}
    }
    Vec::new()
}

pub fn wheel(state: &mut AppState, notches: isize) {
    if state.skills_page.detail_open {
        let lines = (notches * 3).unsigned_abs();
        state.screen_scroll = if notches < 0 {
            state.screen_scroll.saturating_sub(lines)
        } else {
            state.screen_scroll.saturating_add(lines)
        };
        return;
    }
    move_selection(state, notches);
}

pub fn select_row(state: &mut AppState, key: &str) {
    let indices = visible_indices(state);
    if let Ok(index) = key.parse::<usize>() {
        if index < indices.len() {
            state.skills_page.selected = index;
        }
        return;
    }
    if let Some(position) = indices
        .into_iter()
        .position(|row| state.skills_page.rows[row].id == key)
    {
        state.skills_page.selected = position;
    }
}

fn row_style(section: SkillSection, theme: &Theme) -> Style {
    match section {
        SkillSection::Available => Style::default().fg(theme.status.success),
        SkillSection::Disabled => Style::default().fg(theme.text.muted),
        SkillSection::Untrusted => Style::default().fg(theme.status.warning),
        SkillSection::Invalid | SkillSection::Problem => Style::default().fg(theme.status.error),
    }
}

fn section_status(section: SkillSection, t: &UiText) -> &'static str {
    match section {
        SkillSection::Available => t.skills.status_available,
        SkillSection::Disabled => t.skills.status_disabled,
        SkillSection::Untrusted => t.skills.status_untrusted,
        SkillSection::Invalid => t.skills.status_invalid,
        SkillSection::Problem => t.skills.status_problem,
    }
}

pub fn render_screen(frame: &mut Frame, area: Rect, state: &mut AppState) {
    let t = state.t();
    let page = crate::secondary::SecondaryPage {
        title: t.skills.title,
        status: crate::secondary::main_status_spans(state, area.width as usize),
        hint: if state.skills_page.searching {
            t.skills.hint_search
        } else if state.skills_page.detail_open {
            t.skills.hint_detail
        } else {
            t.skills.hint_list
        },
    };
    let layout = crate::secondary::page_layout(state, area, 0);
    crate::secondary::draw_header(frame, state, &layout, &page);
    crate::secondary::draw_footer(frame, state, &layout, &page);
    if state.skills_page.detail_open {
        render_detail(frame, layout.content, state);
    } else {
        render_list(frame, layout.content, state);
    }
}

fn render_list(frame: &mut Frame, area: Rect, state: &mut AppState) {
    let t = state.t();
    let theme = &state.theme;
    let page = &state.skills_page;
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(
            format!("{}: ", t.skills.project),
            Style::default().fg(theme.text.muted),
        ),
        Span::styled(
            page.project
                .as_ref()
                .map(|path| safe(&leveler_core::path::display_path(path)))
                .unwrap_or_default(),
            Style::default().fg(theme.text.secondary),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled(
            format!("{}: ", t.skills.filter),
            Style::default().fg(theme.text.muted),
        ),
        Span::styled(
            page.filter.label(t),
            Style::default()
                .fg(theme.accent.primary)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {}", t.skills.filter_hint),
            Style::default().fg(theme.text.muted),
        ),
    ]));
    if page.searching || !page.query.is_empty() {
        lines.push(Line::from(vec![
            Span::styled(
                format!("{}: ", t.skills.search),
                Style::default().fg(theme.text.muted),
            ),
            Span::styled(
                if page.searching {
                    format!("{}▌", safe(&page.query))
                } else {
                    safe(&page.query)
                },
                Style::default().fg(theme.text.secondary),
            ),
        ]));
    }
    if let Some(error) = &page.error {
        lines.extend(crate::render::list_state_lines(
            crate::render::ListStateBlock {
                kind: crate::render::ListStateKind::Error,
                message: &format!("{}: {error}", t.skills.preferences_error),
                detail: None,
            },
            theme,
            area.width as usize,
        ));
    } else if let Some(notice) = &page.notice {
        lines.push(Line::from(Span::styled(
            format!("✓ {notice}"),
            Style::default().fg(theme.status.success),
        )));
    }
    lines.push(Line::default());

    let indices = visible_indices(state);
    if indices.is_empty() {
        lines.extend(crate::render::list_state_lines(
            crate::render::ListStateBlock {
                kind: if page.query.is_empty() && page.filter == SkillFilter::All {
                    crate::render::ListStateKind::Empty
                } else {
                    crate::render::ListStateKind::FilterEmpty
                },
                message: if page.query.is_empty() && page.filter == SkillFilter::All {
                    t.skills.empty
                } else {
                    t.skills.empty_filter
                },
                detail: None,
            },
            theme,
            area.width as usize,
        ));
    }
    let mut last_section: Option<SkillSection> = None;
    let mut hits: Vec<crate::render::RowHitSpec> = Vec::new();
    let mut focus: Option<usize> = None;
    for (position, &index) in indices.iter().enumerate() {
        let row = &state.skills_page.rows[index];
        if last_section != Some(row.section) {
            let count = indices
                .iter()
                .filter(|index| state.skills_page.rows[**index].section == row.section)
                .count();
            lines.push(crate::render::section_header_line(
                row.section.label(t),
                Some(count),
                None,
                theme,
                area.width as usize,
            ));
            last_section = Some(row.section);
        }
        let selected = position == state.skills_page.selected;
        let title = if row.section == SkillSection::Problem || row.advertised.is_empty() {
            safe(&row.name)
        } else {
            format!("/{}", safe(&row.advertised))
        };
        let tone = skill_section_tone(row.section);
        let mut description = String::new();
        if let Some(detail) = row.description.as_deref().or(row.detail.as_deref()) {
            description.push_str(&safe(detail));
        }
        if let Some(hint) = &row.argument_hint {
            if !description.is_empty() {
                description.push_str("  ");
            }
            description.push_str(&safe(hint));
        }
        if !row.user_invocable {
            if !description.is_empty() {
                description.push_str("  ");
            }
            description.push_str(t.skills.model_only);
        }
        if !row.toggleable && row.section != SkillSection::Problem {
            if !description.is_empty() {
                description.push_str("  ");
            }
            description.push_str(t.skills.trust_hint);
        }
        let mut list_row = crate::render::ListRow::new(title)
            .status(row.section.marker(), tone)
            .meta(scope_source(row, t))
            .tone(crate::render::RowTone::Default);
        if !description.is_empty() {
            list_row = list_row.description(description).description_tone(
                if row.section == SkillSection::Available {
                    crate::render::RowTone::Muted
                } else {
                    crate::render::RowTone::Warning
                },
            );
        }
        let paint = crate::render::row_paint(state, &row.id, selected, true);
        let row_lines = crate::render::list_row_lines(&list_row, paint, theme, area.width as usize);
        if selected {
            focus = Some(lines.len());
        }
        hits.push(crate::render::RowHitSpec {
            line: lines.len(),
            height: row_lines.len() as u16,
            key: row.id.clone(),
        });
        lines.extend(row_lines);
    }

    match focus {
        Some(focus) => {
            crate::render::render_list_rows_with_spans(
                frame,
                area,
                lines,
                &hits,
                focus,
                theme,
                &mut state.screen_rows,
            );
        }
        None => {
            state.screen_rows.clear();
            crate::render::render_scrolled(frame, area, state, lines);
        }
    }
}

fn skill_section_tone(section: SkillSection) -> crate::render::RowTone {
    match section {
        SkillSection::Available => crate::render::RowTone::Success,
        SkillSection::Disabled => crate::render::RowTone::Muted,
        SkillSection::Untrusted => crate::render::RowTone::Warning,
        SkillSection::Invalid | SkillSection::Problem => crate::render::RowTone::Error,
    }
}

fn scope_source(row: &SkillRow, t: &UiText) -> String {
    match (row.scope, row.source) {
        (Some(scope), Some(source)) => format!("{} · {}", scope_label(scope, t), source.label()),
        _ => String::new(),
    }
}

fn detail_line(lines: &mut Vec<Line<'static>>, label: &str, value: &str, theme: &Theme) {
    lines.push(Line::from(vec![
        Span::styled(
            format!("  {label}: "),
            Style::default().fg(theme.text.muted),
        ),
        Span::styled(safe(value), Style::default().fg(theme.text.secondary)),
    ]));
}

fn render_detail(frame: &mut Frame, area: Rect, state: &mut AppState) {
    let t = state.t();
    let theme = &state.theme;
    let Some(row) = selected_row(state).cloned() else {
        crate::render::render_scrolled(
            frame,
            area,
            state,
            vec![Line::from(Span::styled(
                t.skills.empty,
                Style::default().fg(theme.text.muted),
            ))],
        );
        return;
    };
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(Span::styled(
        t.skills.detail_title,
        Style::default()
            .fg(theme.accent.primary)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::default());
    lines.push(Line::from(vec![
        Span::styled("  ", Style::default()),
        Span::styled(
            format!("{} ", row.section.marker()),
            row_style(row.section, theme),
        ),
        Span::styled(
            if row.advertised.is_empty() {
                safe(&row.name)
            } else {
                format!("/{}", safe(&row.advertised))
            },
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ]));
    if let Some(description) = &row.description {
        detail_line(&mut lines, t.skills.description, description, theme);
    }
    detail_line(
        &mut lines,
        t.skills.status,
        section_status(row.section, t),
        theme,
    );
    if let Some(detail) = &row.detail {
        detail_line(&mut lines, t.skills.reason, detail, theme);
    }
    if row.scope.is_some() {
        detail_line(&mut lines, t.skills.location, &scope_source(&row, t), theme);
    }
    if let Some(location) = &row.location {
        detail_line(
            &mut lines,
            t.skills.path,
            &leveler_core::path::display_path(location),
            theme,
        );
    }
    if !row.canonical.is_empty() {
        detail_line(&mut lines, t.skills.canonical, &row.canonical, theme);
    }
    if !row.advertised.is_empty() {
        detail_line(
            &mut lines,
            t.skills.invoke,
            &format!("/{}", row.advertised),
            theme,
        );
    }
    if row.mentionable {
        detail_line(
            &mut lines,
            t.skills.mention,
            &format!("${}", row.name),
            theme,
        );
    }
    if let Some(hint) = &row.argument_hint {
        detail_line(&mut lines, t.skills.arguments, hint, theme);
    }
    detail_line(
        &mut lines,
        t.skills.user_invocation,
        if row.user_invocable {
            t.skills.yes
        } else {
            t.skills.no
        },
        theme,
    );
    detail_line(
        &mut lines,
        t.skills.model_invocation,
        if row.disable_model_invocation {
            t.skills.no
        } else {
            t.skills.yes
        },
        theme,
    );
    for shadowed in &row.shadowed {
        detail_line(&mut lines, t.skills.shadowed, shadowed, theme);
    }
    if row.section == SkillSection::Untrusted {
        detail_line(
            &mut lines,
            t.skills.next_step,
            t.skills.untrusted_hint,
            theme,
        );
    }
    if let Some(notice) = &state.skills_page.notice {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            format!("✓ {notice}"),
            Style::default().fg(theme.status.success),
        )));
    }
    crate::render::render_scrolled(frame, area, state, lines);
}
