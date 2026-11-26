//! Full tool output: the runtime serves the recorded result of a real call.
//!
//! Every assertion is a fact from the durable transcript or from bytes the
//! command actually produced. The test deliberately queries a call id that
//! belongs to another session to prove the answer is bound to the session's
//! own record, and queries an unknown id to prove absence is typed, not empty.

#![cfg(unix)]

use std::sync::Arc;
use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{
    ClientCommand, CommandId, InteractiveRuntimeClient, RuntimeEvent, UiToolOutputState,
    UiToolOutputUnavailable,
};
use leveler_execution::PermissionProfile;
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_test_support::{MockResponse, MockServer};
use tokio::sync::broadcast;

const BIG_CALL_ID: &str = "big-1";
const SMALL_CALL_ID: &str = "small-1";

fn isolate_global_config() {
    use std::sync::OnceLock;
    static EMPTY_HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    let dir = EMPTY_HOME.get_or_init(|| tempfile::tempdir().unwrap());
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

fn tool_call_frame(id: &str, name: &str, arguments: serde_json::Value) -> String {
    serde_json::json!({
        "choices": [{
            "index": 0,
            "delta": {
                "role": "assistant",
                "tool_calls": [{
                    "index": 0,
                    "id": id,
                    "type": "function",
                    "function": { "name": name, "arguments": arguments.to_string() }
                }]
            },
            "finish_reason": null
        }]
    })
    .to_string()
}

fn answer_frame(text: &str) -> String {
    serde_json::json!({
        "choices": [{
            "index": 0,
            "delta": { "role": "assistant", "content": text },
            "finish_reason": null
        }]
    })
    .to_string()
}

fn finish_frame(reason: &str) -> String {
    serde_json::json!({
        "choices": [{ "index": 0, "delta": {}, "finish_reason": reason }]
    })
    .to_string()
}

fn write_config(dir: &std::path::Path, base_url: &str) {
    let config = format!(
        "default_model = \"mock/m\"\n\
         [providers.mock]\n\
         protocol = \"openai_chat\"\n\
         base_url = \"{base_url}\"\n\
         api_key = \"test-only\"\n\
         [models.m]\n\
         provider = \"mock\"\n\
         model_id = \"mock-model\"\n\
         context_window = 131072\n\
         parallel_tool_calls = false\n\
         vision = false\n"
    );
    std::fs::write(dir.join("config.toml"), config).unwrap();
}

struct Fixture {
    app: Arc<Application>,
    session: leveler_core::SessionId,
    _server: MockServer,
    _tmp: tempfile::TempDir,
}

async fn fixture(responses: Vec<MockResponse>) -> Fixture {
    isolate_global_config();
    let server = MockServer::start(responses).await;
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("hello.txt"), "alpha\nbeta\ngamma\n").unwrap();
    write_config(tmp.path(), &server.base_url());
    let layout = Layout::from_parts(
        tmp.path().to_path_buf(),
        tmp.path().join("configs"),
        tmp.path().join("state"),
    );
    let app = Arc::new(Application::assemble(layout).unwrap());
    let session = app
        .create_session(&ModelRef::new("mock", "m"), "read the files")
        .await
        .unwrap();
    Fixture {
        app,
        session,
        _server: server,
        _tmp: tmp,
    }
}

async fn wait_for_terminal(rx: &mut broadcast::Receiver<RuntimeEvent>) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Ok(RuntimeEvent::TurnCompleted))
            | Ok(Ok(RuntimeEvent::TurnAnswered))
            | Ok(Ok(RuntimeEvent::TurnCompletedWithWarnings { .. }))
            | Ok(Ok(RuntimeEvent::TurnCompletedPartiallyUnverified { .. }))
            | Ok(Ok(RuntimeEvent::TurnIncomplete { .. }))
            | Ok(Ok(RuntimeEvent::TurnBlocked { .. }))
            | Ok(Ok(RuntimeEvent::TurnFailed { .. }))
            | Ok(Ok(RuntimeEvent::TurnCancelled))
            | Ok(Ok(RuntimeEvent::TaskCancelled)) => return,
            Ok(Ok(_)) | Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(broadcast::error::RecvError::Closed)) => panic!("event stream closed"),
            Err(_) => panic!("the turn never ended"),
        }
    }
}

async fn query(
    client: &Arc<dyn InteractiveRuntimeClient>,
    rx: &mut broadcast::Receiver<RuntimeEvent>,
    session: &leveler_core::SessionId,
    call_id: &str,
    index: Option<u32>,
    offset: u64,
    max_bytes: u32,
) -> leveler_client_protocol::UiToolOutput {
    let query_id = CommandId::generate();
    client
        .send(ClientCommand::QueryToolOutput {
            session_id: session.clone(),
            query_id: query_id.clone(),
            call_id: call_id.to_string(),
            index,
            offset,
            max_bytes,
        })
        .await
        .expect("the runtime accepts a bounded tool-output query");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Ok(RuntimeEvent::ToolOutputLoaded {
                query_id: answered,
                output,
                ..
            })) if answered == query_id => return output,
            Ok(Ok(_)) | Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(broadcast::error::RecvError::Closed)) => panic!("event stream closed"),
            Err(_) => panic!("no tool output answer arrived"),
        }
    }
}

/// One turn issues a large command (stream spill) and a small read (fits in
/// the model result). Both recorded results are then read back through the
/// public query, and the large stream is paged to its exact end.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_runtime_serves_the_full_recorded_result_and_its_artifacts() {
    let f = fixture(vec![
        sse(vec![
            tool_call_frame(
                BIG_CALL_ID,
                "run_command",
                serde_json::json!({
                    "program": "sh",
                    "args": ["-c", "seq 1 20000"],
                    "reason": "produce an output larger than one stream page"
                }),
            ),
            finish_frame("tool_calls"),
        ]),
        sse(vec![
            tool_call_frame(
                SMALL_CALL_ID,
                "read_file",
                serde_json::json!({"path": "hello.txt"}),
            ),
            finish_frame("tool_calls"),
        ]),
        sse(vec![answer_frame("done"), finish_frame("stop")]),
    ])
    .await;
    let runtime: Arc<dyn InteractiveRuntimeClient> = Arc::new(
        InProcessRuntimeClient::new_with_options(
            f.app.clone(),
            ModelRef::new("mock", "m"),
            PermissionProfile::Assisted,
            false,
            true,
        ),
    );
    let mut rx = runtime.subscribe();
    runtime
        .send(ClientCommand::SubmitMessage {
            session_id: f.session.clone(),
            content: "run the command and read the file".into(),
            attachments: Vec::new(),
        })
        .await
        .unwrap();
    wait_for_terminal(&mut rx).await;

    // The model-visible result: it really is the command's bounded preview,
    // and its stream locator names the complete stdout.
    let result = query(&runtime, &mut rx, &f.session, BIG_CALL_ID, None, 0, 65_536).await;
    assert_eq!(result.state, UiToolOutputState::Available, "{result:?}");
    assert!(result.model_result_reduced, "the result must say it is partial");
    assert!(!result.full_output_lost, "the stream was saved, not lost");
    assert!(result.text.contains("stdout full output:"), "{}", result.text);
    assert_eq!(result.references.len(), 1, "{:?}", result.references);
    let reference = &result.references[0];
    assert_eq!(reference.label, "stdout");
    assert!(reference.available);
    assert!(
        reference.size_bytes > 100_000,
        "seq 1 20000 is ~108 KB: {reference:?}"
    );

    // The complete artifact pages losslessly to the exact last line.
    let mut paged = String::new();
    let mut offset = 0u64;
    loop {
        let page = query(&runtime, &mut rx, &f.session, BIG_CALL_ID, Some(0), offset, 16_384).await;
        assert_eq!(page.state, UiToolOutputState::Available, "{page:?}");
        assert_eq!(page.part, leveler_client_protocol::UiToolOutputPart::Artifact { index: 0 });
        paged.push_str(&page.text);
        if !page.has_more {
            assert_eq!(page.next_offset, page.total_bytes);
            break;
        }
        assert!(page.next_offset > offset, "a page must make progress");
        offset = page.next_offset;
    }
    assert_eq!(paged.lines().count(), 20_000);
    assert!(paged.starts_with("1\n"));
    assert!(paged.ends_with("20000\n"), "{:?}", &paged[paged.len().saturating_sub(20)..]);

    // A small result is served exactly as the model received it.
    let small = query(&runtime, &mut rx, &f.session, SMALL_CALL_ID, None, 0, 65_536).await;
    assert_eq!(small.state, UiToolOutputState::Available);
    assert!(!small.model_result_reduced);
    assert_eq!(small.text, "     1\talpha\n     2\tbeta\n     3\tgamma\n");

    // Absence is typed: a fabricated id has no stored result.
    let missing = query(&runtime, &mut rx, &f.session, "never-called", None, 0, 1024).await;
    assert_eq!(missing.state, UiToolOutputState::Unavailable);
    assert_eq!(
        missing.unavailable,
        Some(UiToolOutputUnavailable::NotSaved)
    );

    // The record is bound to its own session: another session cannot read it.
    let other = f
        .app
        .create_session(&ModelRef::new("mock", "m"), "another session")
        .await
        .unwrap();
    let foreign = query(&runtime, &mut rx, &other, BIG_CALL_ID, None, 0, 1024).await;
    assert_eq!(
        foreign.unavailable,
        Some(UiToolOutputUnavailable::NotSaved),
        "a call id from another session is not a capability"
    );

    // A size window is honored and bounded.
    let bounded = query(&runtime, &mut rx, &f.session, SMALL_CALL_ID, None, 0, 5).await;
    assert_eq!(bounded.text, "     ", "char boundaries stay intact");
    assert!(bounded.has_more);
}

