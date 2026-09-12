//! The contextual prompt suggestion, end to end.
//!
//! One product contract: a structured `next_step` shows up as ghost text, the
//! real input buffer stays empty, Tab moves the text into the buffer, and only
//! Enter sends it. These tests lock the contract — state, keys, lifecycle and
//! the rendered frame — not the internal field names.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use leveler_client_protocol::{
    ClientCommand, MessageId, PermissionProfile, RuntimeEvent, RuntimeStatus, SessionId,
    ToolCallId, UiSessionSnapshot,
};
use leveler_tui::action::{Action, Effect};
use leveler_tui::reducer::reduce;
use leveler_tui::screen::Screen;
use leveler_tui::state::{AppState, Boot, WorkbenchFocus};
use leveler_tui::suggestion;
use leveler_tui::theme::Theme;
use leveler_tui::transcript::TranscriptItem;

const NEXT_STEP: &str = "查看 demo-order-01 的里程碑进度";

fn snapshot() -> UiSessionSnapshot {
    UiSessionSnapshot {
        id: SessionId::new("s1"),
        repository: "/repo".to_string(),
        goal: "interactive session".to_string(),
        model: leveler_client_protocol::ModelRef::parse("deepseek/v3"),
        mode: PermissionProfile::Assisted,
        branch: Some("main".to_string()),
        status: "idle".to_string(),
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
    }
}

fn opened() -> AppState {
    let mut s = AppState::new(
        Theme::no_color(),
        Boot {
            session_id: SessionId::new("s1"),
            user: "麻凡".to_string(),
            version: "0.1.0".to_string(),
            show_welcome: false,
            draft_path: None,
            history_path: None,
            context_window: 200_000,
            locale: leveler_tui::Locale::Zh,
            untrusted_config: Vec::new(),
            reasoning_effort: None,
        },
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: snapshot(),
        }),
    );
    s
}

fn key(code: KeyCode) -> Action {
    Action::Key(KeyEvent::new(code, KeyModifiers::empty()))
}

fn shift(code: KeyCode) -> Action {
    Action::Key(KeyEvent::new(code, KeyModifiers::SHIFT))
}

/// The runtime going busy. There is no dedicated `TurnStarted` event: a turn
/// begins the first time the runtime reports work, which is what this is.
fn busy() -> RuntimeEvent {
    RuntimeEvent::AgentActivity {
        label: "分析上下文".to_string(),
    }
}

fn typed(s: &mut AppState, text: &str) {
    for ch in text.chars() {
        reduce(s, key(KeyCode::Char(ch)));
    }
}

/// Render the real renderer into a terminal buffer and read back both the
/// painted characters and the terminal cursor the frame placed, so assertions
/// are about what a terminal would show, not about view models.
fn draw(state: &mut AppState, w: u16, h: u16) -> (String, (u16, u16)) {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| leveler_tui::render::render(f, state))
        .unwrap();
    let caret = term
        .get_cursor_position()
        .map(|p| (p.x, p.y))
        .unwrap_or((0, 0));
    let buf = term.backend().buffer();
    let mut out = String::new();
    for y in 0..h {
        let mut x = 0u16;
        while x < w {
            let sym = buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" ");
            out.push_str(sym);
            x += unicode_width::UnicodeWidthStr::width(sym).max(1) as u16;
        }
        out.push('\n');
    }
    (out, caret)
}

fn frame(state: &mut AppState, w: u16, h: u16) -> String {
    draw(state, w, h).0
}

/// The composer row, as painted — the line carrying the `›` prompt.
fn composer_row(screen: &str) -> String {
    screen
        .lines()
        .find(|line| line.contains('›'))
        .unwrap_or_default()
        .trim_end()
        .to_string()
}

fn image_attachment() -> leveler_client_protocol::AttachmentRef {
    leveler_client_protocol::AttachmentRef {
        id: leveler_client_protocol::AttachmentId::new("a1"),
        kind: leveler_client_protocol::AttachmentKind::Image,
        name: "clipboard.png".to_string(),
        mime_type: "image/png".to_string(),
        size_bytes: 1024,
        sha256: "0".repeat(64),
        width: Some(64),
        height: Some(64),
    }
}

/// Complete a turn whose `update_goal` carried `status` and `next_step`.
fn goal_turn(s: &mut AppState, status: &str, next_step: Option<&str>) {
    let id = ToolCallId::new("goal");
    let mut args = serde_json::json!({ "status": status, "summary": "阶段一已经完成。" });
    if let Some(step) = next_step {
        args["next_step"] = serde_json::Value::String(step.to_string());
    }
    reduce(
        s,
        Action::Runtime(RuntimeEvent::ToolCallStarted {
            id: id.clone(),
            name: "update_goal".into(),
            arguments: args.to_string(),
            parallel: false,
        }),
    );
    reduce(
        s,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            id,
            ok: true,
            preview: "目标已更新".into(),
            duration_ms: 10,
            applied_diff: None,
        }),
    );
    reduce(s, Action::Runtime(RuntimeEvent::TurnCompleted));
}

/// A turn that ends on plain prose — no structured next step anywhere.
fn prose_turn(s: &mut AppState, text: &str) {
    let id = MessageId::new("m1");
    reduce(
        s,
        Action::Runtime(RuntimeEvent::AssistantMessageStarted {
            message_id: id.clone(),
        }),
    );
    reduce(
        s,
        Action::Runtime(RuntimeEvent::AssistantTextDelta {
            message_id: id.clone(),
            delta: text.into(),
        }),
    );
    reduce(
        s,
        Action::Runtime(RuntimeEvent::AssistantMessageCompleted { message_id: id }),
    );
    reduce(s, Action::Runtime(RuntimeEvent::TurnCompleted));
}

fn with_suggestion() -> AppState {
    let mut s = opened();
    goal_turn(&mut s, "complete", Some(NEXT_STEP));
    assert!(
        suggestion::is_visible(&s),
        "fixture must start with a visible ghost"
    );
    s
}

// ---- A. where the suggestion comes from ------------------------------------

#[test]
fn a1_complete_goal_with_next_step_offers_a_ghost_and_leaves_the_buffer_empty() {
    let mut s = opened();
    goal_turn(&mut s, "complete", Some(NEXT_STEP));

    assert_eq!(s.prompt_suggestion.as_deref(), Some(NEXT_STEP));
    assert!(s.composer.is_empty(), "the ghost is never buffer content");
    assert_eq!(s.composer.canonical_text(), "");
    assert!(suggestion::is_visible(&s));
}

#[test]
fn a2_blocked_goal_with_next_step_also_offers_a_ghost() {
    let mut s = opened();
    goal_turn(&mut s, "blocked", Some("补上缺失的凭据再继续"));

    assert_eq!(s.prompt_suggestion.as_deref(), Some("补上缺失的凭据再继续"));
    assert!(s.composer.is_empty());
}

#[test]
fn a3_incomplete_turn_without_a_next_step_falls_back_to_localized_continue() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::TurnIncomplete {
            reason: "本轮资源窗口已用完".into(),
        }),
    );
    assert_eq!(s.prompt_suggestion.as_deref(), Some("继续"));

    let mut en = AppState::new(
        Theme::no_color(),
        Boot {
            session_id: SessionId::new("s1"),
            user: "ma".to_string(),
            version: "0.1.0".to_string(),
            show_welcome: false,
            draft_path: None,
            history_path: None,
            context_window: 200_000,
            locale: leveler_tui::Locale::En,
            untrusted_config: Vec::new(),
            reasoning_effort: None,
        },
    );
    reduce(
        &mut en,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: snapshot(),
        }),
    );
    reduce(
        &mut en,
        Action::Runtime(RuntimeEvent::TurnIncomplete {
            reason: "budget exhausted".into(),
        }),
    );
    assert_eq!(
        en.prompt_suggestion.as_deref(),
        Some("Continue"),
        "the fallback is UI chrome and must be localized"
    );
}

#[test]
fn a4_normal_completion_without_a_structured_next_step_offers_nothing() {
    let mut s = opened();
    prose_turn(&mut s, "权限检查已经完成。\n\n下一步：运行标签预置脚本。");

    assert_eq!(
        s.prompt_suggestion, None,
        "assistant prose is never parsed into a suggestion"
    );
    assert!(!suggestion::is_visible(&s));
}

#[test]
fn a5_cancelled_and_failed_turns_offer_nothing() {
    let mut s = opened();
    let id = ToolCallId::new("goal");
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ToolCallStarted {
            id: id.clone(),
            name: "update_goal".into(),
            arguments: serde_json::json!({
                "status": "complete",
                "summary": "做完了。",
                "next_step": NEXT_STEP,
            })
            .to_string(),
            parallel: false,
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            id,
            ok: true,
            preview: "ok".into(),
            duration_ms: 5,
            applied_diff: None,
        }),
    );
    reduce(&mut s, Action::Runtime(RuntimeEvent::TurnCancelled));
    assert_eq!(
        s.prompt_suggestion, None,
        "a cancelled turn hands nothing on"
    );

    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::TurnFailed {
            error: "provider 502".into(),
        }),
    );
    assert_eq!(s.prompt_suggestion, None);
    assert!(!suggestion::is_visible(&s));
}

#[test]
fn a6_an_existing_draft_is_never_overwritten_nor_shadowed() {
    let mut s = opened();
    reduce(&mut s, Action::Runtime(busy()));
    typed(&mut s, "我自己想问的问题");
    goal_turn(&mut s, "complete", Some(NEXT_STEP));

    assert_eq!(s.composer.text(), "我自己想问的问题");
    assert_eq!(
        s.prompt_suggestion, None,
        "no stale ghost may be parked to appear the moment the draft clears"
    );

    // Clearing the draft by hand must not resurrect it.
    for _ in 0.."我自己想问的问题".chars().count() {
        reduce(&mut s, key(KeyCode::Backspace));
    }
    assert!(s.composer.is_empty());
    assert_eq!(s.prompt_suggestion, None);
}

// ---- B. Tab -----------------------------------------------------------------

#[test]
fn b1_tab_moves_the_suggestion_into_the_buffer_and_sends_nothing() {
    let mut s = with_suggestion();
    let effects = reduce(&mut s, key(KeyCode::Tab));

    assert_eq!(s.composer.text(), NEXT_STEP);
    assert_eq!(
        s.composer.cursor(),
        NEXT_STEP.chars().count(),
        "caret lands at the end, ready to edit"
    );
    assert_eq!(s.prompt_suggestion, None);
    assert!(effects.is_empty(), "Tab sends nothing: {effects:?}");
    assert_eq!(s.workbench_focus, WorkbenchFocus::Input, "focus stays put");
    assert!(
        s.composer.history().is_empty(),
        "accepting is not submitting: no history entry"
    );
}

#[test]
fn b2_enter_after_tab_submits_exactly_once() {
    let mut s = with_suggestion();

    // Enter BEFORE Tab: the ghost is not input, so nothing is sent.
    let before = reduce(&mut s, key(KeyCode::Enter));
    assert!(
        before.is_empty(),
        "Enter on an empty buffer must not send the ghost: {before:?}"
    );
    assert!(
        s.transcript
            .items()
            .iter()
            .all(|item| !matches!(item, TranscriptItem::User(_))),
        "no user message may exist before Tab + Enter"
    );

    // Tab accepts, then Enter submits — once.
    reduce(&mut s, key(KeyCode::Tab));
    let sent = reduce(&mut s, key(KeyCode::Enter));
    let submissions: Vec<_> = sent
        .iter()
        .filter(|effect| {
            matches!(
                effect,
                Effect::Send(ClientCommand::SubmitMessage { content, .. }) if content == NEXT_STEP
            )
        })
        .collect();
    assert_eq!(submissions.len(), 1, "exactly one submission: {sent:?}");
    assert!(s.composer.is_empty());
    assert_eq!(
        s.composer.history(),
        &[NEXT_STEP.to_string()],
        "the accepted text enters history once, as normal input"
    );
}

#[test]
fn b3_without_a_suggestion_tab_still_switches_focus() {
    let mut s = opened();
    assert_eq!(s.prompt_suggestion, None);
    reduce(&mut s, key(KeyCode::Tab));
    assert_eq!(s.workbench_focus, WorkbenchFocus::Conversation);
}

#[test]
fn b4_file_popup_wins_tab_over_the_suggestion() {
    let mut s = with_suggestion();
    s.file_candidates = vec!["crates/leveler-tui/src/composer.rs".to_string()];
    typed(&mut s, "@composer");
    assert_eq!(
        s.prompt_suggestion, None,
        "typing already destroyed the ghost"
    );

    reduce(&mut s, key(KeyCode::Tab));
    assert!(
        s.composer
            .text()
            .contains("@crates/leveler-tui/src/composer.rs"),
        "Tab completed the file mention: {}",
        s.composer.text()
    );
    assert!(!s.composer.text().contains(NEXT_STEP));
}

#[test]
fn b5_slash_popup_wins_tab_over_the_suggestion() {
    let mut s = with_suggestion();
    typed(&mut s, "/mod");
    assert_eq!(s.prompt_suggestion, None);

    reduce(&mut s, key(KeyCode::Tab));
    assert!(
        s.composer.text().starts_with("/model"),
        "Tab completed the slash command: {}",
        s.composer.text()
    );
}

#[test]
fn b6_shift_tab_still_cycles_the_permission_profile() {
    let mut s = with_suggestion();
    let effects = reduce(&mut s, shift(KeyCode::Tab));

    assert!(
        effects.iter().any(|effect| matches!(
            effect,
            Effect::Send(ClientCommand::SetPermissionProfile { .. })
        )),
        "Shift+Tab is permission cycling, nothing else: {effects:?}"
    );
    assert!(
        s.composer.is_empty(),
        "Shift+Tab must never accept the suggestion"
    );
    assert_eq!(
        s.prompt_suggestion.as_deref(),
        Some(NEXT_STEP),
        "and it must not dismiss it either"
    );

    let effects = reduce(
        &mut s,
        Action::Key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::empty())),
    );
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::Send(ClientCommand::SetPermissionProfile { .. })
    )));
    assert!(s.composer.is_empty());
}

// ---- C. the user edits instead ---------------------------------------------

#[test]
fn c1_typing_a_character_destroys_the_ghost_and_keeps_only_what_was_typed() {
    let mut s = with_suggestion();
    typed(&mut s, "这个订单");

    assert_eq!(s.composer.text(), "这个订单");
    assert_eq!(s.prompt_suggestion, None);
    assert!(!suggestion::is_visible(&s));
}

#[test]
fn c2_a_coalesced_text_burst_destroys_the_ghost() {
    let mut s = with_suggestion();
    reduce(&mut s, Action::TextInput("这个订单".to_string()));

    assert_eq!(s.composer.text(), "这个订单");
    assert_eq!(s.prompt_suggestion, None);
}

#[test]
fn c3_paste_destroys_the_ghost() {
    let mut s = with_suggestion();
    reduce(&mut s, Action::Paste("粘贴的内容".to_string()));

    assert_eq!(s.composer.text(), "粘贴的内容");
    assert_eq!(s.prompt_suggestion, None);
}

#[test]
fn c4_backspace_and_delete_on_an_empty_buffer_dismiss_the_ghost() {
    let mut s = with_suggestion();
    reduce(&mut s, key(KeyCode::Backspace));
    assert_eq!(s.prompt_suggestion, None);
    assert!(s.composer.is_empty());
    assert_eq!(s.composer.cursor(), 0);
    assert!(s.composer.history().is_empty());

    let mut s = with_suggestion();
    reduce(&mut s, key(KeyCode::Delete));
    assert_eq!(s.prompt_suggestion, None);
    assert!(s.composer.is_empty());
}

#[test]
fn c5_esc_dismisses_the_ghost_and_does_nothing_else() {
    let mut s = with_suggestion();
    s.notification = None;
    let effects = reduce(&mut s, key(KeyCode::Esc));

    assert_eq!(s.prompt_suggestion, None);
    assert!(effects.is_empty(), "Esc cancels nothing here: {effects:?}");
    assert_eq!(s.status, RuntimeStatus::Idle);
    assert!(s.running, "Esc must not quit");
    assert!(s.composer.is_empty());
}

#[test]
fn c6_entering_history_browse_destroys_the_ghost_for_good() {
    let mut s = with_suggestion();
    s.composer.set_history(vec!["上一条消息".to_string()]);

    reduce(&mut s, key(KeyCode::Up));
    assert_eq!(s.composer.text(), "上一条消息", "history still works");
    assert_eq!(s.prompt_suggestion, None);

    // Stepping back past the newest entry restores an empty draft — and must
    // not bring the dismissed ghost back.
    reduce(&mut s, key(KeyCode::Down));
    assert!(s.composer.is_empty());
    assert_eq!(s.prompt_suggestion, None);
    assert!(!suggestion::is_visible(&s));
}

#[test]
fn c7_the_external_editor_is_seeded_with_the_empty_buffer() {
    let mut s = with_suggestion();
    let effects = reduce(
        &mut s,
        Action::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL)),
    );
    assert!(effects.is_empty());
    let effects = reduce(
        &mut s,
        Action::Key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL)),
    );

    match effects.as_slice() {
        [Effect::OpenExternalEditor { text }] => {
            assert_eq!(text, "", "a ghost is not a draft and never seeds $EDITOR");
        }
        other => panic!("expected one OpenExternalEditor, got {other:?}"),
    }
    assert_eq!(s.prompt_suggestion, None);
}

#[test]
fn c8_staging_a_clipboard_image_dismisses_the_ghost() {
    let mut s = with_suggestion();

    // An empty bracketed-paste payload is the clipboard-image gesture: it
    // asks the runtime for the image rather than inserting text.
    let effects = reduce(&mut s, Action::Paste(String::new()));
    assert!(
        effects.iter().any(|effect| matches!(
            effect,
            Effect::Send(ClientCommand::AddClipboardImage { .. })
        )),
        "expected the clipboard-image request: {effects:?}"
    );

    // The attachment arriving is the start of a new message.
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AttachmentAdded {
            attachment: image_attachment(),
        }),
    );
    assert_eq!(s.prompt_suggestion, None);
    assert!(!suggestion::is_visible(&s));

    let row = composer_row(&frame(&mut s, 100, 24));
    assert!(row.contains("图片 #1"), "the chip is painted: {row:?}");
    assert!(
        !row.contains("Tab: ") && !row.contains(NEXT_STEP),
        "the ghost must not sit behind an attachment chip: {row:?}"
    );
}

#[test]
fn c9_a_staged_attachment_hides_the_ghost_even_if_one_is_held() {
    let mut s = with_suggestion();
    // Bypass the event path: whatever put the chip there, a staged attachment
    // means the user is composing, so the ghost must not show.
    s.pending_attachments.push(image_attachment());

    assert!(!suggestion::is_visible(&s));
    assert!(!composer_row(&frame(&mut s, 100, 24)).contains("Tab: "));

    // Peeling the chip back off with Backspace leaves a clean empty composer
    // and does not resurrect the ghost.
    reduce(&mut s, key(KeyCode::Backspace));
    assert!(s.pending_attachments.is_empty());
    assert!(s.composer.is_empty());
    assert_eq!(s.prompt_suggestion, None);
}

// ---- D. what the terminal actually paints ----------------------------------

#[test]
fn d1_the_ghost_frame_shows_the_tab_hint_over_an_empty_buffer() {
    let mut s = with_suggestion();
    let row = composer_row(&frame(&mut s, 100, 24));

    assert!(row.contains("Tab: "), "missing Tab hint: {row:?}");
    assert!(row.contains(NEXT_STEP), "missing next step: {row:?}");
    assert!(s.composer.is_empty(), "buffer is still empty");
    assert!(
        row.starts_with("╭") || row.contains("› "),
        "the prompt survives: {row:?}"
    );
}

#[test]
fn d2_the_ghost_adds_no_composer_row_and_moves_no_caret() {
    let mut plain = opened();
    prose_turn(&mut plain, "做完了。");
    let (plain_screen, plain_caret) = draw(&mut plain, 100, 24);

    let mut ghosted = opened();
    goal_turn(&mut ghosted, "complete", Some(NEXT_STEP));
    let (ghost_screen, ghost_caret) = draw(&mut ghosted, 100, 24);
    assert!(composer_row(&ghost_screen).contains("Tab: "));

    assert_eq!(
        plain_screen
            .lines()
            .filter(|line| line.contains('│'))
            .count(),
        ghost_screen
            .lines()
            .filter(|line| line.contains('│'))
            .count(),
        "the ghost grew the composer"
    );
    assert_eq!(
        plain_caret, ghost_caret,
        "the ghost moved the terminal caret"
    );
}

#[test]
fn d3_double_width_and_emoji_suggestions_truncate_without_panicking() {
    for text in [
        "查看 demo-order-01 的里程碑进度并把结论写进交付报告里面去",
        "ship 👨‍👩‍👧 the release 🚀🚀🚀 and then verify everything twice over",
    ] {
        for width in [12u16, 20, 40, 100] {
            let mut s = opened();
            goal_turn(&mut s, "complete", Some(text));
            let screen = frame(&mut s, width, 24);
            for line in screen.lines() {
                assert!(
                    unicode_width::UnicodeWidthStr::width(line) <= width as usize,
                    "row overflowed {width} columns: {line:?}"
                );
            }
        }
    }
}

#[test]
fn d4_a_narrow_terminal_keeps_the_tab_label_and_shortens_the_body() {
    let mut s = with_suggestion();

    // 8 columns leaves the composer no content room at all — degrade, never
    // panic. This is the pre-existing tiny-terminal floor, not a ghost rule.
    let row = composer_row(&frame(&mut s, 8, 12));
    assert!(!row.contains(NEXT_STEP), "impossibly narrow row: {row:?}");

    // From the first column of real room upward, the label comes first and the
    // body is what gives way.
    for width in [10u16, 14, 18, 24] {
        let row = composer_row(&frame(&mut s, width, 12));
        assert!(
            row.contains("Ta"),
            "width {width} dropped the Tab label: {row:?}"
        );
        assert!(
            !row.contains(NEXT_STEP),
            "width {width} should have truncated the body: {row:?}"
        );
    }

    // Wide enough: label and body both fit in full.
    let row = composer_row(&frame(&mut s, 80, 24));
    assert!(row.contains("Tab: "), "{row:?}");
    assert!(row.contains(NEXT_STEP), "{row:?}");
}

#[test]
fn d5_real_input_replaces_the_ghost_in_the_painted_row() {
    let mut s = with_suggestion();
    typed(&mut s, "这个订单");
    let row = composer_row(&frame(&mut s, 100, 24));

    assert!(row.contains("这个订单"));
    assert!(!row.contains("Tab: "), "ghost still painted: {row:?}");
    assert!(!row.contains(NEXT_STEP), "ghost still painted: {row:?}");
}

#[test]
fn d6_the_ghost_hides_while_input_does_not_own_focus() {
    let mut s = with_suggestion();
    s.workbench_focus = WorkbenchFocus::Conversation;
    assert!(!suggestion::is_visible(&s));
    let row = composer_row(&frame(&mut s, 100, 24));
    assert!(!row.contains("Tab: "), "shown without Input focus: {row:?}");

    // Clicking back into the input shows it again: leaving the box only hides.
    s.workbench_focus = WorkbenchFocus::Input;
    assert!(suggestion::is_visible(&s));
    assert!(composer_row(&frame(&mut s, 100, 24)).contains("Tab: "));
}

#[test]
fn d7_the_ghost_hides_while_busy_disconnected_or_off_the_conversation() {
    let mut s = with_suggestion();

    s.status = RuntimeStatus::Busy;
    assert!(!suggestion::is_visible(&s), "hidden while a turn runs");
    s.status = RuntimeStatus::Idle;

    s.runtime_connected = false;
    assert!(!suggestion::is_visible(&s), "hidden while disconnected");
    s.runtime_connected = true;

    s.active_screen = Screen::Help;
    assert!(!suggestion::is_visible(&s), "hidden off the conversation");
    s.active_screen = Screen::Conversation;

    s.turn_nav = Some(0);
    assert!(!suggestion::is_visible(&s), "hidden while reviewing turns");
    s.turn_nav = None;

    assert!(
        suggestion::is_visible(&s),
        "and back once all of that clears"
    );
}

// ---- E. lifecycle -----------------------------------------------------------

#[test]
fn e1_a_new_turn_clears_the_previous_suggestion() {
    let mut s = with_suggestion();
    reduce(&mut s, Action::Runtime(busy()));

    assert_eq!(s.prompt_suggestion, None);
    assert!(!suggestion::is_visible(&s));
}

#[test]
fn e2_session_opened_clears_the_suggestion() {
    let mut s = with_suggestion();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: snapshot(),
        }),
    );
    assert_eq!(s.prompt_suggestion, None);

    let mut s = with_suggestion();
    let mut other = snapshot();
    other.id = SessionId::new("s2");
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened { session: other }),
    );
    assert_eq!(s.prompt_suggestion, None, "a session switch drops it too");
}

#[test]
fn e3_submitting_clears_the_suggestion() {
    let mut s = with_suggestion();
    typed(&mut s, "别的问题");
    reduce(&mut s, key(KeyCode::Enter));

    assert_eq!(s.prompt_suggestion, None);
    assert!(s.composer.is_empty());
}

#[test]
fn e4_a_new_turn_result_replaces_the_old_suggestion() {
    let mut s = with_suggestion();
    reduce(&mut s, Action::Runtime(busy()));
    goal_turn(&mut s, "complete", Some("发布 v0.3.0"));

    assert_eq!(s.prompt_suggestion.as_deref(), Some("发布 v0.3.0"));
}

#[test]
fn e5_notifications_and_ticks_do_not_clear_the_suggestion() {
    let mut s = with_suggestion();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::Notification {
            level: leveler_client_protocol::NotificationLevel::Info,
            message: "后台任务完成".into(),
        }),
    );
    assert_eq!(s.prompt_suggestion.as_deref(), Some(NEXT_STEP));

    s.tick += 1;
    let _ = frame(&mut s, 100, 24);
    assert_eq!(
        s.prompt_suggestion.as_deref(),
        Some(NEXT_STEP),
        "the renderer must be a pure reader"
    );
}

// ---- The acceptance flow, as two real frames -------------------------------

#[test]
fn acceptance_ghost_frame_then_accepted_frame_then_one_send() {
    let mut s = opened();
    goal_turn(&mut s, "complete", Some(NEXT_STEP));

    // Frame A — offered, not accepted.
    let frame_a = composer_row(&frame(&mut s, 80, 24));
    assert!(frame_a.contains("Tab: "), "frame A: {frame_a:?}");
    assert!(frame_a.contains(NEXT_STEP), "frame A: {frame_a:?}");
    assert_eq!(s.composer.canonical_text(), "", "frame A buffer is empty");

    // Enter here sends nothing.
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert!(effects.is_empty(), "frame A Enter sent: {effects:?}");
    assert!(
        s.transcript
            .items()
            .iter()
            .all(|item| !matches!(item, TranscriptItem::User(_)))
    );

    // Tab.
    let effects = reduce(&mut s, key(KeyCode::Tab));
    assert!(effects.is_empty(), "Tab sent: {effects:?}");

    // Frame B — accepted into the real buffer, still not sent.
    let frame_b = composer_row(&frame(&mut s, 80, 24));
    assert!(frame_b.contains(NEXT_STEP), "frame B: {frame_b:?}");
    assert!(
        !frame_b.contains("Tab: "),
        "frame B still ghosted: {frame_b:?}"
    );
    assert_eq!(s.composer.canonical_text(), NEXT_STEP);
    assert!(
        s.transcript
            .items()
            .iter()
            .all(|item| !matches!(item, TranscriptItem::User(_))),
        "Tab must not send"
    );

    // Enter now sends exactly one message.
    let effects = reduce(&mut s, key(KeyCode::Enter));
    let sends: Vec<_> = effects
        .iter()
        .filter(|effect| {
            matches!(
                effect,
                Effect::Send(ClientCommand::SubmitMessage { content, .. }) if content == NEXT_STEP
            )
        })
        .collect();
    assert_eq!(sends.len(), 1, "exactly one send: {effects:?}");

    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::UserMessageAdded {
            message: leveler_client_protocol::UiMessage {
                id: MessageId::new("u1"),
                role: leveler_client_protocol::UiRole::User,
                text: NEXT_STEP.to_string(),
                ordinal: None,
            },
        }),
    );
    let users: Vec<_> = s
        .transcript
        .items()
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::User(text) => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(users, vec![NEXT_STEP.to_string()], "one user message, once");
}

/// Dump the two acceptance frames as characters, for the record. Manual: run
/// with `--ignored --nocapture`.
#[test]
#[ignore = "frame evidence; run with --ignored --nocapture"]
fn evidence_two_frames() {
    let mut s = opened();
    goal_turn(&mut s, "complete", Some(NEXT_STEP));

    println!("== Frame A: offered, buffer empty ==");
    let (screen, caret) = draw(&mut s, 80, 20);
    for line in screen.lines() {
        println!("[{line}]");
    }
    println!(
        "buffer = {:?}  caret = {caret:?}",
        s.composer.canonical_text()
    );
    println!("Enter sends: {:?}", reduce(&mut s, key(KeyCode::Enter)));

    println!("== Frame B: after Tab ==");
    println!("Tab sends: {:?}", reduce(&mut s, key(KeyCode::Tab)));
    let (screen, caret) = draw(&mut s, 80, 20);
    for line in screen.lines() {
        println!("[{line}]");
    }
    println!(
        "buffer = {:?}  caret = {caret:?}",
        s.composer.canonical_text()
    );
    println!("Enter sends: {:?}", reduce(&mut s, key(KeyCode::Enter)));
}
