use crossterm::event::{KeyCode, KeyEvent};

use leveler_client_protocol::ClientCommand;

use crate::action::Effect;
use crate::screen::Screen;
use crate::state::AppState;

fn page_size(state: &AppState) -> usize {
    // Rough page: the screen viewport minus header/status/composer chrome.
    (state.size.1 as usize).saturating_sub(4).max(1)
}

/// Scroll keys shared by the full-screen views' content panes.
fn scroll_screen_key(state: &mut AppState, key: &KeyEvent, line_keys: bool) -> bool {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') if line_keys => {
            state.screen_scroll = state.screen_scroll.saturating_sub(1);
            true
        }
        KeyCode::Down | KeyCode::Char('j') if line_keys => {
            state.screen_scroll = state.screen_scroll.saturating_add(1);
            true
        }
        KeyCode::PageUp => {
            state.screen_scroll = state.screen_scroll.saturating_sub(page_size(state));
            true
        }
        KeyCode::PageDown => {
            state.screen_scroll = state.screen_scroll.saturating_add(page_size(state));
            true
        }
        _ => false,
    }
}

/// Move a list page's cursor by `delta` rows, clamped to the list. Keyboard
/// and wheel share it, so a page cannot have two different ideas of where its
/// own list ends.
pub(super) fn move_list_cursor(state: &mut AppState, delta: isize) {
    let len = match state.active_screen {
        Screen::Sessions => state.sessions.len(),
        Screen::Diff => state.diff.as_ref().map(|d| d.files.len()).unwrap_or(0),
        Screen::Tools => state
            .transcript
            .tool_calls()
            .into_iter()
            .filter(|b| state.tools_screen.filter.matches(b))
            .count(),
        _ => return,
    };
    if len == 0 {
        return;
    }
    let current = match state.active_screen {
        Screen::Sessions => state.sessions_selected,
        Screen::Diff => state.diff_selected,
        Screen::Tools => state.tools_screen.selected,
        _ => return,
    };
    let next = if delta < 0 {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        (current + delta as usize).min(len - 1)
    };
    match state.active_screen {
        Screen::Sessions => state.sessions_selected = next,
        Screen::Diff => state.diff_selected = next,
        Screen::Tools => state.tools_screen.selected = next,
        _ => {}
    }
}

/// One wheel notch over a page. A page whose body is a cursor over rows
/// scrolls by moving that cursor — the page's own way of bringing a row into
/// view; a page whose body is a document moves its own scroll offset. The
/// lists move one row per notch and the documents three lines, which is the
/// same split the jobs list already uses between its list and its log.
pub(super) fn wheel_page(state: &mut AppState, notches: isize) {
    match state.active_screen {
        Screen::Sessions | Screen::Diff | Screen::Tools => move_list_cursor(state, notches),
        Screen::Team => crate::multi_agent::select_team_delta(state, notches),
        Screen::Issue => crate::issue::move_selection(state, notches),
        Screen::Trace => state.trace.move_sel(notches),
        Screen::Context => crate::context::move_sel(&mut state.context, notches),
        Screen::Clean => state.clean.move_selection(notches),
        Screen::Help => {
            let t = state.t();
            state.help.move_selection(notches, t);
        }
        // Everything else (Plan, Goal, Doctor, Shell, Remote) is a document
        // under a `screen_scroll` offset.
        _ => {
            let lines = (notches * 3).unsigned_abs();
            state.screen_scroll = if notches < 0 {
                state.screen_scroll.saturating_sub(lines)
            } else {
                state.screen_scroll.saturating_add(lines)
            };
        }
    }
}

/// A click landed on a page row the page itself published.
///
/// Selecting is the only thing a row click means on these pages: what a
/// SECOND click or an Enter does is the page's Enter binding, which the
/// keyboard already owns. A row whose key does not parse back into an index
/// selects nothing — the page never guesses which row the user meant.
pub(super) fn select_row(state: &mut AppState, key: &str) -> Vec<Effect> {
    match state.active_screen {
        // The child list is keyed by stable identity, not by position.
        Screen::Team => state.team_selected = Some(key.to_string()),
        Screen::Sessions => {
            if let Ok(index) = key.parse::<usize>() {
                state.sessions_selected = index;
            }
        }
        Screen::Diff => {
            if let Ok(index) = key.parse::<usize>() {
                state.diff_selected = index;
            }
        }
        Screen::Tools => {
            if let Ok(index) = key.parse::<usize>() {
                state.tools_screen.selected = index;
                state.screen_scroll = 0;
            }
        }
        Screen::Trace => {
            if let Ok(index) = key.parse::<usize>() {
                state.trace.selected = index;
            }
        }
        Screen::Context => {
            if let Ok(index) = key.parse::<usize>() {
                state.context.selected = index;
            }
        }
        Screen::Clean => {
            if let Ok(index) = key.parse::<usize>() {
                state.clean.selected = index;
            }
        }
        Screen::Help => {
            if let Ok(index) = key.parse::<usize>() {
                state.help.select(index, state.t());
            }
        }
        _ => {}
    }
    Vec::new()
}

/// Handle keys for a full-screen view (Tools today). Esc returns to the
/// conversation.
pub(super) fn handle_screen_key(state: &mut AppState, key: KeyEvent) -> Vec<Effect> {
    match state.active_screen {
        // The one screen where a keystroke is a security decision, so only an
        // explicit y/n counts and Esc simply leaves without answering.
        Screen::Remote => {
            let waiting = state.remote.as_ref().is_some_and(|r| r.pending.is_some());
            match key.code {
                KeyCode::Esc => return back(state),
                KeyCode::Char('y') | KeyCode::Char('Y') if waiting => {
                    return vec![Effect::AnswerPairing { accept: true }];
                }
                KeyCode::Char('n') | KeyCode::Char('N') if waiting => {
                    return vec![Effect::AnswerPairing { accept: false }];
                }
                _ => {
                    scroll_screen_key(state, &key, true);
                }
            }
        }
        Screen::Tools => {
            match key.code {
                KeyCode::Esc => return back(state),
                KeyCode::Up | KeyCode::Char('k') => {
                    move_list_cursor(state, -1);
                    state.screen_scroll = 0;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    move_list_cursor(state, 1);
                    state.screen_scroll = 0;
                }
                KeyCode::Tab | KeyCode::Char('f') => {
                    state.tools_screen.cycle_filter();
                    state.screen_scroll = 0;
                }
                // PageUp/PageDown scroll the detail pane.
                _ => {
                    scroll_screen_key(state, &key, false);
                }
            }
        }
        Screen::Diff => {
            match key.code {
                KeyCode::Esc => return back(state),
                KeyCode::Up | KeyCode::Char('k') => {
                    move_list_cursor(state, -1);
                    state.screen_scroll = 0;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    move_list_cursor(state, 1);
                    state.screen_scroll = 0;
                }
                // PageUp/PageDown scroll the patch pane.
                _ => {
                    scroll_screen_key(state, &key, false);
                }
            }
        }
        Screen::Sessions => {
            match key.code {
                KeyCode::Esc => return back(state),
                KeyCode::Up | KeyCode::Char('k') => move_list_cursor(state, -1),
                KeyCode::Down | KeyCode::Char('j') => move_list_cursor(state, 1),
                KeyCode::Enter => {
                    if let Some(s) = state.sessions.get(state.sessions_selected) {
                        let id = s.id.clone();
                        let requester_session_id = state.session_id.clone();
                        close_screen(state);
                        return vec![Effect::Send(ClientCommand::OpenSessionFor {
                            requester_session_id,
                            session_id: id,
                        })];
                    }
                }
                KeyCode::Char('d') => {
                    // Deleting a session cannot be undone and the runtime keeps
                    // no backup, so `d` only opens the confirmation. The actual
                    // delete is sent from the confirm overlay's explicit choice.
                    if state.sessions.get(state.sessions_selected).is_some() {
                        super::overlay_keys::open_session_delete_confirm(
                            state,
                            state.sessions_selected,
                        );
                    }
                }
                KeyCode::Char('t') => {
                    if let Some(s) = state.sessions.get(state.sessions_selected) {
                        return open_trace(
                            state,
                            Some(s.id.as_str().to_string()),
                            Screen::Sessions,
                        );
                    }
                }
                _ => {}
            }
        }
        Screen::Activity => match key.code {
            KeyCode::Esc => return back(state),
            // `x` stops exactly the open activity; its parent turn keeps
            // running. Esc backs out without stopping anything. Both cases go
            // through the runtime's own cancellation path — never a PID.
            KeyCode::Char('x') | KeyCode::Char('X') => match &state.activity_open {
                Some(crate::activity::ActivityId::Child(id))
                    if state.team.children.iter().any(|child| {
                        child.id == *id
                            && matches!(
                                child.status,
                                crate::multi_agent::ChildStatus::Running
                                    | crate::multi_agent::ChildStatus::Waiting
                            )
                    }) =>
                {
                    return vec![Effect::Send(
                        leveler_client_protocol::ClientCommand::CancelChild {
                            session_id: state.session_id.clone(),
                            child_id: id.clone(),
                        },
                    )];
                }
                Some(crate::activity::ActivityId::Background(task_id)) => {
                    return cancel_background_task(state, task_id);
                }
                _ => {}
            },
            // Follow the newest output until the user scrolls back; reaching
            // the bottom resumes. Home/End and g/G jump to the ends.
            KeyCode::Up | KeyCode::Char('k') => crate::activity::scroll_lines(state, -1),
            KeyCode::Down | KeyCode::Char('j') => crate::activity::scroll_lines(state, 1),
            KeyCode::PageUp => crate::activity::scroll_page(state, -1),
            KeyCode::PageDown => crate::activity::scroll_page(state, 1),
            KeyCode::Home | KeyCode::Char('g') => crate::activity::to_top(state),
            KeyCode::End | KeyCode::Char('G') => crate::activity::to_bottom(state),
            _ => {}
        },
        Screen::Team => match key.code {
            KeyCode::Esc => return back(state),
            KeyCode::Up | KeyCode::Char('k') => crate::multi_agent::select_team_delta(state, -1),
            KeyCode::Down | KeyCode::Char('j') => crate::multi_agent::select_team_delta(state, 1),
            KeyCode::Enter => open_selected_child(state),
            // `x` stops exactly the selected running child, through the
            // runtime's own cancellation — never a PID.
            KeyCode::Char('x') | KeyCode::Char('X') => {
                if let Some(command) = selected_child_cancel(state) {
                    return vec![Effect::Send(command)];
                }
            }
            _ => {}
        },
        Screen::ActivityList => {
            use crate::activity::BackgroundPane;
            match key.code {
                KeyCode::Esc => return back(state),
                KeyCode::Tab | KeyCode::BackTab => crate::activity::toggle_background_focus(state),
                KeyCode::Up | KeyCode::Char('k') => match state.background_list_focus {
                    BackgroundPane::List => crate::activity::move_list_selection(state, -1),
                    BackgroundPane::Output => crate::activity::scroll_selected_output(state, -1),
                },
                KeyCode::Down | KeyCode::Char('j') => match state.background_list_focus {
                    BackgroundPane::List => crate::activity::move_list_selection(state, 1),
                    BackgroundPane::Output => crate::activity::scroll_selected_output(state, 1),
                },
                KeyCode::PageUp => crate::activity::page_selected_output(state, -1),
                KeyCode::PageDown => crate::activity::page_selected_output(state, 1),
                KeyCode::Home | KeyCode::Char('g') => crate::activity::top_selected_output(state),
                KeyCode::End | KeyCode::Char('G') => crate::activity::bottom_selected_output(state),
                // Enter takes the user into the selected task's log; Tab moves
                // back. The page never leaves this screen on its own.
                KeyCode::Enter => crate::activity::focus_background_output(state),
                // `x` stops exactly the selected running task, through the
                // runtime's own cancellation path. A terminal row advertises
                // nothing to stop.
                KeyCode::Char('x') | KeyCode::Char('X') => {
                    let Some(task_id) = crate::activity::selected_list_task(state) else {
                        return Vec::new();
                    };
                    return cancel_background_task(state, &task_id);
                }
                _ => {}
            }
        }
        Screen::Issue => match key.code {
            // One level per press: an open row collapses first, and only the
            // next Esc leaves the page.
            KeyCode::Esc => return back(state),
            KeyCode::Up | KeyCode::Char('k') => crate::issue::move_selection(state, -1),
            KeyCode::Down | KeyCode::Char('j') => crate::issue::move_selection(state, 1),
            // Enter is the same interaction as clicking the row: select and
            // expand/collapse in place. The task's own page stays behind the
            // expanded row's explicit detail entry, so Enter never jumps.
            KeyCode::Enter => crate::issue::toggle_expanded(state),
            // The detail entry is a control the page paints, so the keyboard
            // reaches it too: `→` enters the selected expanded row's detail and
            // `←` collapses it — the same one-level pair the header's `←`
            // glyph and Esc run. A row with no painted entry has no binding.
            KeyCode::Right => {
                let entry = crate::issue::selected_issue(state).filter(|issue| {
                    issue.kind == crate::issue::IssueKind::BackgroundFailure
                        && crate::issue::is_expanded(state, &issue.key)
                });
                if let Some(issue) = entry {
                    return open_issue_detail(state, &issue.key);
                }
            }
            KeyCode::Left => {
                crate::issue::collapse_selected(state);
            }
            _ => {
                scroll_screen_key(state, &key, false);
            }
        },
        Screen::Plan => {
            if key.code == KeyCode::Esc {
                return back(state);
            } else {
                scroll_screen_key(state, &key, true);
            }
        }
        Screen::Goal => {
            if key.code == KeyCode::Esc {
                return back(state);
            } else {
                scroll_screen_key(state, &key, true);
            }
        }
        Screen::Help => {
            if key.code == KeyCode::Esc {
                return back(state);
            } else {
                let page = page_size(state);
                let t = state.t();
                state.help.handle_key(key, t, page);
            }
        }
        Screen::Doctor => {
            if key.code == KeyCode::Esc {
                return back(state);
            } else {
                scroll_screen_key(state, &key, true);
            }
        }
        Screen::Trace => {
            use crate::observability::TraceTab;
            match key.code {
                KeyCode::Esc if state.trace.inspect => state.trace.inspect = false,
                KeyCode::Esc => return back(state),
                KeyCode::Left | KeyCode::BackTab => {
                    state.trace.tab = state.trace.tab.prev();
                    state.screen_scroll = 0;
                }
                KeyCode::Right | KeyCode::Tab => {
                    state.trace.tab = state.trace.tab.next();
                    state.screen_scroll = 0;
                }
                KeyCode::Char(c) if TraceTab::from_digit(c).is_some() => {
                    state.trace.tab = TraceTab::from_digit(c).unwrap();
                    state.screen_scroll = 0;
                }
                KeyCode::Up | KeyCode::Char('k') => state.trace.move_sel(-1),
                KeyCode::Down | KeyCode::Char('j') => state.trace.move_sel(1),
                KeyCode::Enter => {
                    state.trace.inspect = !state.trace.inspect;
                    if let Some(row) = state.trace.selected_row() {
                        let seq = row.sequence;
                        let sid = state
                            .trace
                            .loaded
                            .as_ref()
                            .map(|l| l.session.session_id.clone())
                            .unwrap_or_else(|| state.session_id.clone());
                        return vec![Effect::Send(crate::observability::issue_query(
                            &mut state.trace,
                            sid,
                            Some(seq),
                            20,
                            20,
                        ))];
                    }
                }
                KeyCode::Char('f') => {
                    state.trace.filter = state.trace.filter.next();
                    state.trace.clamp();
                }
                KeyCode::Char('r') => {
                    let sid = state
                        .trace
                        .loaded
                        .as_ref()
                        .map(|l| l.session.session_id.clone())
                        .unwrap_or_else(|| state.session_id.clone());
                    return vec![Effect::Send(crate::observability::issue_query(
                        &mut state.trace,
                        sid,
                        None,
                        0,
                        80,
                    ))];
                }
                _ => {
                    scroll_screen_key(state, &key, true);
                }
            }
        }
        Screen::Context => match key.code {
            KeyCode::Esc => return back(state),
            KeyCode::Up | KeyCode::Char('k') => crate::context::move_sel(&mut state.context, -1),
            KeyCode::Down | KeyCode::Char('j') => crate::context::move_sel(&mut state.context, 1),
            KeyCode::Enter => crate::context::toggle_selected(&mut state.context),
            _ => {
                scroll_screen_key(state, &key, true);
            }
        },
        Screen::Clean => match key.code {
            // Esc leaves the page. It never cancels foreground Agent work, and
            // a cleanup already running keeps running: the page owns no work
            // lifecycle, only a view over the host's plan.
            KeyCode::Esc => return back(state),
            KeyCode::Up | KeyCode::Char('k') => state.clean.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => state.clean.move_selection(1),
            KeyCode::Enter => match state.clean.selected_action() {
                Some(crate::clean::CleanAction::CleanSafe) => {
                    state.clean.begin_clean();
                    return vec![Effect::RunCleanSafe];
                }
                Some(crate::clean::CleanAction::Details) => {
                    state.clean.details = !state.clean.details;
                    state.clean.selected = 0;
                }
                Some(crate::clean::CleanAction::Close) => close_screen(state),
                None => {}
            },
            _ => {
                scroll_screen_key(state, &key, true);
            }
        },
        Screen::Conversation => {}
    }
    Vec::new()
}

/// One level back — the single rule Esc and the header's `←` glyph share.
///
/// Each page owns exactly one level of "back" and the pages chain through the
/// opener's [`crate::state::AppState::detail_return`], never by each page
/// hardcoding its parent: a detail opened from the issue page returns there,
/// a detail opened from the jobs list returns to the list. A future detail
/// page inherits the rule by publishing its `←` hit and reading the same
/// field — there is no second navigation system.
pub(super) fn back(state: &mut AppState) -> Vec<Effect> {
    match state.active_screen {
        // An open issue row is the page's innermost level; the detail page
        // returns to whatever opened it.
        Screen::Issue => {
            if !crate::issue::collapse_selected(state) {
                close_screen(state);
            }
        }
        Screen::Activity => crate::activity::close(state),
        Screen::ActivityList => {
            // Two levels of back: leave the log first, then the page.
            if state.background_list_focus == crate::activity::BackgroundPane::Output {
                crate::activity::focus_background_list(state);
            } else {
                crate::activity::close_background_list(state);
            }
        }
        // The child list clears its own selection on the way out.
        Screen::Team => close_team(state),
        // A page that was opened FROM another page returns there: the opener
        // recorded itself, so the page never guesses its parent. Pages with no
        // opener (the ones the workbench, a slash command or the state strip
        // opens) have no target and leave for the conversation.
        _ => match state.detail_return.take() {
            Some(parent) => {
                state.active_screen = parent;
                state.screen_scroll = 0;
            }
            None => close_screen(state),
        },
    }
    Vec::new()
}

/// Enter the task detail a selected issue row advertises.
///
/// This is the ONE action behind both the mouse's click on the row's
/// `查看任务详情 ↗` entry and the keyboard's `→`, so the two inputs can never
/// diverge. The opener records where the detail returns to — the detail itself
/// never guesses its parent. (`activity::open` resets the field, so the record
/// must come after it.)
pub(super) fn open_issue_detail(state: &mut AppState, key: &str) -> Vec<Effect> {
    let effects = crate::activity::open(
        state,
        crate::activity::ActivityId::Background(key.to_string()),
    );
    state.detail_return = Some(Screen::Issue);
    effects
}

/// Keyboard and mouse share the same live-state check and runtime command.
/// Sending the intent never manufactures a terminal state in the client.
pub(super) fn cancel_background_task(state: &AppState, task_id: &str) -> Vec<Effect> {
    if !state
        .background_task_labels
        .get(task_id)
        .is_some_and(|task| task.is_running())
    {
        return Vec::new();
    }
    vec![Effect::Send(ClientCommand::CancelBackgroundTask {
        session_id: state.session_id.clone(),
        task_id: task_id.to_string(),
    })]
}

/// Open the Issue page with its first active issue selected, acknowledging the
/// failures this visit actually shows.
///
/// The acknowledgement is a snapshot taken here, before the page is painted:
/// a failure that arrives while the page is open was not part of what the user
/// came to see and stays unread (it still reminds when the page is left). The
/// failed turn is never acknowledged — it is a persistent fault, and only the
/// runtime's own status leaving `Error` clears it. Closing the page clears
/// nothing: records, exit codes and output all stay.
pub(super) fn open_issue_screen(state: &mut AppState) {
    if crate::issue::issue_summary(state).is_empty() {
        return;
    }
    crate::issue::ensure_selection(state);
    crate::activity::acknowledge_failures(state);
    state.active_screen = crate::screen::Screen::Issue;
    state.screen_scroll = 0;
    // Forget where the cursor sat on the previous visit, so the first frame
    // follows it: reopening the page on a row below the fold must show it.
    state.issue_selected_line = None;
}

pub(super) fn open_plan_screen(state: &mut AppState) {
    if state
        .plan
        .as_ref()
        .is_some_and(crate::workbench::plan_panel_should_show)
    {
        state.active_screen = Screen::Plan;
        state.screen_scroll = 0;
    }
}

pub(super) fn open_goal_screen(state: &mut AppState) {
    if crate::active_goal::has_visible_goal(state) {
        state.active_screen = Screen::Goal;
        state.screen_scroll = 0;
    }
}

/// Return to the conversation, dropping the view's scroll offset.
/// Open the Team page with the first child selected, so Enter has a subject.
pub(super) fn open_team(state: &mut AppState) -> Vec<Effect> {
    state.active_screen = crate::screen::Screen::Team;
    state.team_selected = state.team.children.first().map(|child| child.id.clone());
    Vec::new()
}

/// Leave the Team page without cancelling anything.
fn close_team(state: &mut AppState) {
    state.active_screen = crate::screen::Screen::Conversation;
    state.team_selected = None;
    state.screen_rows.clear();
}

/// The child the Team page has selected, if it still exists.
fn selected_child(state: &AppState) -> Option<&crate::multi_agent::ChildAgentView> {
    state
        .team_selected
        .as_deref()
        .and_then(|id| state.team.children.iter().find(|child| child.id == id))
}

/// Open the selected child's Activity detail. Enter is observational: the work
/// keeps running and Esc comes back.
/// Open the selected child's Activity detail. Enter is observational: the work
/// keeps running and Esc comes back — to the Team page that opened it.
fn open_selected_child(state: &mut AppState) {
    let Some(id) = selected_child(state).map(|child| child.id.clone()) else {
        return;
    };
    state.activity_open = Some(crate::activity::ActivityId::Child(id));
    state.detail_return = Some(Screen::Team);
    state.active_screen = crate::screen::Screen::Activity;
}

/// Ask the runtime to stop the selected child, when it is still running.
fn selected_child_cancel(state: &AppState) -> Option<leveler_client_protocol::ClientCommand> {
    let child = selected_child(state)?;
    if !matches!(
        child.status,
        crate::multi_agent::ChildStatus::Running | crate::multi_agent::ChildStatus::Waiting
    ) {
        return None;
    }
    Some(leveler_client_protocol::ClientCommand::CancelChild {
        session_id: state.session_id.clone(),
        child_id: child.id.clone(),
    })
}

fn close_screen(state: &mut AppState) {
    state.active_screen = Screen::Conversation;
    state.screen_scroll = 0;
}

/// Toggle a screen on/off (returns to the conversation if already active).
pub(super) fn toggle_screen(state: &mut AppState, screen: Screen) -> Vec<Effect> {
    state.screen_scroll = 0;
    state.active_screen = if state.active_screen == screen {
        Screen::Conversation
    } else {
        screen
    };
    Vec::new()
}

/// Open the Diff screen and request a fresh diff (with patches) from the runtime.
pub(super) fn open_diff_screen(state: &mut AppState) -> Vec<Effect> {
    if state.active_screen == Screen::Diff {
        state.active_screen = Screen::Conversation;
        return Vec::new();
    }
    state.active_screen = Screen::Diff;
    state.diff_selected = 0;
    // The previous diff may belong to another moment; do not show it as the
    // answer while the fresh one is computed.
    state.diff = None;
    state.diff_error = None;
    state.diff_pending = true;
    let query_id = leveler_client_protocol::CommandId::generate();
    state.diff_query = Some(query_id.clone());
    vec![Effect::Send(ClientCommand::RequestDiff {
        session_id: state.session_id.clone(),
        query_id: Some(query_id),
    })]
}

/// Open the observatory for `session_id` (current session when `None`).
///
/// `opener` is the page the observatory was opened FROM — the sessions list
/// when a row's `t` opened it, [`Screen::Conversation`] when a slash command
/// did. It becomes the page's back target, so Esc returns to the list rather
/// than skipping it.
pub(super) fn open_trace(
    state: &mut AppState,
    session_id: Option<String>,
    opener: Screen,
) -> Vec<Effect> {
    let session_id = session_id
        .map(leveler_client_protocol::SessionId::new)
        .unwrap_or_else(|| state.session_id.clone());
    state.active_screen = Screen::Trace;
    state.detail_return = (opener != Screen::Conversation).then_some(opener);
    state.screen_scroll = 0;
    state.trace.inspect = false;
    state.trace.selected = 0;
    vec![Effect::Send(crate::observability::issue_query(
        &mut state.trace,
        session_id,
        None,
        0,
        80,
    ))]
}

/// Open the Context Inspector and ask the runtime for the current accounting.
pub(super) fn open_context(state: &mut AppState) -> Vec<Effect> {
    if state.active_screen == Screen::Context {
        state.active_screen = Screen::Conversation;
        return Vec::new();
    }
    state.active_screen = Screen::Context;
    state.screen_scroll = 0;
    state.context.selected = 0;
    vec![Effect::Send(crate::context::issue_query(state))]
}

/// Open `/clean` and start a fresh scan. The scan runs off-thread in the event
/// loop; this only enters the page and asks for it.
pub(super) fn open_clean(state: &mut AppState) -> Vec<Effect> {
    state.active_screen = Screen::Clean;
    state.screen_scroll = 0;
    state.clean.begin_scan();
    vec![Effect::StartCleanScan]
}

/// Open the Sessions screen and refresh the list (spec §52).
pub(super) fn open_sessions_screen(state: &mut AppState) -> Vec<Effect> {
    if state.active_screen == Screen::Sessions {
        state.active_screen = Screen::Conversation;
        return Vec::new();
    }
    state.active_screen = Screen::Sessions;
    state.sessions_selected = 0;
    vec![Effect::Send(ClientCommand::RequestSessionListFor {
        requester_session_id: state.session_id.clone(),
    })]
}
