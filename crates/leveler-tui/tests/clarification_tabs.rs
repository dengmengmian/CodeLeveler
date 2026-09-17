//! The clarification tab interaction, end to end through the reducer and the
//! renderer: several questions answered as tabs, keyboard ownership while the
//! interaction is open, and the resolved record a replay produces.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_width::UnicodeWidthStr;

use leveler_client_protocol::{
    ClarificationId, ClarificationQuestionKind, ClientCommand, RuntimeEvent, SessionId, ToolCallId,
    UiClarificationQuestion, UiClarificationRequest,
};
use leveler_tui::action::{Action, Effect};
use leveler_tui::overlay::Overlay;
use leveler_tui::reducer::reduce;
use leveler_tui::state::{AppState, Boot};
use leveler_tui::theme::Theme;

fn state() -> AppState {
    AppState::new(
        Theme::no_color(),
        Boot {
            session_id: SessionId::new("s1"),
            user: "u".into(),
            version: "0.1.0".into(),
            show_welcome: false,
            draft_path: None,
            history_path: None,
            context_window: 200_000,
            locale: leveler_tui::Locale::Zh,
            untrusted_config: Vec::new(),
            reasoning_effort: None,
        },
    )
}

fn key(code: KeyCode) -> Action {
    Action::Key(KeyEvent::new(code, KeyModifiers::empty()))
}

fn back_tab() -> Action {
    Action::Key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::empty()))
}

fn question(
    header: &str,
    kind: ClarificationQuestionKind,
    options: &[&str],
) -> UiClarificationQuestion {
    UiClarificationQuestion {
        header: header.into(),
        question: format!("{header}方式"),
        kind,
        options: options.iter().map(|s| (*s).to_string()).collect(),
        allow_other: false,
        min_choices: 0,
        max_choices: None,
    }
}

/// The three-question interaction used across these tests: one single choice,
/// one multi choice, one free text.
fn three_questions() -> UiClarificationRequest {
    UiClarificationRequest {
        id: ClarificationId::new("c1"),
        question: "需要你的选择".into(),
        options: Vec::new(),
        questions: vec![
            question(
                "数据策略",
                ClarificationQuestionKind::Single,
                &["保留 demo fallback", "删除全部 mock", "保持现状"],
            ),
            question(
                "验证范围",
                ClarificationQuestionKind::Multi,
                &["单元测试", "TUI 测试", "workspace test"],
            ),
            question("未登录态", ClarificationQuestionKind::Text, &[]),
        ],
    }
}

fn open(state: &mut AppState, request: UiClarificationRequest) {
    reduce(
        state,
        Action::Runtime(RuntimeEvent::ClarificationRequested { request }),
    );
}

fn rendered(state: &mut AppState, w: u16, h: u16) -> String {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| leveler_tui::render::render(f, state))
        .unwrap();
    let buf = term.backend().buffer();
    let mut out = String::new();
    for y in 0..h {
        let mut x = 0u16;
        while x < w {
            let sym = buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" ");
            out.push_str(sym);
            x += UnicodeWidthStr::width(sym).max(1) as u16;
        }
        out.push('\n');
    }
    out
}

fn cursor_at(state: &mut AppState, w: u16, h: u16) -> (String, (u16, u16)) {
    use ratatui::Terminal;
    use ratatui::backend::{Backend as _, TestBackend};
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| leveler_tui::render::render(f, state))
        .unwrap();
    let pos = term.backend_mut().get_cursor_position().unwrap();
    let buf = term.backend().buffer();
    let mut out = String::new();
    for y in 0..h {
        let mut x = 0u16;
        while x < w {
            let sym = buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" ");
            out.push_str(sym);
            x += UnicodeWidthStr::width(sym).max(1) as u16;
        }
        out.push('\n');
    }
    (out, (pos.x, pos.y))
}

fn active_question(state: &AppState) -> usize {
    match &state.overlay {
        Some(Overlay::Clarification(ov)) => ov.active(),
        other => panic!("expected an open clarification, got {other:?}"),
    }
}

fn answered_count(state: &AppState) -> usize {
    match &state.overlay {
        Some(Overlay::Clarification(ov)) => ov.answered_count(),
        other => panic!("expected an open clarification, got {other:?}"),
    }
}

/// Enter every answer of the three-question interaction so the final Enter
/// submits. Returns the submitted answer.
fn answer_all(state: &mut AppState) -> String {
    reduce(state, key(KeyCode::Enter)); // 数据策略 → option 0
    reduce(state, key(KeyCode::Char(' '))); // 验证范围 → 单元测试
    reduce(state, key(KeyCode::Down));
    reduce(state, key(KeyCode::Char(' '))); // …and TUI 测试
    reduce(state, key(KeyCode::Enter)); // confirm the multi
    for c in "展示登录引导".chars() {
        reduce(state, key(KeyCode::Char(c)));
    }
    let effects = reduce(state, key(KeyCode::Enter));
    match effects.as_slice() {
        [
            Effect::SendInteraction {
                command: ClientCommand::AnswerClarification { answer, .. },
                ..
            },
        ] => answer.clone(),
        other => panic!("expected the interaction to submit, got {other:?}"),
    }
}

#[test]
fn a_multi_question_request_shows_tabs_with_current_and_pending_states() {
    let mut s = state();
    open(&mut s, three_questions());
    let frame = rendered(&mut s, 100, 30);
    assert!(frame.contains("需要你的选择"), "headline missing:\n{frame}");
    assert!(frame.contains("0/3"), "progress missing:\n{frame}");
    for header in ["数据策略", "验证范围", "未登录态"] {
        assert!(frame.contains(header), "tab {header} missing:\n{frame}");
    }
    // One current tab, two pending; no answered yet.
    assert_eq!(frame.matches('●').count(), 1, "{frame}");
    assert_eq!(frame.matches('○').count(), 2, "{frame}");
    // Only the current question is on screen.
    assert!(frame.contains("数据策略方式"), "{frame}");
    assert!(!frame.contains("验证范围方式"), "{frame}");
}

#[test]
fn answering_a_question_marks_its_tab_done_and_advances() {
    let mut s = state();
    open(&mut s, three_questions());
    reduce(&mut s, key(KeyCode::Enter));
    assert_eq!(answered_count(&s), 1);
    assert_eq!(
        active_question(&s),
        1,
        "Enter advances to the next question"
    );
    let frame = rendered(&mut s, 100, 30);
    assert!(frame.contains("1/3"), "{frame}");
    assert_eq!(frame.matches('✓').count(), 1, "the settled tab: {frame}");
    assert_eq!(
        frame.matches('○').count(),
        1,
        "the last tab is pending: {frame}"
    );
}

#[test]
fn tab_and_shift_tab_switch_questions() {
    let mut s = state();
    open(&mut s, three_questions());
    reduce(&mut s, key(KeyCode::Tab));
    assert_eq!(active_question(&s), 1);
    reduce(&mut s, back_tab());
    assert_eq!(active_question(&s), 0);
    reduce(&mut s, back_tab());
    assert_eq!(active_question(&s), 2, "shift+tab wraps backwards");
}

/// Arrows walk the current question and stay there: they never scroll the
/// transcript and never leak to another question.
#[test]
fn arrows_move_only_the_current_questions_cursor() {
    let mut s = state();
    s.size = (100, 30);
    open(&mut s, three_questions());
    let scroll_before = s.conv.scroll;
    reduce(&mut s, key(KeyCode::Down));
    let frame = rendered(&mut s, 100, 30);
    assert!(frame.contains("❯ 删除全部 mock"), "{frame}");
    assert_eq!(
        s.conv.scroll, scroll_before,
        "the transcript must not scroll under an open interaction"
    );
    assert_eq!(active_question(&s), 0);
}

#[test]
fn a_single_choice_enter_submits_that_options_label() {
    let mut s = state();
    open(&mut s, three_questions());
    reduce(&mut s, key(KeyCode::Down));
    reduce(&mut s, key(KeyCode::Down));
    // Answer the rest so the interaction resolves, then check the first line.
    reduce(&mut s, key(KeyCode::Enter));
    reduce(&mut s, key(KeyCode::Enter));
    let effects = reduce(&mut s, key(KeyCode::Enter));
    let Effect::SendInteraction {
        command: ClientCommand::AnswerClarification { answer, .. },
        ..
    } = &effects[0]
    else {
        panic!("{effects:?}");
    };
    assert!(
        answer.starts_with("数据策略: 保持现状"),
        "the focused option is the answer: {answer:?}"
    );
}

#[test]
fn the_last_answer_submits_every_question_labelled() {
    let mut s = state();
    open(&mut s, three_questions());
    let answer = answer_all(&mut s);
    assert_eq!(
        answer,
        "数据策略: 保留 demo fallback\n验证范围: 单元测试, TUI 测试\n未登录态: 展示登录引导"
    );
    assert!(s.overlay.is_none(), "the interaction collapses when done");
}

#[test]
fn space_and_enter_submit_a_multi_choice_set() {
    let mut s = state();
    open(&mut s, three_questions());
    reduce(&mut s, key(KeyCode::Tab));
    reduce(&mut s, key(KeyCode::Char(' ')));
    reduce(&mut s, key(KeyCode::Down));
    reduce(&mut s, key(KeyCode::Char(' ')));
    let frame = rendered(&mut s, 100, 30);
    assert_eq!(frame.matches("[x]").count(), 2, "{frame}");
    assert_eq!(frame.matches("[ ]").count(), 1, "{frame}");
    // Focus and selection are separate marks: the cursor is on the second row
    // while both are selected.
    assert!(frame.contains("❯"), "{frame}");
}

#[test]
fn a_multi_choice_minimum_blocks_enter_and_says_why() {
    let mut s = state();
    let mut request = three_questions();
    request.questions[1].min_choices = 1;
    open(&mut s, request);
    reduce(&mut s, key(KeyCode::Tab));
    assert!(
        reduce(&mut s, key(KeyCode::Enter)).is_empty(),
        "an unmet minimum must not submit"
    );
    assert_eq!(answered_count(&s), 0);
    let frame = rendered(&mut s, 100, 30);
    assert!(
        frame.contains("至少选择 1 项才能确认"),
        "why is not shown:\n{frame}"
    );
    reduce(&mut s, key(KeyCode::Char(' ')));
    let frame = rendered(&mut s, 100, 30);
    assert!(
        !frame.contains("才能确认"),
        "the refusal clears once the pick is made: {frame}"
    );
}

#[test]
fn a_text_question_takes_typing_and_submits() {
    let mut s = state();
    open(&mut s, three_questions());
    reduce(&mut s, key(KeyCode::Enter));
    reduce(&mut s, key(KeyCode::Enter));
    for c in "还有其他要求".chars() {
        reduce(&mut s, key(KeyCode::Char(c)));
    }
    let frame = rendered(&mut s, 100, 30);
    assert!(frame.contains("还有其他要求"), "{frame}");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    let Effect::SendInteraction {
        command: ClientCommand::AnswerClarification { answer, .. },
        ..
    } = &effects[0]
    else {
        panic!("{effects:?}");
    };
    assert!(answer.ends_with("未登录态: 还有其他要求"), "{answer:?}");
}

/// Esc ends the interaction as a skip. It must not reach the main run's
/// cancellation path.
#[test]
fn esc_skips_the_interaction_without_cancelling_the_task() {
    let mut s = state();
    s.status = leveler_client_protocol::RuntimeStatus::Busy;
    open(&mut s, three_questions());
    let effects = reduce(&mut s, key(KeyCode::Esc));
    match effects.as_slice() {
        [
            Effect::SendInteraction {
                command: ClientCommand::AnswerClarification { answer, .. },
                ..
            },
        ] => assert!(answer.is_empty(), "Esc is a skip: {answer:?}"),
        other => panic!("Esc must skip the question, not cancel the run: {other:?}"),
    }
    assert!(
        !effects.iter().any(|e| format!("{e:?}").contains("Cancel")),
        "{effects:?}"
    );
}

/// The overlay is top-most: the composer behind it and the transcript below
/// both stay out of the way, and Enter cannot send a message.
#[test]
fn the_overlay_owns_the_composer_and_transcript_keys() {
    let mut s = state();
    s.size = (100, 30);
    open(&mut s, three_questions());
    let scroll_before = s.conv.scroll;
    for action in [
        key(KeyCode::Char('x')),
        key(KeyCode::Char(' ')),
        key(KeyCode::Up),
        key(KeyCode::Down),
    ] {
        let effects = reduce(&mut s, action);
        assert!(
            effects.is_empty(),
            "no command leaves the overlay: {effects:?}"
        );
    }
    assert!(s.composer.is_empty(), "the composer must not take the keys");
    assert_eq!(
        s.conv.scroll, scroll_before,
        "the transcript must not scroll"
    );
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(
        !effects.iter().any(|e| matches!(e, Effect::Submit { .. })),
        "Enter confirms the question, it does not send a message: {effects:?}"
    );
}

#[test]
fn a_text_question_puts_the_terminal_cursor_after_what_was_typed() {
    let mut s = state();
    open(&mut s, three_questions());
    reduce(&mut s, key(KeyCode::Enter)); // 数据策略
    reduce(&mut s, key(KeyCode::Enter)); // 验证范围 (no minimum)
    assert_eq!(active_question(&s), 2, "the text question is up");
    for c in "ab".chars() {
        reduce(&mut s, key(KeyCode::Char(c)));
    }
    let (frame, (x, y)) = cursor_at(&mut s, 96, 30);
    let row = frame.lines().nth(y as usize).unwrap_or("");
    let before: String = row.chars().take(x as usize).collect();
    assert!(
        before.trim_end().ends_with("> ab"),
        "the cursor sits after the typed text at ({x},{y}): {before:?}\n{frame}"
    );
}

#[test]
fn a_replayed_clarification_shows_the_answers_with_no_active_focus() {
    let mut s = state();
    // History: the call the model made, and the answer the user gave.
    let arguments = serde_json::json!({
        "question": "需要你的选择",
        "questions": [
            {"header": "数据策略", "question": "数据策略方式", "kind": "single",
             "options": ["保留 demo fallback", "删除全部 mock"]},
        ],
    })
    .to_string();
    for event in [
        RuntimeEvent::ToolCallStarted {
            id: ToolCallId::new("t1"),
            name: "request_user_input".into(),
            arguments,
            parallel: false,
        },
        RuntimeEvent::ToolCallCompleted {
            id: ToolCallId::new("t1"),
            ok: true,
            preview: "数据策略: 保留 demo fallback\n未登录态: 展示登录引导".into(),
            duration_ms: 10,
            applied_diff: None,
            exit_code: None,
            stop: None,
        },
    ] {
        reduce(&mut s, Action::Runtime(event));
    }
    assert!(
        s.overlay.is_none(),
        "a replayed answer must never re-open the interaction"
    );
    let frame = rendered(&mut s, 100, 30);
    assert!(frame.contains("需要你的选择"), "{frame}");
    assert!(frame.contains("保留 demo fallback"), "{frame}");
    assert!(frame.contains("展示登录引导"), "{frame}");
    assert!(
        !frame.contains('❯'),
        "history has no focus cursor:\n{frame}"
    );
}

#[test]
fn long_cjk_options_wrap_under_their_own_indent() {
    let mut s = state();
    let mut request = three_questions();
    request.questions[0].options = vec![
        "把登录后的所有读取都改走真实接口，并且保留 demo 数据作为离线回退，只在网络不可用时使用"
            .into(),
    ];
    open(&mut s, request);
    let frame = rendered(&mut s, 60, 30);
    // The label continues on the next row, aligned under the label itself
    // rather than under the cursor margin.
    let rows: Vec<&str> = frame.lines().collect();
    let first = rows
        .iter()
        .position(|r| r.contains("把登录后的所有读取"))
        .expect("the option's first row");
    let next = rows[first + 1];
    let first_col = rows[first].chars().position(|c| c == '把').unwrap();
    let cont_col = next
        .chars()
        .position(|c| c != ' ')
        .expect("a continuation row with text");
    assert_eq!(
        first_col, cont_col,
        "continuation must align under the label:\n{frame}"
    );
}

#[test]
fn a_narrow_terminal_keeps_the_current_tab_visible_and_does_not_panic() {
    let mut s = state();
    open(&mut s, three_questions());
    // Walk to the last question, the worst case for the strip: it must scroll
    // the window instead of dropping the tab you are on.
    reduce(&mut s, key(KeyCode::Tab));
    reduce(&mut s, key(KeyCode::Tab));
    // Height is a separate axis: the overlay fits the composer's slot, and a
    // terminal too short for it clips rather than panics (the same budget the
    // approval overlay has always used).
    for (w, h) in [(80u16, 24u16), (48, 20), (30, 24), (20, 24)] {
        let frame = rendered(&mut s, w, h);
        assert!(
            frame.contains('●'),
            "the current tab's marker must survive at {w}x{h}:\n{frame}"
        );
        if w <= 30 {
            assert!(
                frame.contains('…'),
                "clipping must be said at {w}x{h}:\n{frame}"
            );
        }
    }
    // A resize is just another frame; no state to repair, no panic.
    for (w, h) in [(20u16, 10u16), (30, 12), (12, 8), (200, 60)] {
        reduce(&mut s, Action::Resize(w, h));
        let _ = rendered(&mut s, w, h);
    }
    let wide = rendered(&mut s, 120, 40);
    assert!(
        wide.contains("数据策略") && wide.contains("未登录态"),
        "{wide}"
    );
}

#[test]
fn an_answered_question_can_be_reopened_and_changed() {
    let mut s = state();
    open(&mut s, three_questions());
    reduce(&mut s, key(KeyCode::Enter)); // 数据策略 → option 0
    assert_eq!(answered_count(&s), 1);
    // Go back and change it.
    reduce(&mut s, back_tab());
    assert_eq!(active_question(&s), 0);
    reduce(&mut s, key(KeyCode::Down));
    reduce(&mut s, key(KeyCode::Enter));
    // The change is recorded, and the interaction is still waiting on the rest.
    assert_eq!(answered_count(&s), 1);
    reduce(&mut s, back_tab());
    let frame = rendered(&mut s, 100, 30);
    let chosen: Vec<&str> = frame.lines().filter(|row| row.contains('✓')).collect();
    assert_eq!(chosen.len(), 1, "one recorded pick: {frame}");
    assert!(
        chosen[0].contains("删除全部 mock"),
        "the changed answer replaced the old one: {frame}"
    );
}
