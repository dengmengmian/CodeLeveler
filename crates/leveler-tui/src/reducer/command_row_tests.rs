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

/// A command the sandbox ran without the network, and that failed reaching
/// it, reads as a permission the user has not granted — not as a broken
/// command followed by an unexplained prompt.
#[test]
fn a_network_denied_command_reads_as_needing_network_permission() {
    let mut s = state();
    start(&mut s, "net");
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            id: ToolCallId::new("net"),
            ok: false,
            preview: "[network permission required] This command ran with network access blocked by the sandbox…\n\nexit: 6\n--- stderr ---\ncurl: (6) Could not resolve host: example.com".into(),
            duration_ms: 300,
            applied_diff: None,
            exit_code: Some(6),
            stop: None,
        }),
    );
    let rows = plain(&s);
    assert!(
        row_with(&s, "⚠ 执行命令 · 需要网络权限").is_some(),
        "{rows:?}"
    );
    assert!(!rows.iter().any(|r| r.contains("✗")), "{rows:?}");
    assert!(
        rows.iter().any(|r| r.contains("本次在断网沙箱中运行")),
        "{rows:?}"
    );
    assert!(
        !rows
            .iter()
            .any(|r| r.contains("[network permission required]")),
        "the model-facing marker never reaches the user: {rows:?}"
    );
}

/// A command handed to the background was STARTED, not finished: the call
/// returns the moment the process detaches. The row said "✓ 执行命令 · 已完成
/// · 0.0s" over a bench that would run for five minutes.
#[test]
fn a_backgrounded_command_says_it_started_not_that_it_finished() {
    let mut s = state();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ToolCallStarted {
            id: ToolCallId::new("bg"),
            name: "run_command".into(),
            arguments: r#"{"program":"node","args":["bench/run.js","40000"],"background":true}"#
                .into(),
            parallel: false,
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            id: ToolCallId::new("bg"),
            ok: true,
            preview: "background task started\ntask_id: t-1\nstatus: running".into(),
            duration_ms: 12,
            applied_diff: None,
            exit_code: Some(0),
            stop: None,
        }),
    );
    let rows = plain(&s);
    assert!(
        rows.iter().any(|r| r.contains("已转入后台")),
        "the row says it started: {rows:?}"
    );
    assert!(
        !rows.iter().any(|r| r.contains("已完成")),
        "and never that it finished: {rows:?}"
    );
}

/// A failed command's one-line note is what went wrong, not the exit code the
/// head already states or a stream header.
#[test]
fn a_failed_command_summarizes_its_error_not_its_exit_code() {
    let mut s = state();
    start(&mut s, "t");
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            id: ToolCallId::new("t"),
            ok: false,
            preview: "exit: 101\n--- stdout ---\nrunning 1 test\ntest cli::tests::rollback ... FAILED\n--- stderr ---\n   Compiling envgate v0.1.0\nerror: test failed, to rerun pass `--bin envgate`".into(),
            duration_ms: 16_900,
            applied_diff: None,
            exit_code: Some(101),
            stop: None,
        }),
    );
    let rows = plain(&s);
    assert!(
        rows.iter()
            .any(|r| r.contains("└ test cli::tests::rollback ... FAILED")),
        "the first line reporting a failure: {rows:?}"
    );
    assert!(!rows.iter().any(|r| r.contains("└ exit: 101")), "{rows:?}");

    // A multi-binary `cargo test`: the first suite passed, and its summary
    // says "0 failed". Reading that as the reason left "失败 · exit 101" over
    // "test result: ok. 11 passed; 0 failed" — a passing line under a failure.
    let mut s = state();
    start(&mut s, "u");
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            id: ToolCallId::new("u"),
            ok: false,
            preview: "exit: 101\n--- stdout ---\ntest result: ok. 11 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\nrunning 5 tests\ntest default_outputs_all_stats ... FAILED\n--- stderr ---\nerror: test failed, to rerun pass `--test cli`".into(),
            duration_ms: 2_900,
            applied_diff: None,
            exit_code: Some(101),
            stop: None,
        }),
    );
    let rows = plain(&s);
    assert!(
        rows.iter()
            .any(|r| r.contains("└ test default_outputs_all_stats ... FAILED")),
        "a zero count is not a failure report: {rows:?}"
    );

    // A runner that prints progress first and marks failures with ✗ (colored).
    let mut s = state();
    start(&mut s, "v");
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            id: ToolCallId::new("v"),
            ok: false,
            preview: "exit: 1\n--- stdout ---\n▸ 1. userProfile · 首次 get 自动建账\n  \u{1b}[32m✓\u{1b}[0m 建账\n  \u{1b}[31m✗\u{1b}[0m B 第三笔触顶 cap，cut=80\n\u{1b}[31m✗ 11 failed\u{1b}[0m · 85 passed".into(),
            duration_ms: 200,
            applied_diff: None,
            exit_code: Some(1),
            stop: None,
        }),
    );
    let rows = plain(&s);
    assert!(
        rows.iter()
            .any(|r| r.contains("└ ✗ B 第三笔触顶 cap，cut=80")),
        "{rows:?}"
    );

    let mut s = state();
    start(&mut s, "u");
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            id: ToolCallId::new("u"),
            ok: false,
            preview: "exit: 2\n--- stderr ---\nls: nope: No such file or directory".into(),
            duration_ms: 100,
            applied_diff: None,
            exit_code: Some(2),
            stop: None,
        }),
    );
    let rows = plain(&s);
    assert!(
        rows.iter()
            .any(|r| r.contains("└ ls: nope: No such file or directory")),
        "without an error line, the first thing it printed: {rows:?}"
    );
}

/// The runtime passed verification, so a check that failed did not gate it.
/// "验证 2/3" under a ✓ left the user to guess which one and whether it
/// mattered; the line names it and says it does not block.
#[test]
fn a_passed_verification_names_the_check_that_did_not_block() {
    use leveler_client_protocol::{CheckState, UiCheck, UiVerification};
    let mut s = state();
    start(&mut s, "c");
    complete(&mut s, "c", true, 1_000, Some(0), None);
    let check = |name: &str, status| UiCheck {
        name: name.into(),
        status,
        evidence: None,
    };
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::VerificationUpdated {
            verification: UiVerification {
                checks: vec![
                    check("cargo fmt", CheckState::Failed),
                    check("cargo check", CheckState::Passed),
                    check("cargo test", CheckState::Passed),
                ],
                passed: Some(true),
            },
        }),
    );
    let message_id = leveler_client_protocol::MessageId::new("m");
    for event in [
        RuntimeEvent::AssistantMessageStarted {
            message_id: message_id.clone(),
        },
        RuntimeEvent::AssistantTextDelta {
            message_id: message_id.clone(),
            delta: "续期检查通过。".into(),
        },
        RuntimeEvent::AssistantMessageCompleted { message_id },
        RuntimeEvent::TurnCompleted,
    ] {
        reduce(&mut s, Action::Runtime(event));
    }
    let rows = plain(&s);
    let end = rows
        .iter()
        .find(|r| r.contains("任务已完成"))
        .unwrap_or_else(|| panic!("{rows:?}"));
    assert!(end.contains("验证 ✓"), "{end}");
    assert!(end.contains("cargo fmt 未通过（不阻断）"), "{end}");
    assert!(!end.contains("2/3"), "{end}");
}

/// The diff is whatever `/diff` last fetched; nothing refreshes it when a turn
/// ends. A later turn's end line must not repeat that old count as if the turn
/// had changed those files.
#[test]
fn a_turn_end_does_not_repeat_an_earlier_diff() {
    use leveler_client_protocol::{UiDiff, UiDiffFile};
    let mut s = state();
    let answer = |s: &mut AppState, id: &str| {
        let message_id = leveler_client_protocol::MessageId::new(id);
        for event in [
            RuntimeEvent::AssistantMessageStarted {
                message_id: message_id.clone(),
            },
            RuntimeEvent::AssistantTextDelta {
                message_id: message_id.clone(),
                delta: "好了。".into(),
            },
            RuntimeEvent::AssistantMessageCompleted { message_id },
            RuntimeEvent::TurnCompleted,
        ] {
            reduce(s, Action::Runtime(event));
        }
    };
    start(&mut s, "a");
    complete(&mut s, "a", true, 1_000, Some(0), None);
    let file = |path: &str| UiDiffFile {
        path: path.into(),
        added: 1,
        removed: 0,
        patch: None,
    };
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::DiffUpdated {
            diff: UiDiff {
                files: (0..7).map(|i| file(&format!("f{i}.rs"))).collect(),
            },
        }),
    );
    answer(&mut s, "m1");
    assert!(
        plain(&s).iter().any(|r| r.contains("7 个文件")),
        "the turn that saw the diff may say so: {:?}",
        plain(&s)
    );

    start(&mut s, "b");
    complete(&mut s, "b", true, 1_000, Some(0), None);
    answer(&mut s, "m2");
    let ends: Vec<String> = plain(&s)
        .into_iter()
        .filter(|r| r.contains("任务已完成"))
        .collect();
    assert_eq!(ends.len(), 2, "{ends:?}");
    assert!(!ends[1].contains("个文件"), "{ends:?}");
}

#[test]
fn the_network_permission_tag_is_the_one_the_runtime_writes() {
    assert_eq!(
        crate::activity_stream::NETWORK_PERMISSION_REQUIRED,
        leveler_tools::recoverable::NETWORK_PERMISSION_REQUIRED
    );
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

/// A clean mixed stretch no longer wears a summary header, so its FIRST tool
/// row is what a click lands on. The group still opens — hiding a redundant
/// row must not cost the only mouse route to a call's output.
#[test]
fn clicking_a_headerless_mixed_group_still_opens_it() {
    let mut s = state();
    s.transcript.push_user("看看状态".into());
    for (id, name, args) in [
        ("t1", "read_file", r#"{"path":"README.md"}"#),
        ("t2", "grep", r#"{"pattern":"TaskStatus"}"#),
    ] {
        reduce(
            &mut s,
            Action::Runtime(RuntimeEvent::ToolCallStarted {
                id: ToolCallId::new(id),
                name: name.into(),
                arguments: args.into(),
                parallel: false,
            }),
        );
        reduce(
            &mut s,
            Action::Runtime(RuntimeEvent::ToolCallCompleted {
                id: ToolCallId::new(id),
                ok: true,
                preview: format!("{id} output line\n"),
                duration_ms: 5,
                applied_diff: None,
                exit_code: None,
                stop: None,
            }),
        );
    }
    reduce(&mut s, Action::Runtime(RuntimeEvent::TurnAnswered));

    // No summary row: the first row of the group is the read itself.
    assert!(
        !plain(&s).iter().any(|l| l.contains("检查代码库")),
        "{:?}",
        plain(&s)
    );
    let (line, _) = row_with(&s, "README.md").expect("the read row");
    assert!(
        !plain(&s)[..line].iter().any(|l| l.contains('\u{25b8}')),
        "nothing folded sits above it: {:?}",
        plain(&s)
    );

    let (col, row) = cell_of(&s, "README.md");
    click(&mut s, col, row);
    assert!(
        plain(&s).iter().any(|l| l.contains("t1 output line"))
            && plain(&s).iter().any(|l| l.contains("t2 output line")),
        "the click opened the whole group: {:?}",
        plain(&s)
    );
    // The open group is taller, so the row moved: ask again where it is.
    let (col, row) = cell_of(&s, "README.md");
    click(&mut s, col, row);
    assert!(
        !plain(&s).iter().any(|l| l.contains("t1 output line")),
        "and closes it again: {:?}",
        plain(&s)
    );
    // Presentation only: both calls are still two calls.
    assert_eq!(s.transcript.tool_calls().len(), 2);
}
