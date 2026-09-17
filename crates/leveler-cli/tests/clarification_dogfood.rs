//! Dogfood: a REAL runtime answers a multi-question clarification through the
//! REAL TUI reducer and renderer.
//!
//! The model is scripted (`MockServer`), everything between it and the screen
//! is production code: the agent's `request_user_input` call, the execution
//! clarifier, the app's channel bridge, the client protocol, the TUI reducer
//! and its renderer. The test drives the keys the design specifies and asserts
//! the composed answers reach the model.
//!
//! ```text
//! cargo test -p leveler-cli --test clarification_dogfood -- --nocapture
//! ```

use std::sync::Arc;
use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{
    ClientCommand, InteractiveRuntimeClient, RuntimeEvent, UiSessionSnapshot,
};
use leveler_execution::PermissionProfile;
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_test_support::{MockResponse, MockServer};
use leveler_tui::action::{Action, Effect};
use leveler_tui::overlay::Overlay;
use leveler_tui::reducer::reduce;
use leveler_tui::state::{AppState, Boot};
use leveler_tui::theme::Theme;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
// ratatui re-exports the same crossterm the TUI's `Action::Key` speaks.
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn isolate_global_config() {
    use std::sync::OnceLock;
    static EMPTY_HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    let dir = EMPTY_HOME.get_or_init(|| tempfile::tempdir().unwrap());
    // SAFETY: test-only process isolation; single-threaded setup before async work.
    unsafe {
        std::env::set_var("LEVELER_HOME", dir.path());
    }
}

fn sse(frames: Vec<String>) -> MockResponse {
    let mut body = String::new();
    for frame in frames {
        body.push_str("data: ");
        body.push_str(&frame);
        body.push_str("\n\n");
    }
    body.push_str("data: [DONE]\n\n");
    MockResponse::Sse { body }
}

/// The model asks three questions in one call: a single choice, a multi
/// choice with a minimum, and a free-text question.
fn clarify_call() -> MockResponse {
    let arguments = serde_json::json!({
        "question": "需要你的选择",
        "questions": [
            {
                "header": "数据策略",
                "question": "数据策略怎么定？",
                "kind": "single",
                "options": ["保留 demo fallback", "删除全部 mock", "保持现状"],
                "allow_other": true
            },
            {
                "header": "验证范围",
                "question": "需要跑哪些验证？",
                "kind": "multi",
                "options": ["单元测试", "TUI 测试", "workspace test"],
                "min_choices": 1
            },
            {
                "header": "未登录态",
                "question": "未登录时怎么展示？",
                "kind": "text"
            }
        ]
    });
    sse(vec![
        serde_json::json!({"choices": [{"delta": {"tool_calls": [{
            "index": 0, "id": "call-clarify",
            "function": {"name": "request_user_input", "arguments": arguments.to_string()}
        }]}}]})
        .to_string(),
        serde_json::json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}).to_string(),
    ])
}

fn text(content: &str) -> MockResponse {
    sse(vec![
        serde_json::json!({"choices": [{"delta": {"content": content}, "finish_reason": "stop"}]})
            .to_string(),
    ])
}

fn tui_state(session: leveler_core::SessionId) -> AppState {
    AppState::new(
        Theme::no_color(),
        Boot {
            session_id: session,
            user: "dogfood".into(),
            version: "0.0.0".into(),
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

/// A one-line tag for a runtime event, so a dogfood run says where it stopped.
fn short(event: &RuntimeEvent) -> String {
    let text = format!("{event:?}");
    text.split(['{', ' ', '('])
        .next()
        .unwrap_or("?")
        .to_string()
}

fn key(code: KeyCode) -> Action {
    Action::Key(KeyEvent::new(code, KeyModifiers::empty()))
}

fn frame(state: &mut AppState, label: &str) -> String {
    const W: u16 = 96;
    const H: u16 = 30;
    let mut term = Terminal::new(TestBackend::new(W, H)).unwrap();
    term.draw(|f| leveler_tui::render::render(f, state))
        .unwrap();
    let buf = term.backend().buffer();
    let mut out = format!("\n===== {label} =====\n");
    for y in 0..H {
        let mut line = String::new();
        let mut x = 0u16;
        while x < W {
            let sym = buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" ");
            line.push_str(sym);
            x += unicode_width::UnicodeWidthStr::width(sym).max(1) as u16;
        }
        let line = line.trim_end();
        if !line.is_empty() {
            out.push_str(&format!("|{line}\n"));
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_multi_question_clarification_runs_end_to_end_from_the_agent_to_the_screen() {
    isolate_global_config();
    let server = MockServer::start(vec![
        // 1: the model asks the three questions.
        clarify_call(),
        // 2: the model answers after being handed the user's decisions.
        text("已按你的选择处理"),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("configs/providers")).unwrap();
    std::fs::create_dir_all(tmp.path().join("configs/models")).unwrap();
    std::fs::write(
        tmp.path().join("configs/providers/mock.yaml"),
        format!(
            "id: mock\nprotocol: openai_chat\nbase_url: {}\n",
            server.base_url()
        ),
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("configs/models/m.yaml"),
        r#"
id: m
provider: mock
model_id: mock-model
protocol: openai_chat
capabilities:
  streaming: true
  tool_calling: true
  parallel_tool_calls: false
  structured_output: true
  reasoning: false
  vision: false
limits:
  context_window: 16384
  reliable_context: 8192
  max_output_tokens: 2048
  max_tool_schema_bytes: 16384
  max_parallel_tool_calls: 1
compatibility:
  synthesize_tool_call_ids: true
  drop_unsupported_fields: true
"#,
    )
    .unwrap();
    let layout = Layout::from_parts(
        tmp.path().to_path_buf(),
        tmp.path().join("configs"),
        tmp.path().join("state"),
    );
    let app = Arc::new(Application::assemble(layout).unwrap());
    let model = ModelRef::new("mock", "m");
    let session = app.create_session(&model, "dogfood").await.unwrap();
    let client = Arc::new(InProcessRuntimeClient::new(
        app,
        model,
        PermissionProfile::Assisted,
        false,
    ));

    // The real runtime snapshot, the same one a connecting client receives.
    let snapshot: UiSessionSnapshot = client.snapshot(&session).await.unwrap();
    let mut state = tui_state(session.clone());
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::SessionOpened { session: snapshot }),
    );

    let mut rx = client.subscribe();
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: "做一次数据策略调整".into(),
            attachments: vec![],
        })
        .await
        .unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let mut drove = false;
    let mut sent_answers = false;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let event = match tokio::time::timeout(left, rx.recv()).await {
            Ok(Ok(event)) => event,
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            other => panic!("the runtime went quiet: {other:?}"),
        };
        // Every terminal a turn can reach; `TurnAnswered` is the one a
        // question-answering turn ends on.
        let completed = matches!(
            event,
            RuntimeEvent::TurnCompleted
                | RuntimeEvent::TurnAnswered
                | RuntimeEvent::TurnCompletedWithWarnings { .. }
                | RuntimeEvent::TurnCompletedUnverified { .. }
                | RuntimeEvent::TurnCompletedChecksFailed { .. }
                | RuntimeEvent::TurnFailed { .. }
                | RuntimeEvent::TurnCancelled
        );
        println!("[event] {}", short(&event));
        reduce(&mut state, Action::Runtime(event));

        let clarifying = matches!(&state.overlay, Some(Overlay::Clarification(_)));
        if clarifying && !drove {
            drove = true;
            // The wire carried all three questions, with their kinds.
            let tabs = match &state.overlay {
                Some(Overlay::Clarification(ov)) => ov.len(),
                _ => unreachable!(),
            };
            assert_eq!(tabs, 3, "three questions arrive as three tabs");
            println!("{}", frame(&mut state, "question 1/3"));
            assert!(
                frame(&mut state, "").contains("● 数据策略"),
                "the first tab is current"
            );

            // Answer the single choice by moving to the second option.
            reduce(&mut state, key(KeyCode::Down));
            println!("{}", frame(&mut state, "question 1: moved"));
            reduce(&mut state, key(KeyCode::Enter)); // → advances to the multi
            println!("{}", frame(&mut state, "question 2/3"));

            // The multi question has a minimum, so a bare Enter must refuse.
            let refused = reduce(&mut state, key(KeyCode::Enter));
            assert!(refused.is_empty(), "an unmet minimum must not submit");
            let refused_frame = frame(&mut state, "question 2: minimum refused");
            println!("{refused_frame}");
            assert!(
                refused_frame.contains("至少选择 1 项才能确认"),
                "{refused_frame}"
            );

            // Pick two of the three, then confirm.
            reduce(&mut state, key(KeyCode::Char(' ')));
            reduce(&mut state, key(KeyCode::Down));
            reduce(&mut state, key(KeyCode::Char(' ')));
            println!("{}", frame(&mut state, "question 2: two picked"));
            reduce(&mut state, key(KeyCode::Enter)); // → advances to the text question
            println!("{}", frame(&mut state, "question 3/3"));

            for c in "展示登录引导".chars() {
                reduce(&mut state, key(KeyCode::Char(c)));
            }
            println!("{}", frame(&mut state, "question 3: typed"));

            let effects = reduce(&mut state, key(KeyCode::Enter));
            let (request_id, answer) = effects
                .iter()
                .find_map(|e| match e {
                    Effect::SendInteraction {
                        command: ClientCommand::AnswerClarification { request_id, answer },
                        ..
                    } => Some((request_id.clone(), answer.clone())),
                    _ => None,
                })
                .expect("the last question submits the interaction");
            // What the user decided, in the order the tabs were shown.
            assert_eq!(
                answer,
                "数据策略: 删除全部 mock\n验证范围: 单元测试, TUI 测试\n未登录态: 展示登录引导"
            );
            // Exactly what the TUI's own effect loop does with it: the
            // request-id command rides a session envelope.
            client
                .issue(
                    session.clone(),
                    ClientCommand::AnswerClarification { request_id, answer },
                )
                .await
                .unwrap();
            sent_answers = true;
        }
        if completed {
            break;
        }
        assert!(
            server.request_count() < 2 || sent_answers,
            "the model was asked again before the answer was sent"
        );
        assert!(
            tokio::time::Instant::now() < deadline,
            "the turn did not settle"
        );
    }
    assert!(drove, "the clarification never reached the TUI");
    assert!(sent_answers);

    // The model was actually handed the decisions: the second request carries
    // the tool result, which is the composed answer.
    let bodies = server.request_bodies().await;
    assert!(bodies.len() >= 2, "the model ran once: {bodies:?}");
    let second = &bodies[1];
    for expected in ["删除全部 mock", "单元测试", "TUI 测试", "展示登录引导"] {
        assert!(
            second.contains(expected),
            "the model never saw {expected:?}: {second}"
        );
    }
    assert!(
        !second.contains("request_user_input") || second.contains("tool"),
        "the answer must come back as the tool result: {second}"
    );
}
