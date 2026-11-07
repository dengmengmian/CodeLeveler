//! The Context & Usage page: ONE page, two entry points, and read-only access
//! while the agent is running.
//!
//! These tests go through the reducer and the real renderer (TestBackend), the
//! same way a keystroke does. They pin:
//!
//! - `/context` and `/usage` open the same screen at different sections;
//! - the page is reachable while a turn is BUSY and never enters the message
//!   queue (a diagnostic page is not a prompt);
//! - side-effecting commands keep their busy refusal;
//! - the Usage section renders the runtime's report, and every unknown keeps
//!   its own wording instead of becoming a zero;
//! - a scope change never shows another scope's numbers, and a superseded
//!   answer never overwrites a newer one;
//! - closing the page stops it from owing answers.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use leveler_client_protocol::{
    ClientCommand, CommandId, PermissionProfile, RuntimeEvent, RuntimeStatus, SessionId,
    UiModelUsage, UiSessionSnapshot, UiUsageReport, UiUsageScope,
};
use leveler_tui::action::{Action, Effect};
use leveler_tui::query_track::QueryTrack;
use leveler_tui::reducer::reduce;
use leveler_tui::screen::{Screen, StatsSection};
use leveler_tui::state::{AppState, Boot};
use leveler_tui::theme::Theme;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn key(code: KeyCode) -> Action {
    Action::Key(KeyEvent::new(code, KeyModifiers::empty()))
}

fn typed(s: &mut AppState, text: &str) {
    for ch in text.chars() {
        reduce(s, key(KeyCode::Char(ch)));
    }
}

fn enter(s: &mut AppState) -> Vec<Effect> {
    reduce(s, key(KeyCode::Enter))
}

fn opened() -> AppState {
    let mut s = AppState::new(
        Theme::no_color(),
        Boot {
            session_id: SessionId::new("stats"),
            user: "tester".into(),
            version: "0.1.0".into(),
            show_welcome: false,
            draft_path: None,
            history_path: None,
            context_window: 200_000,
            locale: leveler_tui::Locale::Zh,
            untrusted_config: Vec::new(),
            model_notice: None,
            thinking: None,
        },
    );
    s.size = (100, 32);
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: UiSessionSnapshot {
                history: None,
                id: SessionId::new("stats"),
                repository: Some("/repo".into()),
                task_status: None,
                task_terminal: None,
                goal: "stats".into(),
                model: leveler_client_protocol::ModelRef::parse("deepseek/v3"),
                mode: PermissionProfile::Assisted,
                branch: Some("main".into()),
                status: "idle".into(),
                finalization_stage: None,
                messages: Vec::new(),
                pending_interactions: Vec::new(),
                available_models: Vec::new(),
                vision: false,
                last_sequence: None,
                latest_turn_ordinal: None,
                active_tools: Vec::new(),
                interrupted_tool_calls: Vec::new(),
                active_background_tasks: Vec::new(),
                background_tasks: Vec::new(),
                plan: None,
                diff: None,
                checkpoints: Vec::new(),
                recaps: Vec::new(),
                user_shells: Vec::new(),
                completion_report: None,
                thinking: None,
                work_profile: None,
                collaboration: None,
                execution_task: None,
                children: Vec::new(),
                foreign_origin: None,
            },
        }),
    );
    s
}

fn screen(state: &mut AppState) -> String {
    let (w, h) = state.size;
    let backend = TestBackend::new(w, h);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|f| leveler_tui::render::render(f, state))
        .unwrap();
    let buf = term.backend().buffer();
    let mut out = String::new();
    for y in 0..h {
        let mut x = 0u16;
        while x < w {
            let sym = buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" ");
            out.push_str(sym);
            x += unicode_width::UnicodeWidthStr::width(sym).max(1) as u16;
        }
        while out.ends_with(' ') {
            out.pop();
        }
        out.push('\n');
    }
    out
}

fn query_id(effects: &[Effect], want_usage: bool) -> Option<CommandId> {
    effects.iter().find_map(|effect| match effect {
        Effect::Send(ClientCommand::QueryUsage {
            query_id: Some(id), ..
        }) if want_usage => Some(id.clone()),
        Effect::Send(ClientCommand::QueryContext {
            query_id: Some(id), ..
        }) if !want_usage => Some(id.clone()),
        _ => None,
    })
}

fn context_accounting() -> leveler_model::ContextAccounting {
    use leveler_model::{
        ContextAccounting, ContextCategory, ContextPressure, FoldRequirement, ModelRef,
        TokenCountKind,
    };
    ContextAccounting {
        model: ModelRef::new("deepseek", "deepseek-chat"),
        context_window_tokens: Some(128_000),
        compact_at_tokens: Some(64_000),
        output_reservation_tokens: Some(32_000),
        headroom_tokens: Some(0),
        input_capacity_tokens: Some(96_000),
        fold_state: FoldRequirement::None,
        used_tokens: 9_100,
        free_tokens: Some(118_900),
        token_count_kind: TokenCountKind::Estimated,
        pressure: ContextPressure::Normal,
        categories: vec![
            ContextCategory {
                name: "messages".into(),
                label: "Messages".into(),
                tokens: 57,
                calls: 0,
                children: Vec::new(),
            },
            ContextCategory {
                name: "tool_definitions".into(),
                label: "Tool definitions".into(),
                tokens: 6_100,
                calls: 0,
                children: Vec::new(),
            },
            ContextCategory {
                name: "system".into(),
                label: "System".into(),
                tokens: 2_943,
                calls: 0,
                children: Vec::new(),
            },
        ],
        reasoning_projection: None,
        last_compaction: None,
    }
}

fn report(scope: UiUsageScope, requests: u32, input: u64, output: u64) -> UiUsageReport {
    UiUsageReport {
        scope,
        from: None,
        to: None,
        generated_at: "2026-01-01T00:00:00+00:00".into(),
        requests,
        unmeasured_requests: 0,
        input_tokens: input,
        cached_input_tokens: Some(input / 2),
        output_tokens: output,
        reasoning_tokens: Some(output / 2),
        cost_usd_micros: Some(1_234),
        priced_requests: requests,
        models: vec![UiModelUsage {
            provider: "deepseek".into(),
            model: "deepseek-flash".into(),
            requests,
            unmeasured_requests: 0,
            input_tokens: input,
            cached_input_tokens: Some(input / 2),
            output_tokens: output,
            reasoning_tokens: Some(output / 2),
            cost_usd_micros: Some(1_234),
            priced_requests: requests,
        }],
    }
}

fn deliver_usage(s: &mut AppState, id: &CommandId, report: UiUsageReport) {
    reduce(
        s,
        Action::Runtime(RuntimeEvent::UsageLoaded {
            query_id: Some(id.clone()),
            report: Box::new(report),
        }),
    );
}

#[test]
fn both_commands_open_the_same_page_at_their_own_section() {
    let mut s = opened();
    typed(&mut s, "/context");
    let effects = enter(&mut s);
    assert_eq!(s.active_screen, Screen::Context);
    assert_eq!(s.stats_section, StatsSection::Context);
    assert!(query_id(&effects, false).is_some(), "{effects:?}");
    assert!(
        query_id(&effects, true).is_some(),
        "the page shows both sections, so both are asked for: {effects:?}"
    );

    // Esc is the one way out; the page owns the keyboard while it is up.
    reduce(&mut s, key(KeyCode::Esc));
    assert_eq!(s.active_screen, Screen::Conversation);

    typed(&mut s, "/usage");
    let effects = enter(&mut s);
    assert_eq!(
        s.active_screen,
        Screen::Context,
        "/usage opens the SAME page"
    );
    assert_eq!(s.stats_section, StatsSection::Usage);
    assert!(s.stats_reveal == Some(StatsSection::Usage));
    assert!(query_id(&effects, true).is_some(), "{effects:?}");
    assert!(
        query_id(&effects, false).is_some(),
        "the page shows its Context section too: {effects:?}"
    );

    // Re-entering at the other section moves to it and keeps the same page.
    reduce(&mut s, key(KeyCode::Esc));
    typed(&mut s, "/context");
    enter(&mut s);
    assert_eq!(s.active_screen, Screen::Context);
    assert_eq!(s.stats_section, StatsSection::Context);
}

#[test]
fn usage_is_discoverable_in_help_and_completion() {
    let names: Vec<&str> = leveler_tui::screen::slash_commands(leveler_tui::Locale::Zh.text())
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert!(names.contains(&"/usage"), "{names:?}");

    let listed = leveler_tui::screen::all_slash_tokens();
    assert!(listed.contains(&"/usage"));

    let mut s = opened();
    typed(&mut s, "/us");
    let popup = leveler_tui::screen::visible_slash_popup(&s);
    assert!(
        popup.iter().any(|(name, _)| name == "/usage"),
        "completion must offer it: {popup:?}"
    );

    // Searchable, like /context: it is not noise on a bare `/`.
    let mut s = opened();
    typed(&mut s, "/");
    let popup = leveler_tui::screen::visible_slash_popup(&s);
    assert!(
        !popup.iter().any(|(name, _)| name == "/usage"),
        "not on the bare-/ list: {popup:?}"
    );
}

/// The whole point of the Busy work: a diagnostic page opens WHILE the agent
/// runs, and nothing about it becomes a queued prompt.
#[test]
fn read_only_diagnostics_open_while_busy_and_never_enter_the_queue() {
    let cases: &[(&str, Screen)] = &[
        ("/trace", Screen::Trace),
        ("/context", Screen::Context),
        ("/usage", Screen::Context),
    ];
    for (command, expected) in cases {
        let mut s = opened();
        s.status = RuntimeStatus::Busy;
        typed(&mut s, command);
        let effects = enter(&mut s);
        assert_eq!(s.active_screen, *expected, "{command} must open while busy");
        assert!(
            s.pending_inputs.is_empty(),
            "{command} must not queue itself as a prompt"
        );
        assert!(
            effects
                .iter()
                .all(|effect| !matches!(effect, Effect::Submit { .. })),
            "{command} must not start a turn: {effects:?}"
        );
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::Send(_))),
            "{command} must ask the runtime for its read model: {effects:?}"
        );
        // The running turn is untouched.
        assert!(s.is_busy(), "{command} must not change the turn's state");
    }
}

#[test]
fn busy_help_diff_and_output_stay_local_commands() {
    // `/help` and `/doctor` are pure local pages.
    let mut s = opened();
    s.status = RuntimeStatus::Busy;
    typed(&mut s, "/help");
    enter(&mut s);
    assert_eq!(s.active_screen, Screen::Help);
    reduce(&mut s, key(KeyCode::Esc));
    assert_eq!(s.active_screen, Screen::Conversation);

    // `/diff` asks the runtime for the diff it shows; it never starts a turn.
    let mut s = opened();
    s.status = RuntimeStatus::Busy;
    typed(&mut s, "/diff");
    let effects = enter(&mut s);
    assert_eq!(s.active_screen, Screen::Diff);
    assert!(matches!(
        effects.as_slice(),
        [Effect::Send(ClientCommand::RequestDiff { .. })]
    ));
    assert!(s.is_busy());
}

#[test]
fn side_effecting_commands_keep_their_busy_refusal() {
    for command in ["/compact", "/new", "/restore", "/update"] {
        let mut s = opened();
        s.status = RuntimeStatus::Busy;
        typed(&mut s, command);
        let effects = enter(&mut s);
        assert!(
            effects.is_empty(),
            "{command} must not run while busy: {effects:?}"
        );
        assert_eq!(s.active_screen, Screen::Conversation);
        assert!(
            s.notification.is_some(),
            "{command} must say why it refused"
        );
    }
}

#[test]
fn tab_switches_the_usage_range_without_stealing_navigation() {
    let mut s = opened();
    typed(&mut s, "/usage");
    enter(&mut s);
    assert_eq!(s.usage.scope, UiUsageScope::Session);

    let effects = reduce(&mut s, key(KeyCode::Tab));
    assert_eq!(s.usage.scope, UiUsageScope::Today);
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::Send(ClientCommand::QueryUsage {
                scope: UiUsageScope::Today,
                ..
            })]
        ),
        "{effects:?}"
    );

    // Shift+Tab is the permission cycle everywhere, so backwards is `[`.
    let effects = reduce(&mut s, key(KeyCode::Char('[')));
    assert_eq!(s.usage.scope, UiUsageScope::Session);
    assert!(matches!(
        effects.as_slice(),
        [Effect::Send(ClientCommand::QueryUsage {
            scope: UiUsageScope::Session,
            ..
        })]
    ));

    // Two backwards presses move two scopes, not one: the key is a step, not a
    // one-shot.
    let mut s = opened();
    typed(&mut s, "/usage");
    enter(&mut s);
    reduce(&mut s, key(KeyCode::Char('[')));
    assert_eq!(s.usage.scope, UiUsageScope::Turn, "Session -> Turn");
    reduce(&mut s, key(KeyCode::Char('[')));
    assert_eq!(s.usage.scope, UiUsageScope::All, "Turn wraps to All");

    // The range keys are the ONLY new binding: ↑↓ still move the Context
    // selection, and Esc still leaves the page.
    let open = {
        let mut s2 = opened();
        typed(&mut s2, "/context");
        let effects = enter(&mut s2);
        (s2, query_id(&effects, false).expect("context query"))
    };
    let (mut s, context_id) = open;
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ContextLoaded {
            query_id: Some(context_id),
            accounting: Some(context_accounting()),
        }),
    );
    s.stats_section = StatsSection::Context;
    s.context.selected = 0;
    reduce(&mut s, key(KeyCode::Down));
    assert_eq!(s.context.selected, 1);
    reduce(&mut s, key(KeyCode::Esc));
    assert_eq!(s.active_screen, Screen::Conversation);
}

#[test]
fn the_usage_section_renders_the_runtimes_report() {
    let mut s = opened();
    typed(&mut s, "/usage");
    let effects = enter(&mut s);
    let id = query_id(&effects, true).expect("usage query");
    deliver_usage(&mut s, &id, report(UiUsageScope::Session, 3, 12_000, 1_500));

    let text = screen(&mut s);
    assert!(text.contains("上下文与用量"), "{text}");
    assert!(text.contains("模型用量 · 本会话"), "{text}");
    assert!(text.contains("Total tokens"), "{text}");
    assert!(
        text.lines()
            .any(|line| line.contains("Total tokens") && line.contains("13.5k")),
        "total: {text}"
    );
    assert!(text.contains("Requests"), "{text}");
    assert!(text.contains("输入"), "{text}");
    assert!(
        text.contains("缓存输入 6k"),
        "cached is a slice of input: {text}"
    );
    assert!(text.contains("输出"), "{text}");
    assert!(
        text.contains("思考 750"),
        "reasoning is a slice of output: {text}"
    );
    assert!(text.contains("$0.0012"), "{text}");
    assert!(text.contains("deepseek / deepseek-flash"), "{text}");
    assert!(text.contains("3 次 · 13.5k tokens"), "{text}");
    // No second diamond chart for usage: the map belongs to Context only.
    let usage_lines: Vec<&str> = text
        .lines()
        .skip_while(|line| !line.contains("模型用量"))
        .collect();
    assert!(
        usage_lines.iter().all(|line| !line.contains('●')),
        "usage must stay numeric: {usage_lines:?}"
    );
}

#[test]
fn the_usage_section_keeps_its_empty_and_unknown_states_distinct() {
    // 1. Nothing has answered for this scope yet.
    let mut s = opened();
    typed(&mut s, "/usage");
    enter(&mut s);
    let text = screen(&mut s);
    assert!(text.contains("读取用量中"), "{text}");
    assert!(!text.contains("$0.00"), "{text}");

    // 2. The scope really holds no call.
    let mut s = opened();
    typed(&mut s, "/usage");
    let effects = enter(&mut s);
    let id = query_id(&effects, true).unwrap();
    let mut empty = report(UiUsageScope::Session, 0, 0, 0);
    empty.cached_input_tokens = None;
    empty.reasoning_tokens = None;
    empty.cost_usd_micros = None;
    empty.priced_requests = 0;
    empty.models.clear();
    deliver_usage(&mut s, &id, empty);
    let text = screen(&mut s);
    assert!(text.contains("本会话暂无模型使用记录"), "{text}");
    assert!(!text.contains("$0.00"), "{text}");

    // 3. Calls happened but the provider reported no usage.
    let mut s = opened();
    typed(&mut s, "/usage");
    let effects = enter(&mut s);
    let id = query_id(&effects, true).unwrap();
    let mut unmeasured = report(UiUsageScope::Session, 2, 0, 0);
    unmeasured.unmeasured_requests = 2;
    unmeasured.cached_input_tokens = None;
    unmeasured.reasoning_tokens = None;
    unmeasured.cost_usd_micros = None;
    unmeasured.priced_requests = 0;
    unmeasured.models.clear();
    deliver_usage(&mut s, &id, unmeasured);
    let text = screen(&mut s);
    assert!(text.contains("Provider 未返回用量"), "{text}");
    assert!(text.contains("不可估算"), "no fabricated cost: {text}");
    assert!(!text.contains("$0.00"), "{text}");
}

#[test]
fn an_unknown_price_is_not_zero_and_a_partial_one_says_so() {
    let mut s = opened();
    typed(&mut s, "/usage");
    let effects = enter(&mut s);
    let id = query_id(&effects, true).unwrap();
    let mut report = report(UiUsageScope::Session, 4, 1_000, 100);
    report.cost_usd_micros = Some(500);
    report.priced_requests = 2;
    deliver_usage(&mut s, &id, report);
    let text = screen(&mut s);
    assert!(text.contains("部分请求无价格"), "{text}");
    assert!(text.contains("~$0.0005"), "{text}");
}

#[test]
fn a_report_for_another_scope_never_appears_under_this_label() {
    let mut s = opened();
    typed(&mut s, "/usage");
    let effects = enter(&mut s);
    let session_id = query_id(&effects, true).unwrap();
    // The reader switches to Today before the session answer lands.
    let switch = reduce(&mut s, key(KeyCode::Tab));
    let today_id = query_id(&switch, true).unwrap();
    assert_ne!(session_id, today_id);

    deliver_usage(
        &mut s,
        &session_id,
        report(UiUsageScope::Session, 9, 9_000, 900),
    );
    assert!(
        s.usage.loaded.is_none(),
        "a superseded answer is dropped, not shown under another scope"
    );
    let text = screen(&mut s);
    assert!(text.contains("读取用量中"), "{text}");

    deliver_usage(&mut s, &today_id, report(UiUsageScope::Today, 1, 100, 10));
    assert!(screen(&mut s).contains("模型用量 · 今天"));
}

#[test]
fn a_replayed_answer_is_never_applied_twice() {
    let mut s = opened();
    typed(&mut s, "/usage");
    let effects = enter(&mut s);
    let id = query_id(&effects, true).unwrap();
    deliver_usage(&mut s, &id, report(UiUsageScope::Session, 1, 10, 1));
    // The same event replayed (a reconnect, a duplicate delivery).
    deliver_usage(&mut s, &id, report(UiUsageScope::Session, 99, 9_900, 900));
    let text = screen(&mut s);
    assert!(
        text.lines()
            .any(|line| line.contains("Total tokens") && line.trim_end().ends_with("11")),
        "first answer stands: {text}"
    );
    assert!(!text.contains("99"));
}

#[test]
fn closing_the_page_drops_the_answers_it_was_owed() {
    let mut s = opened();
    typed(&mut s, "/trace");
    let effects = enter(&mut s);
    let id = query_id(&effects, false);
    assert!(id.is_none(), "trace has its own query kind");
    let trace_id = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::Send(ClientCommand::QueryObservability {
                query_id: Some(id), ..
            }) => Some(id.clone()),
            _ => None,
        })
        .expect("trace query");
    reduce(&mut s, key(KeyCode::Esc));
    assert_eq!(s.active_screen, Screen::Conversation);
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ObservabilityLoaded {
            query_id: Some(trace_id),
            observation: empty_observation(),
        }),
    );
    assert!(
        s.trace.loaded.is_none(),
        "a closed page is not waiting for that answer any more"
    );
    assert!(!s.trace.track.in_flight());
}

fn empty_observation() -> leveler_client_protocol::UiObservabilityLoaded {
    leveler_client_protocol::UiObservabilityLoaded {
        session: leveler_client_protocol::UiSessionObservation {
            session_id: SessionId::new("stats"),
            goal: "stats".into(),
            repository: None,
            created_at: "t".into(),
            updated_at: "t".into(),
            status: "idle".into(),
            model: "m".into(),
            work_profile: "single".into(),
            collaboration: "chat".into(),
            last_sequence: None,
            request_count: 0,
            input_tokens: 0,
            output_tokens: 0,
            avg_latency_ms: None,
            last_latency_ms: None,
            request_failures: 0,
            request_retries: 0,
            tool_started: 0,
            tool_finished: 0,
            compact_count: 0,
            subagent_started: 0,
            duration_ms: None,
            cached_input_tokens: None,
            reasoning_tokens: None,
            cost_usd_micros: None,
            lanes: Vec::new(),
        },
        window: Vec::new(),
        window_from: 0,
        window_to: 0,
        requests: Vec::new(),
        tools: Vec::new(),
        agents: Vec::new(),
        recovery: leveler_client_protocol::UiRecoveryObservation {
            interrupted_turns: 0,
            workspace_snapshots: 0,
            review_stages: Vec::new(),
        },
        relations: Vec::new(),
    }
}

/// A busy turn fires many facts the trace page shows. Every one of them must
/// coalesce into at most one outstanding query, and the answer must land.
#[test]
fn trace_refreshes_coalesce_and_land_while_busy() {
    let mut s = opened();
    s.status = RuntimeStatus::Busy;
    typed(&mut s, "/trace");
    let open = enter(&mut s);
    let first = open
        .iter()
        .find_map(|effect| match effect {
            Effect::Send(ClientCommand::QueryObservability {
                query_id: Some(id), ..
            }) => Some(id.clone()),
            _ => None,
        })
        .expect("the page asks once on open");

    let mut issued = 0;
    for _ in 0..5 {
        let effects = reduce(
            &mut s,
            Action::Runtime(RuntimeEvent::ToolCallStarted {
                id: leveler_client_protocol::ToolCallId::new("c1"),
                name: "read_file".into(),
                arguments: "{}".into(),
                parallel: false,
                model_step: None,
                answer_effect: None,
            }),
        );
        issued += effects
            .iter()
            .filter(|effect| {
                matches!(
                    effect,
                    Effect::Send(ClientCommand::QueryObservability { .. })
                )
            })
            .count();
    }
    assert_eq!(issued, 0, "no query is issued while one is in flight");

    // The answer lands, and exactly ONE follow-up covers the whole burst.
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ObservabilityLoaded {
            query_id: Some(first),
            observation: empty_observation(),
        }),
    );
    assert!(s.trace.loaded.is_some(), "the answer is applied");
    let effects = reduce(&mut s, Action::IdleTick(std::time::Instant::now()));
    let follow_ups = effects
        .iter()
        .filter(|effect| {
            matches!(
                effect,
                Effect::Send(ClientCommand::QueryObservability { .. })
            )
        })
        .count();
    assert!(follow_ups <= 1, "at most one follow-up: {effects:?}");
}

#[test]
fn a_superseded_trace_answer_does_not_overwrite_a_newer_one() {
    let mut s = opened();
    typed(&mut s, "/trace");
    let open = enter(&mut s);
    let older = open
        .iter()
        .find_map(|effect| match effect {
            Effect::Send(ClientCommand::QueryObservability {
                query_id: Some(id), ..
            }) => Some(id.clone()),
            _ => None,
        })
        .unwrap();
    // `r` asks again while the first query is still in flight.
    let retry = reduce(&mut s, key(KeyCode::Char('r')));
    let newer = retry
        .iter()
        .find_map(|effect| match effect {
            Effect::Send(ClientCommand::QueryObservability {
                query_id: Some(id), ..
            }) => Some(id.clone()),
            _ => None,
        })
        .unwrap();
    assert_ne!(older, newer);

    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ObservabilityLoaded {
            query_id: Some(older),
            observation: empty_observation(),
        }),
    );
    assert!(
        s.trace.loaded.is_none(),
        "the superseded answer must not be applied"
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ObservabilityLoaded {
            query_id: Some(newer),
            observation: empty_observation(),
        }),
    );
    assert!(s.trace.loaded.is_some());
}

#[test]
fn the_page_holds_its_geometry_at_eighty_and_forty_columns() {
    for (w, h) in [(80u16, 30u16), (40, 30)] {
        let mut s = opened();
        s.size = (w, h);
        typed(&mut s, "/usage");
        let effects = enter(&mut s);
        let id = query_id(&effects, true).unwrap();
        deliver_usage(&mut s, &id, report(UiUsageScope::Session, 2, 6_100, 90));
        let text = screen(&mut s);
        assert!(
            text.lines()
                .all(|l| unicode_width::UnicodeWidthStr::width(l) <= w as usize),
            "width {w}: a line overflowed:\n{text}"
        );
        assert!(text.contains("模型用量"), "width {w}:\n{text}");
        assert!(text.contains("6.1k"), "width {w}:\n{text}");
    }
}

#[test]
fn an_open_trace_asks_again_on_the_live_cadence_while_a_turn_runs() {
    // A single long tool appends durable rows (`command_progress`) that carry no
    // client event. Before this cadence existed the events tab kept the rows it
    // had when the tool started until the tool finished — a tested 20s staleness
    // on a real `sleep 20`, and minutes for a build.
    let mut s = opened();
    s.status = RuntimeStatus::Busy;
    typed(&mut s, "/trace");
    let open = enter(&mut s);
    let first = trace_query(&open).expect("the page asks once on open");

    let start = std::time::Instant::now();
    let early = reduce(&mut s, Action::IdleTick(start));
    assert!(
        trace_query(&early).is_none(),
        "a fresh page does not re-ask: {early:?}"
    );

    // The answer lands; the page is now clean and idle.
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ObservabilityLoaded {
            query_id: Some(first),
            observation: empty_observation(),
        }),
    );
    let answered_at = std::time::Instant::now();

    // Inside the window nothing is issued, however many ticks arrive.
    for _ in 0..5 {
        let effects = reduce(
            &mut s,
            Action::IdleTick(answered_at + std::time::Duration::from_millis(200)),
        );
        assert!(trace_query(&effects).is_none(), "{effects:?}");
    }

    // Past the window the page asks exactly once, and the in-flight query caps
    // the next ticks (single flight).
    let due = answered_at + leveler_tui::LIVE_REFRESH_INTERVAL;
    let refresh = reduce(&mut s, Action::IdleTick(due));
    let second = trace_query(&refresh).expect("the cadence re-asks");
    let repeated = reduce(
        &mut s,
        Action::IdleTick(due + std::time::Duration::from_secs(30)),
    );
    assert!(
        trace_query(&repeated).is_none(),
        "an outstanding query is never stacked: {repeated:?}"
    );

    // Its answer is accepted, so the follow-up cadence starts again.
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ObservabilityLoaded {
            query_id: Some(second),
            observation: empty_observation(),
        }),
    );
    let after = reduce(
        &mut s,
        Action::IdleTick(due + leveler_tui::LIVE_REFRESH_INTERVAL * 2),
    );
    assert!(trace_query(&after).is_some(), "{after:?}");
}

#[test]
fn the_live_cadence_owes_nothing_to_a_closed_page_or_an_idle_turn() {
    // Closed page: nothing is owed on the clock.
    let mut closed = opened();
    closed.status = RuntimeStatus::Busy;
    let effects = reduce(
        &mut closed,
        Action::IdleTick(std::time::Instant::now() + leveler_tui::LIVE_REFRESH_INTERVAL * 3),
    );
    assert!(trace_query(&effects).is_none(), "{effects:?}");

    // Open page, finished turn: the ledger cannot change without a fact event,
    // so the clock must not turn into a polling loop on an idle session.
    let mut idle = opened();
    typed(&mut idle, "/trace");
    let open = enter(&mut idle);
    let first = trace_query(&open).unwrap();
    reduce(
        &mut idle,
        Action::Runtime(RuntimeEvent::ObservabilityLoaded {
            query_id: Some(first),
            observation: empty_observation(),
        }),
    );
    let effects = reduce(
        &mut idle,
        Action::IdleTick(std::time::Instant::now() + leveler_tui::LIVE_REFRESH_INTERVAL * 3),
    );
    assert!(trace_query(&effects).is_none(), "{effects:?}");
}

fn trace_query(effects: &[Effect]) -> Option<CommandId> {
    effects.iter().find_map(|effect| match effect {
        Effect::Send(ClientCommand::QueryObservability {
            query_id: Some(id), ..
        }) => Some(id.clone()),
        _ => None,
    })
}

#[test]
fn the_tracker_is_the_one_refresh_authority() {
    // A page that never opened owes nothing, even after events.
    let mut track = QueryTrack::default();
    assert!(!track.wants_refresh());
    let id = track.issue();
    track.mark_dirty();
    assert!(!track.wants_refresh(), "one query in flight is the cap");
    assert!(track.accept(Some(&id)));
    assert!(track.wants_refresh());
}
