//! A command call in the conversation says, from runtime facts alone, whether
//! it is still running, for how long, how it ended, what it printed — and lets
//! the user stop that one command.

use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use leveler_client_protocol::{
    ClientCommand, RuntimeEvent, RuntimeStatus, SessionId, ToolCallId, UiActiveToolCall,
    UiCommandStop,
};

use super::reduce;
use crate::action::{Action, Effect, EffectCompletion};
use crate::state::{AppState, Boot};
use crate::transcript::{StopRequest, ToolStatus, TranscriptItem};

fn state() -> AppState {
    let mut s = AppState::new(
        crate::theme::Theme::no_color(),
        Boot {
            session_id: SessionId::new("s1"),
            user: "u".into(),
            version: "0.1.0".into(),
            show_welcome: false,
            draft_path: None,
            history_path: None,
            context_window: 200_000,
            locale: crate::i18n::Locale::Zh,
            untrusted_config: Vec::new(),
            reasoning_effort: None,
        },
    );
    s.size = (100, 40);
    s.conv.rect = Some((0, 2, 100, 30));
    s.conv.auto_scroll = false;
    s.conv.scroll = 0;
    s.status = RuntimeStatus::Busy;
    s
}

const CMD: &str = r#"{"cmd":"certbot renew --dry-run"}"#;

fn start(s: &mut AppState, id: &str) {
    s.transcript.push_user("验证续期".into());
    reduce(
        s,
        Action::Runtime(RuntimeEvent::ToolCallStarted {
            id: ToolCallId::new(id),
            name: "shell_command".into(),
            arguments: CMD.into(),
            parallel: false,
        }),
    );
}

fn complete(
    s: &mut AppState,
    id: &str,
    ok: bool,
    duration_ms: u64,
    exit_code: Option<i32>,
    stop: Option<UiCommandStop>,
) {
    reduce(
        s,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            id: ToolCallId::new(id),
            ok,
            preview: format!("exit: {}\n", exit_code.unwrap_or(0)),
            duration_ms,
            applied_diff: None,
            exit_code,
            stop,
        }),
    );
}

fn output(s: &mut AppState, id: &str, chunk: &str) {
    reduce(
        s,
        Action::Runtime(RuntimeEvent::ToolCallOutput {
            id: ToolCallId::new(id),
            stream: "stdout".into(),
            chunk: chunk.into(),
        }),
    );
}

fn plain(s: &AppState) -> Vec<String> {
    s.conversation_lines(s.conv.rect.unwrap().2 as usize)
        .iter()
        .map(crate::selection::line_to_plain)
        .collect()
}

fn row_with(s: &AppState, needle: &str) -> Option<(usize, String)> {
    plain(s)
        .into_iter()
        .enumerate()
        .find(|(_, l)| l.contains(needle))
}

fn call(s: &AppState, id: &str) -> crate::transcript::ToolCallBlock {
    s.transcript
        .tool_calls()
        .into_iter()
        .find(|c| c.id.as_str() == id)
        .cloned()
        .expect("call")
}

fn screen_row_of(s: &AppState, abs_line: usize) -> u16 {
    let (_, ry, rw, rh) = s.conv.rect.unwrap();
    let total = s.conversation_lines(rw as usize).len();
    let pad = (rh as usize).saturating_sub(total);
    ry + (pad + abs_line) as u16
}

fn click(s: &mut AppState, col: u16, row: u16) -> Vec<Effect> {
    reduce(
        s,
        Action::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: col,
            row,
            modifiers: KeyModifiers::empty(),
        }),
    )
}

/// Screen (col, row) of the first occurrence of `needle`.
fn cell_of(s: &AppState, needle: &str) -> (u16, u16) {
    let (line, text) = row_with(s, needle).expect(needle);
    let byte = text.find(needle).unwrap();
    let col = unicode_width::UnicodeWidthStr::width(&text[..byte]) as u16;
    (s.conv.rect.unwrap().0 + col, screen_row_of(s, line))
}

#[test]
fn a_running_command_says_it_is_running_for_how_long_and_offers_stop() {
    let mut s = state();
    s.elapsed_secs = 3;
    start(&mut s, "c1");
    s.elapsed_secs = 45;
    let (line, head) = row_with(&s, "执行命令").expect("command row");
    assert!(head.contains("◌ 执行命令 · 运行中 · 42s"), "{head:?}");
    assert!(head.contains("停止"), "{head:?}");
    assert!(
        plain(&s)[line + 1].contains("$ certbot renew --dry-run"),
        "{:?}",
        plain(&s)
    );
}

/// The clock ticks without any transcript change: the row must repaint it
/// instead of serving a frame cached before the second rolled over.
#[test]
fn the_running_clock_advances_with_the_app_tick() {
    let mut s = state();
    start(&mut s, "c1");
    s.elapsed_secs = 10;
    assert!(row_with(&s, "运行中 · 10s").is_some(), "{:?}", plain(&s));
    s.elapsed_secs = 11;
    assert!(row_with(&s, "运行中 · 11s").is_some(), "{:?}", plain(&s));
}

#[test]
fn terminal_rows_name_how_the_command_ended() {
    let mut s = state();
    start(&mut s, "ok");
    complete(&mut s, "ok", true, 18_200, Some(0), None);
    assert!(
        row_with(&s, "✓ 执行命令 · 已完成 · 18.2s").is_some(),
        "{:?}",
        plain(&s)
    );

    let mut s = state();
    start(&mut s, "bad");
    complete(&mut s, "bad", false, 18_200, Some(1), None);
    assert!(
        row_with(&s, "✗ 执行命令 · 失败 · 18.2s · exit 1").is_some(),
        "{:?}",
        plain(&s)
    );

    let mut s = state();
    start(&mut s, "stopped");
    complete(
        &mut s,
        "stopped",
        false,
        1_500,
        None,
        Some(UiCommandStop::Confirmed),
    );
    assert_eq!(call(&s, "stopped").status, ToolStatus::Cancelled);
    assert!(
        row_with(&s, "⊘ 执行命令 · 已停止 · 1.5s").is_some(),
        "{:?}",
        plain(&s)
    );
}

/// Stopping a command at the live edge keeps following it. Dogfood: the stop
/// click pinned the viewport like a disclosure click, so the stopped row's
/// result and the model's answer landed below the screen behind a ▼ badge.
#[test]
fn stopping_a_command_at_the_live_edge_keeps_following() {
    let mut s = state();
    start(&mut s, "c1");
    s.conv.auto_scroll = true;
    let (col, row) = cell_of(&s, "停止");
    click(&mut s, col, row);
    assert_eq!(call(&s, "c1").stop, StopRequest::Sent);
    assert!(s.conv.auto_scroll, "a stop must not leave auto-follow");
}

/// A stop the runtime could not confirm is not a stop, and not a failure.
#[test]
fn an_unconfirmed_stop_is_unknown_never_stopped_or_failed() {
    let mut s = state();
    start(&mut s, "c1");
    complete(
        &mut s,
        "c1",
        false,
        1_500,
        None,
        Some(UiCommandStop::Unconfirmed),
    );
    assert_eq!(call(&s, "c1").status, ToolStatus::Unknown);
    let (_, head) = row_with(&s, "执行命令").unwrap();
    assert!(head.contains("? 执行命令 · 状态未知"), "{head:?}");
    assert!(
        !head.contains("已停止") && !head.contains("失败"),
        "{head:?}"
    );
}

/// A turn that ends with no terminal for a running call leaves that call's
/// outcome unknown — the UI holds no fact that it failed.
#[test]
fn a_turn_ending_under_a_running_command_leaves_it_unknown() {
    let mut s = state();
    start(&mut s, "c1");
    reduce(&mut s, Action::Runtime(RuntimeEvent::TurnCancelled));
    assert_eq!(call(&s, "c1").status, ToolStatus::Unknown);
}

#[test]
fn clicking_stop_requests_a_stop_of_that_command_only() {
    let mut s = state();
    start(&mut s, "c1");
    s.elapsed_secs = 5;
    let (col, row) = cell_of(&s, "停止");
    let effects = click(&mut s, col, row);
    assert_eq!(
        effects,
        vec![Effect::Send(ClientCommand::CancelToolCall {
            session_id: SessionId::new("s1"),
            call_id: ToolCallId::new("c1"),
        })]
    );
    assert_eq!(call(&s, "c1").stop, StopRequest::Sent);
    let (_, head) = row_with(&s, "执行命令").unwrap();
    assert!(head.contains("正在停止…"), "{head:?}");
    assert!(!head.contains("运行中"), "{head:?}");

    // Stopping is intent; only the runtime's terminal says it stopped.
    complete(
        &mut s,
        "c1",
        false,
        5_000,
        None,
        Some(UiCommandStop::Confirmed),
    );
    assert!(
        row_with(&s, "⊘ 执行命令 · 已停止").is_some(),
        "{:?}",
        plain(&s)
    );
}

#[test]
fn a_stop_whose_delivery_is_unknown_says_so() {
    let mut s = state();
    start(&mut s, "c1");
    let (col, row) = cell_of(&s, "停止");
    let effects = click(&mut s, col, row);
    let Effect::Send(command) = effects[0].clone() else {
        panic!("{effects:?}")
    };
    reduce(
        &mut s,
        Action::EffectCompleted(EffectCompletion::CommandUncertain {
            command,
            snapshot: None,
        }),
    );
    assert_eq!(call(&s, "c1").stop, StopRequest::Uncertain);
    let (_, head) = row_with(&s, "执行命令").unwrap();
    assert!(head.contains("停止状态未知"), "{head:?}");
    assert!(!head.contains("已停止"), "{head:?}");
}

#[test]
fn a_refused_stop_returns_the_row_to_running() {
    let mut s = state();
    start(&mut s, "c1");
    let (col, row) = cell_of(&s, "停止");
    let effects = click(&mut s, col, row);
    let Effect::Send(command) = effects[0].clone() else {
        panic!("{effects:?}")
    };
    reduce(
        &mut s,
        Action::EffectCompleted(EffectCompletion::CommandRejected {
            command,
            message: "该命令已结束或尚未开始执行,无需停止".into(),
            snapshot: None,
        }),
    );
    assert_eq!(call(&s, "c1").stop, StopRequest::None);
    let (_, head) = row_with(&s, "执行命令").unwrap();
    assert!(head.contains("运行中") && head.contains("停止"), "{head:?}");
}

#[test]
fn clicking_the_row_shows_and_hides_its_live_output() {
    let mut s = state();
    start(&mut s, "c1");
    output(&mut s, "c1", "Processing /etc/letsencrypt/renewal\n");
    output(&mut s, "c1", "Waiting for DNS propagation\n");
    assert!(
        row_with(&s, "Waiting for DNS").is_none(),
        "collapsed by default"
    );

    let (_, row) = cell_of(&s, "$ certbot");
    click(&mut s, 6, row);
    assert!(call(&s, "c1").expanded);
    assert!(
        row_with(&s, "Processing /etc/letsencrypt").is_some(),
        "{:?}",
        plain(&s)
    );
    assert!(row_with(&s, "Waiting for DNS").is_some(), "{:?}", plain(&s));

    let (_, row) = cell_of(&s, "$ certbot");
    click(&mut s, 6, row);
    assert!(!call(&s, "c1").expanded);
    assert!(row_with(&s, "Waiting for DNS").is_none());
}

/// A chatty command cannot grow the conversation without bound.
#[test]
fn expanded_output_is_bounded_and_says_what_it_hid() {
    let mut s = state();
    start(&mut s, "c1");
    let chunk: String = (1..=200).map(|i| format!("line {i}\n")).collect();
    output(&mut s, "c1", &chunk);
    let (_, row) = cell_of(&s, "$ certbot");
    click(&mut s, 6, row);
    let lines = plain(&s);
    assert!(lines.iter().any(|l| l.contains("line 200")), "{lines:?}");
    assert!(!lines.iter().any(|l| l.ends_with("line 1")), "{lines:?}");
    assert!(lines.iter().any(|l| l.contains("行未显示")), "{lines:?}");
    assert!(lines.len() < 60, "{}", lines.len());
}

/// Two commands the runtime ran in one concurrent burst each keep their own
/// status, clock, output and disclosure.
#[test]
fn parallel_commands_keep_independent_identity() {
    let mut s = state();
    s.transcript.push_user("并行验证".into());
    for id in ["a", "b"] {
        reduce(
            &mut s,
            Action::Runtime(RuntimeEvent::ToolCallStarted {
                id: ToolCallId::new(id),
                name: "run_command".into(),
                arguments: format!(r#"{{"program":"echo","args":["{id}"]}}"#),
                parallel: true,
            }),
        );
    }
    output(&mut s, "a", "from a\n");
    complete(&mut s, "b", true, 2_000, Some(0), None);
    let lines = plain(&s);
    assert!(lines.iter().any(|l| l.contains("并行")), "{lines:?}");
    assert!(
        lines.iter().any(|l| l.contains("运行中")) && lines.iter().any(|l| l.contains("已完成")),
        "{lines:?}"
    );
    let (_, row) = cell_of(&s, "$ echo a");
    click(&mut s, 8, row);
    assert!(call(&s, "a").expanded);
    assert!(!call(&s, "b").expanded);
    assert!(row_with(&s, "from a").is_some(), "{:?}", plain(&s));
}

/// Reconnecting to a long command keeps its runtime-measured clock and what
/// it already printed.
#[test]
fn a_reconnect_restores_the_running_clock_and_output() {
    let mut s = state();
    let mut snap = leveler_client_protocol::UiSessionSnapshot {
        id: SessionId::new("s1"),
        repository: "/repo".into(),
        goal: "g".into(),
        model: None,
        mode: leveler_client_protocol::PermissionProfile::Assisted,
        branch: None,
        status: "busy".into(),
        finalization_stage: None,
        messages: Vec::new(),
        pending_interactions: Vec::new(),
        available_models: Vec::new(),
        vision: false,
        last_sequence: None,
        active_tools: Vec::new(),
        plan: None,
        verification: None,
        diff: None,
        checkpoints: Vec::new(),
        recaps: Vec::new(),
        user_shells: Vec::new(),
        completion_report: None,
        reasoning: None,
        work_profile: None,
        collaboration: None,
        children: Vec::new(),
    };
    snap.active_tools = vec![UiActiveToolCall {
        id: ToolCallId::new("c1"),
        name: "shell_command".into(),
        arguments: CMD.into(),
        elapsed_ms: 90_000,
        output_tail: "Processing renewal\n".into(),
        output_truncated: false,
    }];
    s.elapsed_secs = 0;
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened { session: snap }),
    );
    let call = call(&s, "c1");
    assert_eq!(call.output, "Processing renewal\n");
    assert!(row_with(&s, "运行中 · 1m 30s").is_some(), "{:?}", plain(&s));
    assert!(matches!(
        s.transcript.items().last(),
        Some(TranscriptItem::ToolGroup(_))
    ));
}
