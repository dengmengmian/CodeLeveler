//! The reported Dogfood bug: a ToolGroup going open → closed inserted its
//! parent row and re-indented every child under it — a visible screen reflow.
//!
//! These tests drive the REAL reducer and the REAL conversation builder, so the
//! geometry they assert is the geometry the terminal paints. A group's parent
//! row is structural: it must exist from the first call, and closing the group
//! may only rewrite its text.

use leveler_client_protocol::{
    MessageId, RuntimeEvent, SessionId, ToolCallId, UiMessage, UiRole, UiSessionSnapshot,
};
use leveler_tui::action::Action;
use leveler_tui::conversation::build::build_conversation_lines_with_hits;
use leveler_tui::reducer::reduce;
use leveler_tui::state::{AppState, Boot};
use leveler_tui::theme::Theme;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

const W: usize = 100;

fn opened() -> AppState {
    let mut s = AppState::new(
        Theme::no_color(),
        Boot {
            session_id: SessionId::new("s1"),
            user: "麻凡".into(),
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
    let snap = UiSessionSnapshot {
        id: SessionId::new("s1"),
        repository: Some("~/x".into()),
        task_status: None,
        task_terminal: None,
        goal: "g".into(),
        model: leveler_client_protocol::ModelRef::parse("deepseek/v3"),
        mode: leveler_client_protocol::PermissionProfile::Assisted,
        branch: Some("main".into()),
        status: "idle".into(),
        finalization_stage: None,
        messages: Vec::new(),
        pending_interactions: Vec::new(),
        available_models: Vec::new(),
        vision: false,
        last_sequence: None,
        active_tools: Vec::new(),
        active_background_tasks: Vec::new(),
        plan: None,
        diff: None,
        checkpoints: Vec::new(),
        recaps: Vec::new(),
        user_shells: Vec::new(),
        completion_report: None,
        thinking: None,
        work_profile: None,
        collaboration: None,
        children: Vec::new(),
    };
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened { session: snap }),
    );
    s
}

fn lines(s: &AppState) -> Vec<String> {
    build_conversation_lines_with_hits(s, W)
        .0
        .into_iter()
        .map(|l| l.spans.iter().map(|sp| sp.content.as_ref()).collect())
        .collect()
}

const H: u16 = 30;

/// The FULL painted screen, the way the terminal shows it. This is what the
/// reader actually sees shift, so the reflow assertion is made against it, not
/// only against the conversation line list.
fn screen(s: &mut AppState) -> Vec<String> {
    let backend = TestBackend::new(W as u16, H);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|f| leveler_tui::render::render(f, s)).unwrap();
    let buf = term.backend().buffer();
    (0..H)
        .map(|y| {
            let mut line = String::new();
            let mut x = 0u16;
            while x < W as u16 {
                let sym = buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" ");
                line.push_str(sym);
                x += unicode_width::UnicodeWidthStr::width(sym).max(1) as u16;
            }
            line.trim_end().to_string()
        })
        .collect()
}

/// The screen row carrying `needle`.
fn screen_row(screen: &[String], needle: &str) -> usize {
    screen
        .iter()
        .position(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("no screen row with {needle:?}: {screen:?}"))
}

fn start(s: &mut AppState, id: &str, args: &str) {
    reduce(
        s,
        Action::Runtime(RuntimeEvent::ToolCallStarted {
            id: ToolCallId::new(id),
            name: "run_command".into(),
            arguments: args.into(),
            parallel: false,
            model_step: None,
            answer_effect: None,
        }),
    );
}

fn finish(s: &mut AppState, id: &str, ms: u64) {
    reduce(
        s,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            exit_code: None,
            stop: None,
            id: ToolCallId::new(id),
            ok: true,
            preview: "ok".into(),
            duration_ms: ms,
            applied_diff: None,
        }),
    );
}

fn next_assistant(s: &mut AppState, id: &str) {
    reduce(
        s,
        Action::Runtime(RuntimeEvent::AssistantMessageStarted {
            message_id: MessageId::new(id),
        }),
    );
}

/// The row index of the group's stage parent (`⋮` running / `▸` closed).
fn parent_row(lines: &[String]) -> usize {
    lines
        .iter()
        .position(|l| {
            let t = l.trim_start();
            t.starts_with('\u{22ee}') || t.starts_with('\u{25b8}')
        })
        .unwrap_or_else(|| panic!("no stage parent row: {lines:?}"))
}

/// The starting column of the group's first child row.

/// The number of rows the group occupies, parent included.
///
/// A group's own summary row sits at the transcript baseline (`›`/`⋮`/`▸`),
/// exactly like `▌` for the user and `●` for the agent; only its children step
/// in one level, so `GROUP_BODY_INDENT` (2 columns) is the tell.
fn group_rows(lines: &[String], from: usize) -> usize {
    1 + lines[from + 1..]
        .iter()
        .take_while(|l| l.starts_with("  ") || l.trim_start().starts_with('\u{2514}'))
        .count()
}

#[test]
fn a_running_stage_keeps_its_geometry_from_first_call_to_close() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::UserMessageAdded {
            message: UiMessage {
                id: MessageId::new("u1"),
                role: UiRole::User,
                text: "跑一下 check 和 test".into(),
                ordinal: None,
                kind: None,
                images: 0,
            },
        }),
    );
    next_assistant(&mut s, "a1");

    // t0: one command, still running — the group's FIRST second.
    start(&mut s, "c1", r#"{"program":"cargo","args":["check"]}"#);
    let t0 = lines(&s);

    // t1: the first command settled and a second is in flight; still open.
    finish(&mut s, "c1", 200);
    start(&mut s, "c2", r#"{"program":"cargo","args":["test"]}"#);
    let t1 = lines(&s);

    // t2: the second settles and the next assistant message closes the group.
    finish(&mut s, "c2", 200);
    next_assistant(&mut s, "a2");
    let t2 = lines(&s);

    let p0 = parent_row(&t0);
    let p1 = parent_row(&t1);
    let p2 = parent_row(&t2);

    // The receipt header is present in all three, and closing never inserts one.
    assert!(t0[p0].contains("正在执行"), "t0 live: {t0:?}");
    assert!(t1[p1].contains("正在执行"), "t1 live: {t1:?}");
    assert!(t2[p2].contains("执行了 2 个命令"), "t2 closed: {t2:?}");
    assert_eq!(p1, p2, "completion keeps the receipt in place");

    // A receipt is one row whether it runs or settles: its members are
    // reachable by opening it, never by growing the conversation.
    assert_eq!(1, group_rows(&t0, p0), "t0:{t0:?}");
    assert_eq!(1, group_rows(&t1, p1), "t1:{t1:?}");
    assert_eq!(1, group_rows(&t2, p2), "t2:{t2:?}");

    // Only the wording changed, and the settled receipt names what it ran
    // without inlining the assembled command.
    assert!(t1[p1].contains("正在执行 1 项"), "t1: {t1:?}");
    assert!(
        t2[p2].contains("执行了 2 个命令") && !t2.iter().any(|l| l.contains("$ cargo")),
        "t2: {t2:?}"
    );
}

/// Real calls are often faster than a paint: a burst of 0ms commands must not
/// produce a different geometry at any point in its life than a slow one.
#[test]
fn a_burst_of_instant_commands_keeps_the_same_geometry_at_every_step() {
    let mut s = opened();
    next_assistant(&mut s, "a1");
    let mut seen: Vec<(usize, usize)> = Vec::new();
    for (i, id) in ["c1", "c2", "c3"].into_iter().enumerate() {
        start(
            &mut s,
            id,
            &format!(r#"{{"program":"echo","args":["{i}"]}}"#),
        );
        let snap = lines(&s);
        let p = parent_row(&snap);
        seen.push((p, group_rows(&snap, p)));
        finish(&mut s, id, 0);
    }
    next_assistant(&mut s, "a2");
    let closed = lines(&s);
    let p = parent_row(&closed);
    assert_eq!(p, seen[0].0);
    assert_eq!(
        group_rows(&closed, p),
        1,
        "instant commands also fold once closed"
    );

    for (i, (row, rows)) in seen.iter().enumerate() {
        assert_eq!(*row, seen[0].0, "step {i} moved the receipt: {seen:?}");
        assert_eq!(*rows, 1, "step {i} row count: {seen:?}");
    }
}

/// The whole thing on the painted screen: a fast run of commands must not move
/// the rows above or below the group when it closes. This is the assertion the
/// hand-test made by eye.
#[test]
fn the_painted_screen_does_not_shift_when_the_group_closes() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::UserMessageAdded {
            message: UiMessage {
                id: MessageId::new("u1"),
                role: UiRole::User,
                text: "先 check，再 test".into(),
                ordinal: None,
                kind: None,
                images: 0,
            },
        }),
    );
    next_assistant(&mut s, "a1");

    // t0: one command running.
    start(&mut s, "c1", r#"{"program":"cargo","args":["check"]}"#);
    let f0 = screen(&mut s);

    // t1: first settled, second running — the group is still open.
    finish(&mut s, "c1", 200);
    start(&mut s, "c2", r#"{"program":"cargo","args":["test"]}"#);
    let f1 = screen(&mut s);

    // t2: second settled, next assistant message closes the group.
    finish(&mut s, "c2", 200);
    next_assistant(&mut s, "a2");
    let f2 = screen(&mut s);

    // The group sits at the same DISTANCE from the prose above it in both
    // frames and at the same columns: closing inserted no row and moved
    // nothing. (Absolute screen rows shift up by one at t2 because the closing
    // assistant message is appended below and the viewport is bottom-anchored —
    // content arriving below the group is not the group reflowing.)
    // The parent row exists on the painted screen from the group's first
    // second, adjacent to the child it owns.
    // The receipt is one row from its first second, so the painted frame is
    // stable: settling changes the wording, never the geometry around it.
    let receipt = |f: &[String]| screen_row(f, "▸ 正在执行 1 项 · 执行了 1 个命令");
    assert_eq!(
        receipt(&f0),
        screen_row(&f1, "▸ 正在执行 1 项 · 执行了 2 个命令"),
        "t0 frame:\n{}\nt1 frame:\n{}",
        f0.join("\n"),
        f1.join("\n")
    );
    // The prose above the group is the anchor: the distance from it is what a
    // reader perceives as "nothing moved".
    let gap =
        |f: &[String], needle: &str| screen_row(f, needle) - screen_row(f, "▌ 先 check，再 test");
    assert_eq!(
        gap(&f1, "▸ 正在执行 1 项 · 执行了 2 个命令"),
        gap(&f2, "▸ 执行了 2 个命令"),
        "the group gained a row at close:\nt1:\n{}\nt2:\n{}",
        f1.join("\n"),
        f2.join("\n")
    );
    assert!(
        !f2.iter().any(|l| l.contains("$ cargo")),
        "the painted settled process is collapsed"
    );
}

/// The reported all-ok lie, driven through the REAL reducer: one command
/// succeeds, the next is stopped, and the closed parent announced `全部成功`
/// above its own `⊘ 已停止` row. Cancelled is not a failure, so counting
/// failures let it through.
#[test]
fn a_stopped_command_never_lets_the_stage_claim_all_ok() {
    let mut s = opened();
    next_assistant(&mut s, "a1");

    start(&mut s, "c1", r#"{"program":"cargo","args":["check"]}"#);
    finish(&mut s, "c1", 200);

    start(&mut s, "c2", r#"{"program":"sleep","args":["120"]}"#);
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            exit_code: None,
            stop: Some(leveler_client_protocol::UiCommandStop::Confirmed),
            id: ToolCallId::new("c2"),
            ok: false,
            preview: String::new(),
            duration_ms: 700,
            applied_diff: None,
        }),
    );
    next_assistant(&mut s, "a2");

    let t = lines(&s);
    let p = parent_row(&t);
    assert!(
        !t[p].contains("全部成功") && !t[p].contains("all ok"),
        "a stopped call is not a success: {t:?}"
    );
    // The work is counted without claiming success; full outcomes are still
    // reachable through the keyboard.
    assert!(t[p].contains("执行了 2 个命令"), "{t:?}");
    reduce(
        &mut s,
        Action::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('o'),
            crossterm::event::KeyModifiers::CONTROL,
        )),
    );
    assert!(
        lines(&s)
            .iter()
            .any(|l| l.contains("⊘") && l.contains("sleep 120"))
    );
}
