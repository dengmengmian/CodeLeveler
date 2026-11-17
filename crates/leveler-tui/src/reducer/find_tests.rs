//! `/find` and `/jump` through the real reducer: registration, dispatch,
//! busy behaviour, focus restore, the jump itself and stale-result rejection.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use leveler_client_protocol::{
    MessageId, PermissionProfile, RuntimeEvent, RuntimeStatus, SessionId, ToolCallId, UiMessage,
    UiRole, UiSessionSnapshot,
};

use super::reduce;
use super::submit::handle_slash;
use crate::action::{Action, Effect};
use crate::fold::DisplayMode;
use crate::screen::Screen;
use crate::state::{AppState, Boot, WorkbenchFocus};
use crate::transcript::{TurnEndStatus, UserShellStatus};
use leveler_core::UserShellId;

fn state() -> AppState {
    state_at(None)
}

fn state_at(root: Option<&std::path::Path>) -> AppState {
    let mut s = AppState::new(
        crate::theme::Theme::no_color(),
        Boot {
            session_id: SessionId::new("s1"),
            user: "u".into(),
            version: "0.1.0".into(),
            show_welcome: false,
            draft_path: root.map(|root| root.join("state").join("draft.txt")),
            history_path: None,
            context_window: 200_000,
            locale: crate::i18n::Locale::Zh,
            untrusted_config: Vec::new(),
            model_notice: None,
            thinking: None,
        },
    );
    s.size = (100, 32);
    s.conv.rect = Some((0, 2, 100, 24));
    s
}

fn temp_root(name: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "leveler-find-reducer-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("state").join("artifacts")).unwrap();
    root
}

/// Complete one artifact scan synchronously and fold it back through the
/// reducer, for tests that need the merged result set before pressing Enter.
fn finish_artifacts(s: &mut AppState, effects: Vec<Effect>) {
    for effect in effects {
        let Effect::RunFindArtifacts {
            generation,
            cancel,
            requests,
            query,
        } = effect
        else {
            continue;
        };
        for request in requests {
            let outcome = crate::find::artifacts::scan_file(
                &request.path,
                &query,
                cancel.atomic(),
                generation,
            );
            reduce(
                s,
                Action::FindArtifactReady {
                    generation,
                    request,
                    outcome,
                },
            );
        }
    }
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// Execute the transcript-search effect synchronously and fold its answer
/// back through the reducer, so a test sees the end state of a real query.
fn finish_search(s: &mut AppState, effects: Vec<Effect>) {
    for effect in effects {
        let Effect::RunFindTranscript {
            generation,
            corpus,
            query,
        } = effect
        else {
            continue;
        };
        let (hits, capped) = crate::find::search_corpus(&corpus, &query);
        reduce(
            s,
            Action::FindTranscriptReady {
                generation,
                hits,
                capped,
            },
        );
    }
}

fn push_turn(s: &mut AppState, text: &str, status: Option<TurnEndStatus>, secs: u64) {
    s.transcript.push_user(text.into());
    if let Some(status) = status {
        s.transcript.push_turn_end(status, 1, secs, None, None);
    }
}

fn snapshot(session: &str) -> UiSessionSnapshot {
    UiSessionSnapshot {
        history: None,
        id: SessionId::new(session),
        repository: Some("/repo".into()),
        task_status: None,
        task_terminal: None,
        goal: "g".into(),
        title: None,
        model: None,
        mode: PermissionProfile::Assisted,
        branch: None,
        status: "idle".into(),
        finalization_stage: None,
        messages: vec![UiMessage {
            id: MessageId::new("m1"),
            role: UiRole::User,
            text: "other session".into(),
            ordinal: None,
            kind: None,
            images: 0,
        }],
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
    }
}

#[test]
fn find_opens_while_busy_and_prefills_the_query() {
    let mut s = state();
    s.status = RuntimeStatus::Busy;
    let effects = handle_slash(&mut s, "find refund");
    assert_eq!(s.active_screen, Screen::Find);
    assert_eq!(s.find.query, "refund");
    assert_eq!(s.find.tab, crate::find::FindTab::Search);
    assert!(
        effects
            .iter()
            .all(|effect| !matches!(effect, Effect::Send(_) | Effect::Submit { .. })),
        "a local search must never reach the runtime"
    );
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::RunFindTranscript { .. })),
        "an open with a query must start the search"
    );
}

#[test]
fn the_search_alias_opens_the_same_page() {
    let mut s = state();
    handle_slash(&mut s, "search needle");
    assert_eq!(s.active_screen, Screen::Find);
    assert_eq!(s.find.query, "needle");
    assert_eq!(s.find.generation, s.find.cancel.load());
}

#[test]
fn esc_returns_to_the_conversation_and_restores_the_focus() {
    let mut s = state();
    s.workbench_focus = WorkbenchFocus::Goal;
    handle_slash(&mut s, "find");
    assert_eq!(s.active_screen, Screen::Find);
    reduce(&mut s, Action::Key(key(KeyCode::Esc)));
    assert_eq!(s.active_screen, Screen::Conversation);
    assert_eq!(s.workbench_focus, WorkbenchFocus::Goal);
    assert!(s.find.query.is_empty());
    assert!(s.find.hits.is_empty());
}

#[test]
fn enter_on_a_folded_tool_result_expands_it_and_locates_the_match() {
    let mut s = state();
    s.transcript.push_user("run the tests".into());
    let call = ToolCallId::new("c1");
    s.transcript.push_tool_started(
        call.clone(),
        "run_command".into(),
        "{}".into(),
        false,
        0,
        Some(1),
        None,
    );
    s.transcript
        .complete_tool(&call, false, "test_session_resume FAILED".into(), 9, None);
    let item = s.transcript.items().len() - 1;
    s.transcript.set_item_display(item, DisplayMode::Collapsed);
    assert_eq!(
        s.transcript.item_display(item),
        Some(DisplayMode::Collapsed)
    );

    let effects = handle_slash(&mut s, "find test_session_resume FAILED");
    finish_search(&mut s, effects);
    assert_eq!(s.find.hits.len(), 1, "{:?}", s.find.hits);
    assert_eq!(s.find.hits[0].item, item);

    reduce(&mut s, Action::Key(key(KeyCode::Enter)));
    assert_eq!(s.active_screen, Screen::Conversation);
    assert_eq!(
        s.transcript.item_display(item),
        Some(DisplayMode::Expanded),
        "the located entry must reveal the matched body"
    );
    assert!(
        !s.conv.auto_scroll,
        "a jump pins the viewport off the live edge"
    );
    let highlight = s.find_highlight.as_ref().expect("a located highlight");
    assert_eq!(highlight.item, item);
    assert_eq!(highlight.query, "test_session_resume FAILED");
}

#[test]
fn jump_opens_turns_and_a_number_jumps_to_that_turn() {
    let mut s = state();
    // Enough history that the second turn is below a 24-row viewport: the jump
    // has to MOVE the viewport, not merely be visible on a page that fits.
    s.transcript.push_note(
        (0..40)
            .map(|i| format!("padding {i}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    push_turn(&mut s, "first request", Some(TurnEndStatus::Completed), 12);
    push_turn(&mut s, "second request", Some(TurnEndStatus::Failed), 3);

    handle_slash(&mut s, "jump");
    assert_eq!(s.active_screen, Screen::Find);
    assert_eq!(s.find.tab, crate::find::FindTab::Turns);

    handle_slash(&mut s, "jump 2");
    assert_eq!(s.active_screen, Screen::Conversation);
    assert!(!s.conv.auto_scroll);
    let width = crate::conversation::geometry::content_width(&s);
    let (lines, ..) = s.conversation_build(width);
    let row = lines
        .iter()
        .position(|line| {
            let plain: String = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect();
            plain.contains("second request")
        })
        .expect("the second prompt is in the projection");
    let height = crate::conversation::geometry::viewport_height(&s);
    assert!(
        s.conv.scroll <= row && row < s.conv.scroll + height,
        "scroll {}..{} must reveal row {row}",
        s.conv.scroll,
        s.conv.scroll + height
    );
    assert!(s.conv.scroll > 0, "the jump must scroll a long transcript");
    let notification = s.notification.as_ref().expect("a jump receipt");
    assert!(
        notification.message.contains('2'),
        "{}",
        notification.message
    );
}

#[test]
fn an_out_of_range_jump_is_refused_with_the_valid_range() {
    let mut s = state();
    push_turn(&mut s, "only turn", Some(TurnEndStatus::Completed), 1);
    let before = s.conv.scroll;
    handle_slash(&mut s, "jump 9");
    assert_eq!(s.active_screen, Screen::Conversation);
    assert_eq!(s.conv.scroll, before);
    let note = s.notification.as_ref().expect("range feedback");
    assert!(note.message.contains('1'), "{}", note.message);
}

#[test]
fn typing_in_turns_switches_to_search() {
    let mut s = state();
    push_turn(&mut s, "a turn", Some(TurnEndStatus::Completed), 1);
    handle_slash(&mut s, "jump");
    assert_eq!(s.find.tab, crate::find::FindTab::Turns);
    reduce(&mut s, Action::Key(key(KeyCode::Char('r'))));
    assert_eq!(s.find.tab, crate::find::FindTab::Search);
    assert_eq!(s.find.query, "r");
}

#[test]
fn a_stale_result_never_replaces_the_current_search() {
    let mut s = state();
    s.transcript.push_user("refund".into());
    let effects = handle_slash(&mut s, "find refund");
    let generation = s.find.generation;
    finish_search(&mut s, effects);
    assert_eq!(s.find.hits.len(), 1);

    let stale = vec![crate::find::FindHit {
        item: 0,
        call: None,
        call_id: None,
        shell_id: None,
        source: crate::find::FindSource::User,
        turn: 1,
        occurrence: 0,
        snippet: "stale".into(),
        match_start: 0,
        match_end: 5,
        artifact: None,
        id: None,
        text_fingerprint: 0,
    }];
    reduce(
        &mut s,
        Action::FindTranscriptReady {
            generation: generation.wrapping_sub(1),
            hits: stale,
            capped: false,
        },
    );
    assert_eq!(s.find.hits.len(), 1);
    assert_eq!(s.find.hits[0].snippet, "refund");
}

#[test]
fn switching_sessions_clears_the_search_and_its_results() {
    let mut s = state();
    s.transcript.push_user("refund".into());
    let effects = handle_slash(&mut s, "find refund");
    finish_search(&mut s, effects);
    assert!(!s.find.hits.is_empty());

    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: snapshot("s2"),
        }),
    );
    assert_eq!(s.session_id, SessionId::new("s2"));
    assert!(s.find.hits.is_empty());
    assert!(s.find.query.is_empty());
    assert!(s.find.corpus.is_none());
    assert!(s.find_highlight.is_none());
    assert_eq!(s.active_screen, Screen::Conversation);
    assert_eq!(s.history_omitted_turns, 0);
}

#[test]
fn an_artifact_result_opens_the_full_output_page_at_the_matched_line() {
    let root = temp_root("artifact-jump");
    let artifact = root
        .join("state")
        .join("artifacts")
        .join("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef.txt");
    std::fs::write(&artifact, "head\nUNIQUE_ARTIFACT_TOKEN\ntail\n").unwrap();
    let mut s = state_at(Some(&root));
    s.transcript.push_user("run long".into());
    let call = ToolCallId::new("c1");
    s.transcript.push_tool_started(
        call.clone(),
        "run_command".into(),
        "{}".into(),
        false,
        0,
        Some(1),
        None,
    );
    let reference =
        leveler_model::tool_output::saved_reference(artifact.to_str().unwrap(), "hash", 34);
    s.transcript
        .complete_tool(&call, true, format!("{reference}\nhead tail"), 5, None);

    let effects = handle_slash(&mut s, "find UNIQUE_ARTIFACT_TOKEN");
    finish_search(&mut s, effects.clone());
    finish_artifacts(&mut s, effects);
    assert_eq!(s.find.hits.len(), 1, "{:?}", s.find.hits);
    assert_eq!(
        s.find.hits[0].artifact.as_ref().map(|hit| hit.line),
        Some(2)
    );

    let effects = reduce(&mut s, Action::Key(key(KeyCode::Enter)));
    assert_eq!(s.active_screen, Screen::Output);
    let view = s.output_view.as_ref().expect("the output page");
    assert_eq!(view.target_line, Some(2));
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::LoadOutputIndex { .. })),
        "the page must build its index"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_swept_artifact_is_reported_instead_of_opening_an_empty_page() {
    let root = temp_root("artifact-swept");
    let artifact = root
        .join("state")
        .join("artifacts")
        .join("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef.txt");
    std::fs::write(&artifact, "head\nUNIQUE_ARTIFACT_TOKEN\ntail\n").unwrap();
    let mut s = state_at(Some(&root));
    s.transcript.push_user("run long".into());
    let call = ToolCallId::new("c1");
    s.transcript.push_tool_started(
        call.clone(),
        "run_command".into(),
        "{}".into(),
        false,
        0,
        Some(1),
        None,
    );
    let reference =
        leveler_model::tool_output::saved_reference(artifact.to_str().unwrap(), "hash", 34);
    s.transcript
        .complete_tool(&call, true, format!("{reference}\nhead tail"), 5, None);
    let effects = handle_slash(&mut s, "find UNIQUE_ARTIFACT_TOKEN");
    finish_search(&mut s, effects.clone());
    finish_artifacts(&mut s, effects);
    assert_eq!(s.find.hits.len(), 1);

    std::fs::remove_file(&artifact).unwrap();
    let effects = reduce(&mut s, Action::Key(key(KeyCode::Enter)));
    assert_eq!(s.active_screen, Screen::Find, "the page stays open");
    assert!(s.output_view.is_none());
    assert!(s.find.notice.is_some(), "the missing artifact is stated");
    assert!(s.find.artifacts_unavailable > 0);
    assert!(effects.is_empty());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_scope_line_states_omitted_turns_and_incomplete_outputs() {
    let mut s = state();
    s.history_omitted_turns = 7;
    s.find.partial_outputs = 2;
    s.find.corpus = Some(std::sync::Arc::new(crate::find::build_corpus(&s)));
    let line = crate::find::scope_line(&s, crate::i18n::Locale::Zh.text());
    assert!(line.contains('7'), "{line}");
    assert!(line.contains('2'), "{line}");
    assert!(line.contains("范围"), "{line}");
}

#[test]
fn a_user_shell_capture_result_opens_its_complete_output() {
    let root = temp_root("shell-capture");
    let directory = root
        .join("state")
        .join("shell-output")
        .join("s1")
        .join("cap");
    std::fs::create_dir_all(&directory).unwrap();
    for stream in ["stdout", "stderr", "combined"] {
        std::fs::write(
            directory.join(format!("{stream}.log")),
            "head\nSHELL_CAPTURE_TOKEN\ntail\n",
        )
        .unwrap();
        std::fs::write(
            directory.join(format!("{stream}.meta.json")),
            "{\"complete\":true}\n",
        )
        .unwrap();
    }
    let mut s = state_at(Some(&root));
    let id = UserShellId::new("sh1");
    s.transcript
        .push_user_shell_started(id.clone(), "cargo test".into(), "/repo".into(), 0);
    s.transcript
        .append_user_shell_output(&id, "head tail", false);
    s.transcript
        .set_user_shell_full_output(&id, Some(directory.display().to_string()));
    s.transcript
        .complete_user_shell(&id, Some(0), 10, UserShellStatus::Success);

    let effects = handle_slash(&mut s, "find SHELL_CAPTURE_TOKEN");
    finish_search(&mut s, effects.clone());
    finish_artifacts(&mut s, effects);
    assert_eq!(s.find.hits.len(), 1, "{:?}", s.find.hits);
    let artifact = s.find.hits[0].artifact.as_ref().expect("a capture hit");
    assert_eq!(artifact.kind, crate::find::ArtifactKind::ShellCapture);
    assert_eq!(artifact.line, 2);

    reduce(&mut s, Action::Key(key(KeyCode::Enter)));
    assert_eq!(s.active_screen, Screen::Output);
    assert_eq!(
        s.output_view.as_ref().expect("output page").target_line,
        Some(2)
    );
    let _ = std::fs::remove_dir_all(&root);
}
