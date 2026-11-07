//! `/resume-claude` and `/resume-codex`: continue a session that another
//! coding agent stored on this machine.
//!
//! The page owns only what the runtime discovered and the cursor. It never
//! reads the source store itself and never decides whether a session can be
//! imported: discovery, exact-id lookup and the import are the runtime's.
//! Importing creates a NEW native session; the source file stays read-only.

use leveler_client_protocol::{ClientCommand, NotificationLevel, UiForeignSession, UiForeignTool};

use crate::action::Effect;
use crate::screen::Screen;
use crate::state::{AppState, Notification};

/// The picker's last authoritative projection.
#[derive(Debug, Default)]
pub struct ForeignSessionsView {
    /// Which source the page is showing. `None` until a command opens it.
    pub tool: Option<UiForeignTool>,
    /// The runtime's last discovery answer, newest first.
    pub sessions: Vec<UiForeignSession>,
    pub selected: usize,
    /// Discovery is in flight. The page must not claim "nothing found" yet.
    pub loading: bool,
    /// Import is in flight. Enter must not start a second import for the same
    /// picker, which would create two native sessions for one source session.
    pub importing: bool,
    /// Discovery itself failed. Without this the page would render a failed
    /// scan as a real empty result.
    pub error: Option<String>,
}

impl ForeignSessionsView {
    pub fn selected_session(&self) -> Option<&UiForeignSession> {
        self.sessions.get(self.selected)
    }
}

/// A source id is a UUID; the header shows a stable short form while every
/// notice and the durable provenance keep the full id.
pub(crate) fn short_native_id(id: &str) -> &str {
    match id.char_indices().nth(8) {
        Some((end, _)) => &id[..end],
        None => id,
    }
}

/// Open one source's picker and ask the runtime what it can see.
pub(crate) fn open(state: &mut AppState, tool: UiForeignTool) -> Vec<Effect> {
    state.foreign_sessions = ForeignSessionsView {
        tool: Some(tool),
        loading: true,
        ..ForeignSessionsView::default()
    };
    state.active_screen = Screen::ForeignSessions;
    state.screen_scroll = 0;
    vec![Effect::Send(ClientCommand::ListForeignSessionsFor {
        requester_session_id: state.session_id.clone(),
        tool,
    })]
}

/// `/resume-claude [id]` / `/resume-codex [id]`: an id imports that exact
/// source session; an omitted id opens that source's picker.
pub(crate) fn resume(state: &mut AppState, command: &str, tool: UiForeignTool) -> Vec<Effect> {
    match command.split_whitespace().nth(1) {
        Some(id) => import(state, tool, id.to_string()),
        None => open(state, tool),
    }
}

/// Import one exact source id. The runtime answers with `ForeignSessionImported`
/// or `ForeignSessionImportFailed`; this only starts the read and tells the
/// person which source is being read.
pub(crate) fn import(state: &mut AppState, tool: UiForeignTool, native_id: String) -> Vec<Effect> {
    // One import per source session at a time: a second Enter while the first
    // is in flight must not create a second native session.
    if state.foreign_sessions.importing {
        return Vec::new();
    }
    state.foreign_sessions.importing = true;
    let message = state
        .t()
        .foreign_reading
        .replacen("{}", tool.display_name(), 1)
        .replacen("{}", &native_id, 1);
    state.notification = Some(Notification {
        level: NotificationLevel::Info,
        message,
    });
    vec![Effect::Send(ClientCommand::ImportForeignSession {
        requester_session_id: state.session_id.clone(),
        tool,
        native_id,
    })]
}

/// Import the row under the picker's cursor. No cursor means no subject.
pub(crate) fn import_selected(state: &mut AppState) -> Vec<Effect> {
    let Some(session) = state.foreign_sessions.selected_session().cloned() else {
        return Vec::new();
    };
    import(state, session.tool, session.native_id)
}

/// Move the picker's cursor, clamped to what the runtime reported.
pub(crate) fn move_selection(state: &mut AppState, delta: isize) {
    let len = state.foreign_sessions.sessions.len();
    if len == 0 {
        return;
    }
    let current = state.foreign_sessions.selected;
    state.foreign_sessions.selected = if delta < 0 {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        (current + delta as usize).min(len - 1)
    };
}
