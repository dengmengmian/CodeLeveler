//! Headless acceptance for `/tasks` and `/doctor`: the real reducer, the real
//! runtime-event projection and the real painted frame. The pages must open
//! while busy, paint only authoritative facts, keep a stable selection, return
//! one level on Esc, and ignore superseded asynchronous answers.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use leveler_client_protocol::schedule::{
    ScheduleActionWire, ScheduleGrantView, ScheduleRequest, ScheduleResponse, ScheduleTriggerWire,
    ScheduleView,
};
use leveler_client_protocol::{
    ClientCommand, CommandId, DiagnosticCategory, DiagnosticFinding, DiagnosticReport,
    DiagnosticSeverity, DiagnosticStatus, RuntimeEvent, RuntimeStatus, SessionId,
    UiGlobalTaskIndex, UiGlobalTaskSummary, UiTaskStatus, action_id, finding_id,
};
use leveler_tui::action::{Action, Effect};
use leveler_tui::reducer::reduce;
use leveler_tui::render::render;
use leveler_tui::screen::Screen;
use leveler_tui::state::{AppState, BackgroundTaskChrome, Boot};
use leveler_tui::theme::Theme;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use unicode_width::UnicodeWidthStr;

fn state() -> AppState {
    let mut state = AppState::new(
        Theme::no_color(),
        Boot {
            session_id: SessionId::new("s1"),
            user: "u".into(),
            version: "0".into(),
            show_welcome: false,
            draft_path: None,
            history_path: None,
            context_window: 0,
            locale: leveler_tui::Locale::Zh,
            untrusted_config: Vec::new(),
            model_notice: None,
            thinking: None,
        },
    );
    state.size = (100, 30);
    state
}

fn key(state: &mut AppState, code: KeyCode) -> Vec<Effect> {
    reduce(state, Action::Key(KeyEvent::new(code, KeyModifiers::NONE)))
}

fn submit(state: &mut AppState, command: &str) -> Vec<Effect> {
    state.composer.replace(command);
    key(state, KeyCode::Enter)
}

fn text(state: &mut AppState, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| render(frame, state)).unwrap();
    let buffer = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..buffer.area.height {
        let mut x = 0;
        while x < buffer.area.width {
            let symbol = buffer.cell((x, y)).map(|cell| cell.symbol()).unwrap_or(" ");
            out.push_str(symbol);
            x += UnicodeWidthStr::width(symbol).max(1) as u16;
        }
        out.push('\n');
    }
    out
}

fn query_of(effects: &[Effect], predicate: impl Fn(&ClientCommand) -> bool) -> Option<String> {
    effects.iter().find_map(|effect| match effect {
        Effect::Send(command) if predicate(command) => match command {
            ClientCommand::QueryGlobalTasks { query_id, .. } => Some(query_id.clone()),
            ClientCommand::QueryDiagnostics { query_id, .. } => Some(query_id.as_str().to_string()),
            _ => None,
        },
        _ => None,
    })
}

#[test]
fn tasks_opens_while_busy_and_sends_only_read_only_queries() {
    let mut state = state();
    state.status = RuntimeStatus::Busy;
    let effects = submit(&mut state, "/tasks");
    assert_eq!(state.active_screen, Screen::Tasks);
    assert_eq!(state.status, RuntimeStatus::Busy, "the turn is untouched");
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::Send(ClientCommand::QueryGlobalTasks { .. }))),
        "global session query: {effects:?}"
    );
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::Send(ClientCommand::ManageSchedule { .. }))),
        "schedule definitions query: {effects:?}"
    );
    assert!(
        !effects.iter().any(|effect| matches!(
            effect,
            Effect::Send(ClientCommand::SubmitMessage { .. })
                | Effect::Send(ClientCommand::SteerCurrentTurn { .. })
        )),
        "opening a page never submits or steers: {effects:?}"
    );
    key(&mut state, KeyCode::Esc);
    assert_eq!(state.active_screen, Screen::Conversation);
}

#[test]
fn tasks_paints_live_goal_child_and_background_rows_with_real_statuses() {
    let mut state = state();
    state.execution_goal = Some(leveler_client_protocol::UiExecutionTask {
        goal_id: "g1".into(),
        objective: "Implement payment service".into(),
        running: true,
        user_paused: false,
    });
    state.background_task_labels.insert(
        "bg1".into(),
        BackgroundTaskChrome::running("cargo test --workspace", 0),
    );
    state
        .team
        .children
        .push(leveler_tui::multi_agent::ChildAgentView {
            id: "c1".into(),
            nickname: "scout".into(),
            role: "explorer".into(),
            profile_id: None,
            agent_name: None,
            read_only: true,
            title: Some("Analyze database migrations".into()),
            purpose: "scan migrations".into(),
            status: leveler_tui::multi_agent::ChildStatus::Running,
            contribution: leveler_tui::multi_agent::Contribution::Pending,
            recent_step: None,
            input_tokens: 0,
            output_tokens: 0,
            stop: None,
            limit: None,
            started_elapsed_secs: 0,
            settled_elapsed_secs: None,
            detail: None,
            activity: Vec::new(),
        });
    state.tasks.selected = Some("goal:g1".into());
    let _ = submit(&mut state, "/tasks");

    let frame = text(&mut state, 100, 30);
    assert!(frame.contains("Implement payment service"), "{frame}");
    assert!(frame.contains("Analyze database migrations"), "{frame}");
    assert!(frame.contains("cargo test --workspace"), "{frame}");
    assert!(frame.contains("子 Agent"), "{frame}");
    assert!(frame.contains("后台任务"), "{frame}");
    assert!(frame.contains("运行中"), "{frame}");
    assert!(frame.contains("当前会话"), "{frame}");
}

#[test]
fn tasks_active_never_contains_a_finished_task_and_tab_reveals_it() {
    let mut state = state();
    let mut done = BackgroundTaskChrome::running("cargo build", 0);
    done.ok = Some(true);
    state.background_task_labels.insert("bg-done".into(), done);
    let _ = submit(&mut state, "/tasks");
    let frame = text(&mut state, 100, 30);
    assert!(!frame.contains("cargo build"), "{frame}");
    key(&mut state, KeyCode::Tab); // Active → Recent
    let frame = text(&mut state, 100, 30);
    assert!(frame.contains("cargo build"), "{frame}");
    assert!(frame.contains("完成"), "{frame}");
}

#[test]
fn tasks_schedule_definition_is_scheduled_and_a_run_is_separate() {
    let mut state = state();
    state.tasks.schedules = vec![schedule("nightly", true)];
    let _ = submit(&mut state, "/tasks");
    key(&mut state, KeyCode::Tab);
    key(&mut state, KeyCode::Tab); // Active → Recent → All
    let frame = text(&mut state, 100, 30);
    assert!(frame.contains("定义"), "{frame}");
    assert!(frame.contains("运行"), "{frame}");
    assert!(frame.contains("已计划"), "{frame}");
}

#[test]
fn tasks_enters_the_existing_activity_detail_and_returns_with_selection() {
    let mut state = state();
    state
        .background_task_labels
        .insert("bg1".into(), BackgroundTaskChrome::running("cargo test", 0));
    state.tasks.selected = Some("background:bg1".into());
    let _ = submit(&mut state, "/tasks");
    let effects = key(&mut state, KeyCode::Enter);
    assert!(effects.is_empty(), "opening a detail sends no command");
    assert_eq!(state.active_screen, Screen::Activity);
    assert_eq!(state.detail_return, Some(Screen::Tasks));
    key(&mut state, KeyCode::Esc);
    assert_eq!(state.active_screen, Screen::Tasks);
    assert_eq!(state.tasks.selected.as_deref(), Some("background:bg1"));
}

#[test]
fn entering_a_goal_opens_the_existing_goal_page_and_esc_returns() {
    let mut state = state();
    state.execution_goal = Some(leveler_client_protocol::UiExecutionTask {
        goal_id: "g1".into(),
        objective: "ship the payment service".into(),
        running: true,
        user_paused: false,
    });
    let _ = submit(&mut state, "/tasks");
    state.tasks.selected = Some("goal:g1".into());
    let effects = key(&mut state, KeyCode::Enter);
    assert!(effects.is_empty());
    assert_eq!(state.active_screen, Screen::Goal);
    key(&mut state, KeyCode::Esc);
    assert_eq!(state.active_screen, Screen::Tasks);
    assert_eq!(state.tasks.selected.as_deref(), Some("goal:g1"));
}

#[test]
fn a_session_change_clears_page_local_attribution() {
    let mut tasks_state = state();
    tasks_state.tasks.selected = Some("child:c1".into());
    tasks_state.tasks.section = leveler_tui::tasks::TasksSection::All;
    leveler_tui::tasks::session_changed(&mut tasks_state);
    assert_eq!(
        tasks_state.tasks.selected.as_deref(),
        Some("session:current")
    );

    let mut doctor_state = state();
    doctor_state.doctor.report = Some(report(DiagnosticStatus::Pass, "old"));
    doctor_state.doctor.selected = Some("config.providers".into());
    leveler_tui::doctor::session_changed(&mut doctor_state);
    assert!(doctor_state.doctor.report.is_none());
    assert!(doctor_state.doctor.selected.is_some());
}

#[test]
fn tasks_cancel_targets_only_the_selected_live_task() {
    let mut state = state();
    state
        .background_task_labels
        .insert("bg1".into(), BackgroundTaskChrome::running("cargo test", 0));
    state.background_task_labels.insert(
        "bg2".into(),
        BackgroundTaskChrome::running("cargo build", 0),
    );
    state.tasks.selected = Some("background:bg1".into());
    let _ = submit(&mut state, "/tasks");
    let effects = key(&mut state, KeyCode::Char('x'));
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::Send(ClientCommand::CancelBackgroundTask { task_id, .. })] if task_id == "bg1"
        ),
        "{effects:?}"
    );

    // A finished row cannot be cancelled through a stale selection.
    if let Some(chrome) = state.background_task_labels.get_mut("bg2") {
        chrome.ok = Some(true);
    }
    state.tasks.selected = Some("background:bg2".into());
    state.tasks.section = leveler_tui::tasks::TasksSection::All;
    let effects = key(&mut state, KeyCode::Char('x'));
    assert!(effects.is_empty(), "{effects:?}");
}

#[test]
fn tasks_global_sessions_are_snapshots_and_are_never_cancellable() {
    let mut state = state();
    let effects = submit(&mut state, "/tasks");
    let query = query_of(&effects, |command| {
        matches!(command, ClientCommand::QueryGlobalTasks { .. })
    })
    .expect("global task query");
    let requester = state.session_id.clone();
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::GlobalTasksLoaded {
            requester_session_id: requester.clone(),
            query_id: query,
            index: UiGlobalTaskIndex {
                tasks: vec![UiGlobalTaskSummary {
                    id: SessionId::new("other"),
                    source_id: "/tmp/other/sessions.db".into(),
                    title: "other workspace".into(),
                    primary_workspace: Some("/tmp/other".into()),
                    workspace_available: Some(true),
                    last_activity_at: "2026-11-17T00:00:00Z".into(),
                    archived_at: None,
                    status: UiTaskStatus::Running,
                    model: "deepseek".into(),
                }],
                source_errors: Vec::new(),
            },
        }),
    );
    state.tasks.section = leveler_tui::tasks::TasksSection::All;
    let frame = text(&mut state, 100, 30);
    assert!(frame.contains("other workspace"), "{frame}");
    state.tasks.selected = Some("session:/tmp/other/sessions.db:other".into());
    assert!(
        key(&mut state, KeyCode::Char('x')).is_empty(),
        "cross-session rows are read-only"
    );
}

#[test]
fn tasks_schedule_answer_belongs_to_this_page_only() {
    let mut state = state();
    let effects = submit(&mut state, "/tasks");
    let query = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::Send(ClientCommand::ManageSchedule {
                query_id, request, ..
            }) if matches!(**request, ScheduleRequest::List { .. }) => Some(query_id.clone()),
            _ => None,
        })
        .expect("schedule list query");
    let requester = state.session_id.clone();
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::ScheduleManaged {
            requester_session_id: requester,
            query_id: query,
            response: Box::new(ScheduleResponse::List {
                schedules: vec![schedule("nightly", false)],
            }),
        }),
    );
    assert_eq!(state.tasks.schedules.len(), 1);
    assert!(state.tasks.schedule_error.is_none());
}

#[test]
fn a_reconnect_re_reads_both_snapshot_sources_while_open() {
    let mut state = state();
    let _ = submit(&mut state, "/tasks");
    // Drain the initial queries so the reconnect's effects are unambiguous.
    state.tasks.index_query = None;
    state.tasks.schedule_query = None;
    let effects = reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::RuntimeReconnected {
            runtime_build: leveler_client_protocol::BuildIdentity::default(),
            runtime_restarted: Some(false),
        }),
    );
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::Send(ClientCommand::QueryGlobalTasks { .. }))),
        "{effects:?}"
    );
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::Send(ClientCommand::ManageSchedule { .. }))),
        "{effects:?}"
    );
}

#[test]
fn a_reconnect_re_reads_the_shared_checks_while_doctor_is_open() {
    let mut state = state();
    let _ = submit(&mut state, "/doctor");
    state.doctor.query = None;
    let effects = reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::RuntimeReconnected {
            runtime_build: leveler_client_protocol::BuildIdentity::default(),
            runtime_restarted: Some(false),
        }),
    );
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::Send(ClientCommand::QueryDiagnostics { .. }))),
        "{effects:?}"
    );
}

#[test]
fn schedule_terminal_runs_are_not_reported_as_running() {
    use leveler_client_protocol::schedule::ScheduleRunView;
    let mut state = state();
    let mut view = schedule("nightly", false);
    view.last_run = Some(ScheduleRunView {
        id: "run-1".into(),
        state: "failed".into(),
        scheduled_for: "2026-11-17T00:00:00Z".into(),
        started_at: "2026-11-17T00:00:01Z".into(),
        finished_at: Some("2026-11-17T00:00:02Z".into()),
        session_id: None,
        execution_id: None,
        exit_code: Some(7),
        error: None,
        provider: None,
        native_outcome: None,
        task_outcome: None,
        capability_state: None,
        permission_state: None,
        usage: None,
        cancel_requested_at: None,
    });
    state.tasks.schedules = vec![view];
    let rows = leveler_tui::tasks::all_rows(&state);
    let run = rows
        .iter()
        .find(|row| row.kind == leveler_tui::tasks::TaskKind::ScheduleRun)
        .expect("a run row");
    assert_eq!(run.status, leveler_tui::tasks::TaskStatus::Failed);
    let active = leveler_tui::tasks::visible_rows(&state);
    assert!(
        !active
            .iter()
            .any(|row| row.kind == leveler_tui::tasks::TaskKind::ScheduleRun),
        "{active:?}"
    );
    let definition = rows
        .iter()
        .find(|row| row.kind == leveler_tui::tasks::TaskKind::ScheduleDefinition)
        .unwrap();
    assert_eq!(definition.status, leveler_tui::tasks::TaskStatus::Scheduled);
}

#[test]
fn a_query_answered_twice_does_not_duplicate_rows() {
    let mut state = state();
    let effects = submit(&mut state, "/tasks");
    let query = query_of(&effects, |command| {
        matches!(command, ClientCommand::QueryGlobalTasks { .. })
    })
    .expect("global task query");
    let requester = state.session_id.clone();
    let index = || UiGlobalTaskIndex {
        tasks: vec![UiGlobalTaskSummary {
            id: SessionId::new("other"),
            source_id: "/tmp/db".into(),
            title: "other".into(),
            primary_workspace: None,
            workspace_available: None,
            last_activity_at: "t".into(),
            archived_at: None,
            status: UiTaskStatus::Unknown,
            model: String::new(),
        }],
        source_errors: Vec::new(),
    };
    for _ in 0..2 {
        reduce(
            &mut state,
            Action::Runtime(RuntimeEvent::GlobalTasksLoaded {
                requester_session_id: requester.clone(),
                query_id: query.clone(),
                index: index(),
            }),
        );
    }
    let rows: Vec<_> = leveler_tui::tasks::all_rows(&state)
        .into_iter()
        .filter(|row| row.session.as_ref().map(SessionId::as_str) == Some("other"))
        .collect();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].status, leveler_tui::tasks::TaskStatus::Unknown);
}

#[test]
fn an_answer_for_another_session_is_ignored() {
    let mut state = state();
    let effects = submit(&mut state, "/tasks");
    let query = query_of(&effects, |command| {
        matches!(command, ClientCommand::QueryGlobalTasks { .. })
    })
    .expect("global task query");
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::GlobalTasksLoaded {
            requester_session_id: SessionId::new("someone-else"),
            query_id: query,
            index: UiGlobalTaskIndex::default(),
        }),
    );
    assert!(state.tasks.index.is_none());
}

#[test]
fn doctor_opens_while_busy_with_local_facts_and_a_shared_query() {
    let mut state = state();
    state.status = RuntimeStatus::Busy;
    state.snapshot_version = Some(1);
    state.interrupted_tool_calls = vec![leveler_client_protocol::UiInterruptedToolCall {
        call_id: "c1".into(),
        tool: "run_command".into(),
        pending_approval: false,
        state: leveler_client_protocol::UiInterruptedCallState::Unknown,
    }];
    let effects = submit(&mut state, "/doctor");
    assert_eq!(state.active_screen, Screen::Doctor);
    assert_eq!(state.status, RuntimeStatus::Busy);
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::Send(ClientCommand::QueryDiagnostics { .. }))),
        "{effects:?}"
    );
    let recovered = leveler_tui::doctor::local_findings(&state)
        .into_iter()
        .find(|finding| finding.id == finding_id::RUNTIME_RECOVERY)
        .expect("recovery finding");
    assert_eq!(recovered.status, DiagnosticStatus::Warn);
    key(&mut state, KeyCode::Enter); // collapse the fact panel
    let frame = text(&mut state, 100, 50);
    assert!(frame.contains("运行时连接"), "{frame}");
    assert!(frame.contains("通过"), "{frame}");
    assert!(frame.contains("崩溃恢复窗口"), "{frame}");
    assert!(frame.contains("警告"), "{frame}");
    assert!(frame.contains("终端能力"), "{frame}");
}

#[test]
fn doctor_ignores_a_superseded_answer_and_accepts_the_newest() {
    let mut state = state();
    let effects = submit(&mut state, "/doctor");
    let first = query_of(&effects, |command| {
        matches!(command, ClientCommand::QueryDiagnostics { .. })
    })
    .expect("first diagnostics query");
    let newer = key(&mut state, KeyCode::Char('r'));
    let second = query_of(&newer, |command| {
        matches!(command, ClientCommand::QueryDiagnostics { .. })
    })
    .expect("refresh query");
    assert_ne!(first, second);
    let requester = state.session_id.clone();

    let stale = report(DiagnosticStatus::Fail, "stale failure");
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::DiagnosticsLoaded {
            requester_session_id: requester.clone(),
            query_id: CommandId::new(first),
            report: Box::new(stale),
        }),
    );
    assert!(state.doctor.report.is_none(), "stale answer dropped");

    let fresh = report(DiagnosticStatus::Warn, "fresh warning");
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::DiagnosticsLoaded {
            requester_session_id: requester,
            query_id: CommandId::new(second),
            report: Box::new(fresh),
        }),
    );
    key(&mut state, KeyCode::Enter);
    let frame = text(&mut state, 100, 50);
    assert!(frame.contains("Provider 配置"), "{frame}");
    assert!(frame.contains("警告"), "{frame}");
    assert!(!frame.contains("stale failure"), "{frame}");
}

#[test]
fn doctor_shows_not_checked_for_mcp_instead_of_a_pass() {
    let mut state = state();
    let effects = submit(&mut state, "/doctor");
    let query = query_of(&effects, |command| {
        matches!(command, ClientCommand::QueryDiagnostics { .. })
    })
    .expect("diagnostics query");
    let requester = state.session_id.clone();
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::DiagnosticsLoaded {
            requester_session_id: requester,
            query_id: CommandId::new(query),
            report: Box::new(DiagnosticReport {
                generated_at: "2026-11-17T00:00:00Z".into(),
                findings: vec![DiagnosticFinding {
                    id: finding_id::MCP_CONNECTION.into(),
                    subject: None,
                    category: DiagnosticCategory::Mcp,
                    severity: DiagnosticSeverity::Info,
                    status: DiagnosticStatus::NotChecked,
                    evidence: vec!["live connection state is not exposed".into()],
                    action: None,
                    checked_at: "2026-11-17T00:00:00Z".into(),
                }],
            }),
        }),
    );
    key(&mut state, KeyCode::Enter);
    let frame = text(&mut state, 100, 50);
    assert!(frame.contains("MCP 连接"), "{frame}");
    assert!(frame.contains("未检查"), "{frame}");
    // The MCP overview row must not claim PASS.
    let mcp_line = frame
        .lines()
        .find(|line| line.contains("MCP"))
        .unwrap_or_default();
    assert!(!mcp_line.contains("通过"), "{frame}");
}

#[test]
fn doctor_never_claims_pass_for_unknown_status() {
    let mut state = state();
    let effects = submit(&mut state, "/doctor");
    let query = query_of(&effects, |command| {
        matches!(command, ClientCommand::QueryDiagnostics { .. })
    })
    .expect("diagnostics query");
    let requester = state.session_id.clone();
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::DiagnosticsLoaded {
            requester_session_id: requester,
            query_id: CommandId::new(query),
            report: Box::new(DiagnosticReport {
                generated_at: "2026-11-17T00:00:00Z".into(),
                findings: vec![DiagnosticFinding {
                    id: finding_id::SANDBOX_CAPABILITY.into(),
                    subject: None,
                    category: DiagnosticCategory::Sandbox,
                    severity: DiagnosticSeverity::Info,
                    status: DiagnosticStatus::Unknown,
                    evidence: vec!["capability probe returned no answer".into()],
                    action: None,
                    checked_at: "t".into(),
                }],
            }),
        }),
    );
    key(&mut state, KeyCode::Enter);
    let sandbox_line = text(&mut state, 100, 50)
        .lines()
        .find(|line| line.contains("沙箱"))
        .unwrap_or_default()
        .to_string();
    assert!(!sandbox_line.contains("通过"), "{sandbox_line}");
    assert!(sandbox_line.contains("未知"), "{sandbox_line}");
}

fn report(status: DiagnosticStatus, evidence: &str) -> DiagnosticReport {
    DiagnosticReport {
        generated_at: "2026-11-17T00:00:00Z".into(),
        findings: vec![DiagnosticFinding {
            id: finding_id::CONFIG_PROVIDERS.into(),
            subject: None,
            category: DiagnosticCategory::Configuration,
            severity: if status == DiagnosticStatus::Pass {
                DiagnosticSeverity::Info
            } else {
                DiagnosticSeverity::Warning
            },
            status,
            evidence: vec![evidence.into()],
            action: Some(action_id::FIX_CONFIG.into()),
            checked_at: "2026-11-17T00:00:00Z".into(),
        }],
    }
}

fn schedule(id: &str, running: bool) -> ScheduleView {
    ScheduleView {
        id: id.into(),
        name: format!("nightly {id}"),
        description: "runs the nightly job".into(),
        action_kind: "shell".into(),
        action: ScheduleActionWire::Shell {
            command: "echo hi".into(),
        },
        workspace: "/tmp/work".into(),
        trigger: ScheduleTriggerWire::Daily {
            time: "09:00".into(),
            timezone: None,
        },
        trigger_text: "Every day at 09:00".into(),
        timezone: None,
        enabled: true,
        running,
        run_state: None,
        next_run_at: None,
        last_run: None,
        needs_attention: false,
        attention_reason: None,
        session_policy: "continue".into(),
        overlap: "skip-if-running".into(),
        mode: "assisted".into(),
        model: None,
        timeout_secs: 600,
        misfire: "skip".into(),
        misfire_grace_secs: 0,
        session_id: None,
        grant: ScheduleGrantView {
            granted: true,
            external_side_effects: false,
            granted_at: None,
            requires_regrant: false,
        },
        created_at: "2026-11-01T00:00:00Z".into(),
        revision: "rev-1".into(),
    }
}
