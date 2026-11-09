//! REAL DeepSeek dogfood for Phase 2C-R4 mid-turn steering.
//!
//! The runtime, client protocol, journal, executor and provider are the
//! production ones built from this worktree. The model is the configured real
//! DeepSeek gateway. Nothing about the model's behavior is scripted: the
//! harness only submits the user's text at a moment when a real tool is
//! running, and then reads durable facts.
//!
//! * **D1** — one instruction appended through `SteerCurrentTurn` while a real
//!   foreground tool runs. The same turn consumes it at its next model round,
//!   the model's own final answer reflects the new requirement, and the
//!   journal row is consumed exactly once under the turn's identity.
//! * **D2** — three appends in one real turn, then a real compaction, then a
//!   follow-up turn that must still name all three requirements.
//! * **D3** — an appended instruction is accepted durably, the runtime is
//!   really SIGKILLed before it is consumed, and a new runtime on the same
//!   isolated state delivers it exactly once.
//!
//! Ignored by default because it spends real tokens and real processes:
//!
//! ```text
//! set -a; . ~/.leveler/deepseek.env; set +a
//! R4_EVIDENCE_DIR=<abs>/evidence/deepseek \
//!   cargo test -p leveler-cli --test deepseek_steer_dogfood \
//!   -- --ignored --nocapture --test-threads=1
//! ```
#![cfg(unix)]

use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use leveler_client_protocol::{ClientCommand, InteractiveRuntimeClient, MessageId, RuntimeEvent};
use leveler_local_transport::{CollaborationMode, CreateSessionRequest, LocalSocketRuntimeClient};
use leveler_storage::{
    Database, MessageRepository, ModelCallKind, ModelRequestRepository, SessionInputRepository,
    SessionInputState, TurnRepository,
};
use leveler_test_support::ManagedChild;

const PROVIDER_BASE_URL: &str = "https://taotoken.net/api/v1";
const MODEL_REF: &str = "deepseek/deepseek-v4.1-flash-1017";

// One marker per requirement so "the model actually saw it" is a fact in its
// own final answer, not a guess.
const STEER_MARKER: &str = "R4-STR33R-SEEN";
const D2_MARKERS: [&str; 3] = ["R4-A-KEEP", "R4-B-PREFIX", "R4-C-LIST"];
const D3_MARKER: &str = "R4-CRASH-STEER";

struct Environment {
    _temp: tempfile::TempDir,
    home: PathBuf,
    config: PathBuf,
    repo: PathBuf,
}

impl Environment {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let home = root.join("home");
        let config = root.join("config");
        let repo = root.join("repo");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(config.join("providers")).unwrap();
        std::fs::create_dir_all(config.join("models")).unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .arg(&repo)
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(repo.join("tracked.txt"), "before").unwrap();
        std::fs::write(
            home.join("config.toml"),
            format!(
                r#"
default_model = "{MODEL_REF}"
lang = "en"

[update]
auto_update = false

[providers.deepseek]
protocol = "openai_chat"
base_url = "{PROVIDER_BASE_URL}"
api_key_env = "DEEPSEEK_API_KEY"

[models."deepseek-v4.1-flash-1017"]
provider = "deepseek"
model_id = "deepseek-v4.1-flash-1017"
protocol = "openai_chat"
context_window = 1048576
reliable_context = 786432
max_output_tokens = 65536
reasoning = true
reasoning_style = "thinking_flag"
supported_efforts = ["low", "high", "max"]
reasoning_effort = "low"
thinking_supports_forced_tool_choice = false
passback_reasoning_content = true
"#
            ),
        )
        .unwrap();
        Self {
            _temp: temp,
            home,
            config,
            repo,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_leveler"));
        command
            .arg("--repo")
            .arg(&self.repo)
            .env("HOME", &self.home)
            .env("LEVELER_HOME", &self.home)
            .env("LEVELER_CONFIG_DIR", &self.config)
            .stdin(Stdio::null());
        command
    }

    fn serve(&self, name: &str) -> (ManagedChild, PathBuf) {
        let ready = self.home.join(format!("{name}-ready.json"));
        let stderr = self.home.join(format!("{name}-stderr.txt"));
        let mut command = self.command();
        command
            .args(["serve", "--ready-json"])
            .arg(&ready)
            .stdout(Stdio::null())
            .stderr(Stdio::from(std::fs::File::create(&stderr).unwrap()));
        (ManagedChild::spawn(&mut command).unwrap(), ready)
    }

    fn state_dir(&self) -> PathBuf {
        let dirs: Vec<_> = std::fs::read_dir(self.home.join("state/projects"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.is_dir())
            .collect();
        assert_eq!(dirs.len(), 1, "one isolated project state namespace");
        dirs[0].clone()
    }

    fn socket(&self) -> PathBuf {
        let sockets: Vec<_> = std::fs::read_dir(self.home.join("run/sockets"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "sock"))
            .collect();
        assert_eq!(sockets.len(), 1, "one runtime socket");
        sockets[0].clone()
    }

    async fn db(&self) -> Database {
        Database::connect(&self.state_dir().join("sessions.db"))
            .await
            .unwrap()
    }
}

async fn ready(path: &Path, child: &mut ManagedChild) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if path.exists() {
            return;
        }
        if let Some(status) = child.try_wait().unwrap() {
            let stderr = path.with_file_name(
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .replace("-ready.json", "-stderr.txt"),
            );
            panic!(
                "runtime exited before readiness ({status}); stderr={:?}",
                std::fs::read_to_string(stderr).unwrap_or_default()
            );
        }
        assert!(
            Instant::now() < deadline,
            "runtime did not become ready: {}",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn create_chat_session(
    client: &LocalSocketRuntimeClient,
    goal: &str,
) -> leveler_core::SessionId {
    client
        .create_session(CreateSessionRequest {
            request_id: None,
            collaboration: CollaborationMode::Chat,
            workspace: leveler_local_transport::CreateWorkspaceSelection::RuntimeDefault,
            approval_policy: leveler_client_protocol::ApprovalPolicy::AutoApprove,
            goal: goal.to_string(),
            model: None,
            mode: leveler_client_protocol::PermissionProfile::FullAccess,
        })
        .await
        .unwrap()
        .session
        .id
}

async fn submit(
    client: &LocalSocketRuntimeClient,
    session: &leveler_core::SessionId,
    content: &str,
) {
    client
        .send(ClientCommand::SubmitMessage {
            session_id: session.clone(),
            content: content.to_string(),
            attachments: Vec::new(),
        })
        .await
        .expect("the runtime must admit the message");
}

#[derive(Debug, Default)]
struct TurnObservation {
    calls: Vec<String>,
    text: String,
    delivery: Option<String>,
    outcome: String,
    steers_sent: usize,
}

/// Collect one turn while steering at the FIRST real tool call.
///
/// The appends are sent only after `ToolCallStarted` proves a tool is running,
/// so this is a genuine mid-turn append and never a queued second turn.
async fn run_and_steer(
    client: &LocalSocketRuntimeClient,
    events: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>,
    session: &leveler_core::SessionId,
    steers: Vec<(String, MessageId)>,
    budget: Duration,
) -> TurnObservation {
    let mut turn = TurnObservation::default();
    let deadline = Instant::now() + budget;
    let mut pending = steers;
    loop {
        let now = Instant::now();
        if now >= deadline {
            turn.outcome = "timeout".into();
            return turn;
        }
        let wait = (deadline - now).min(Duration::from_secs(30));
        match tokio::time::timeout(wait, events.recv()).await {
            Err(_) => continue,
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped))) => {
                eprintln!("  [event stream lagged, skipped {skipped}]");
                continue;
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                turn.outcome = "stream_closed".into();
                return turn;
            }
            Ok(Ok(RuntimeEvent::ToolCallStarted { name, .. })) => {
                turn.calls.push(name.clone());
                for (text, id) in pending.drain(..) {
                    eprintln!("  [steer] sending {id} while {name} runs");
                    client
                        .send(ClientCommand::SteerCurrentTurn {
                            session_id: session.clone(),
                            content: text,
                            message_id: Some(id),
                        })
                        .await
                        .expect("the runtime must admit the steer");
                    turn.steers_sent += 1;
                }
            }
            Ok(Ok(RuntimeEvent::AssistantTextDelta { delta, .. })) => turn.text.push_str(&delta),
            Ok(Ok(RuntimeEvent::DeliveryCommitted { text })) => turn.delivery = Some(text),
            Ok(Ok(RuntimeEvent::Notification { level, message })) => {
                eprintln!("  [notification {level:?}] {message}");
            }
            Ok(Ok(event)) => match event {
                RuntimeEvent::TurnCompleted => {
                    turn.outcome = "completed".into();
                    return turn;
                }
                RuntimeEvent::TurnCompletedWithWarnings { reason, .. } => {
                    turn.outcome = format!("completed_with_warnings: {reason}");
                    return turn;
                }
                RuntimeEvent::TurnAnswered => {
                    turn.outcome = "answered".into();
                    return turn;
                }
                RuntimeEvent::TurnPaused { reason } => {
                    turn.outcome = format!("paused: {reason}");
                    return turn;
                }
                RuntimeEvent::TurnIncomplete { reason } => {
                    turn.outcome = format!("incomplete: {reason}");
                    return turn;
                }
                RuntimeEvent::TurnTruncated { error } => {
                    turn.outcome = format!("truncated: {error}");
                    return turn;
                }
                RuntimeEvent::TurnFailed { error, .. } => {
                    turn.outcome = format!("failed: {error}");
                    return turn;
                }
                RuntimeEvent::TurnCancelled => {
                    turn.outcome = "interrupted".into();
                    return turn;
                }
                RuntimeEvent::TaskCancelled => {
                    turn.outcome = "task_cancelled".into();
                    return turn;
                }
                _ => {}
            },
        }
    }
}

/// D1 — real mid-turn steer, one turn, one consumption.
#[test]
#[ignore = "real API: needs DEEPSEEK_API_KEY and spends real model tokens"]
fn real_model_mid_turn_steer_reaches_the_next_round() {
    leveler_test_support::bounded_test(
        "real_model_mid_turn_steer_reaches_the_next_round",
        Duration::from_secs(600),
        || async {
            assert!(
                std::env::var("DEEPSEEK_API_KEY").is_ok_and(|value| !value.trim().is_empty()),
                "DEEPSEEK_API_KEY must carry the real gateway key"
            );
            let env = Environment::new();
            let (mut runtime, ready_file) = env.serve("d1");
            ready(&ready_file, &mut runtime).await;
            let client = LocalSocketRuntimeClient::connect(&env.socket())
                .await
                .unwrap();
            let session = create_chat_session(&client, "steering certification").await;
            let mut events = client.subscribe_session(&session);
            let steer_id = MessageId::new("d1-steer-id");
            let steer = format!(
                "保留现有测试，但把输出格式改为 JSON。最终回答只输出一个 JSON 对象，\
                 必须包含 \"steer_seen\": true 以及标记 {STEER_MARKER}。"
            );

            submit(
                &client,
                &session,
                "Certifying mid-turn steering. Do exactly this:\n\
                 1. Run ONE foreground command with run_command: program \"sh\", \
                 args [\"-c\",\"sleep 25; echo baseline > baseline.txt\"]. \
                 Do not use background, do not wait on other tasks, do not call other tools.\n\
                 2. The user may send an additional instruction while it runs. \
                 After the command returns, follow the LATEST user instruction for your final answer.\n\
                 3. Until then, do not write any file.",
            )
            .await;
            let turn = run_and_steer(
                &client,
                &mut events,
                &session,
                vec![(steer.clone(), steer_id.clone())],
                Duration::from_secs(420),
            )
            .await;
            let delivery = turn
                .delivery
                .clone()
                .filter(|text| !text.trim().is_empty())
                .unwrap_or_else(|| turn.text.clone());
            eprintln!(
                "D1 outcome={} tools={:?} steers={} delivery={delivery}",
                turn.outcome, turn.calls, turn.steers_sent
            );
            assert_ne!(turn.outcome, "timeout", "no terminal: {turn:?}");
            assert_eq!(
                turn.steers_sent, 1,
                "the steer was sent during the tool call"
            );
            assert!(
                delivery.contains(STEER_MARKER),
                "the model did not follow the steer: {delivery}"
            );
            assert!(
                delivery.contains("steer_seen"),
                "the final answer is not the JSON the steer asked for: {delivery}"
            );

            let db = env.db().await;
            let turns = TurnRepository::new(&db).list(&session).await.unwrap();
            assert_eq!(turns.len(), 1, "the steer must not start a second turn");
            let inputs = SessionInputRepository::new(&db)
                .all(&session)
                .await
                .unwrap();
            let row = inputs
                .iter()
                .find(|row| row.id == steer_id.as_str())
                .expect("the steer is journaled");
            assert_eq!(row.state, SessionInputState::Consumed);
            assert_eq!(
                row.turn_id.as_ref().map(|id| id.as_str()),
                Some(turns[0].id.as_str())
            );
            assert_eq!(
                row.content, steer,
                "the journal keeps the user's exact text"
            );
            let messages = MessageRepository::new(&db)
                .load_timed(&session)
                .await
                .unwrap();
            assert_eq!(
                messages
                    .iter()
                    .filter(|message| message.input_id.as_deref() == Some(steer_id.as_str()))
                    .count(),
                1,
                "one transcript row for the steer"
            );
            let rounds = ModelRequestRepository::new(&db)
                .load_for_session(&session)
                .await
                .unwrap()
                .into_iter()
                .filter(|record| record.kind == ModelCallKind::Round)
                .count();
            assert!(
                rounds >= 2,
                "the steer needs a later model round, got {rounds}"
            );
            assert_eq!(
                std::fs::read_to_string(env.repo.join("baseline.txt"))
                    .unwrap()
                    .trim(),
                "baseline",
                "the running tool's side effect happened exactly once"
            );
            export(
                "d1",
                serde_json::json!({
                    "session": session.as_str(),
                    "turn": turns[0].id,
                    "turn_status": turns[0].status,
                    "steer_input_id": steer_id.as_str(),
                    "steer_state": row.state.as_str(),
                    "steer_ordinal": row.ordinal,
                    "delivery": delivery,
                    "tools": turn.calls,
                    "rounds": rounds,
                    "outcome": turn.outcome,
                }),
            );
            let _ = runtime.kill();
        },
    );
}

/// D2 — three appends in one turn, a real compaction, and a follow-up turn
/// that must still name all three requirements.
#[test]
#[ignore = "real API: needs DEEPSEEK_API_KEY and spends real model tokens"]
fn real_model_steers_survive_compaction() {
    leveler_test_support::bounded_test(
        "real_model_steers_survive_compaction",
        Duration::from_secs(900),
        || async {
            assert!(std::env::var("DEEPSEEK_API_KEY").is_ok_and(|v| !v.trim().is_empty()));
            let env = Environment::new();
            let (mut runtime, ready_file) = env.serve("d2");
            ready(&ready_file, &mut runtime).await;
            let client = LocalSocketRuntimeClient::connect(&env.socket())
                .await
                .unwrap();
            let session = create_chat_session(&client, "compaction steering certification").await;
            let mut events = client.subscribe_session(&session);

            let ids = [
                MessageId::new("d2-steer-1"),
                MessageId::new("d2-steer-2"),
                MessageId::new("d2-steer-3"),
            ];
            let steers = [
                format!("补充 A：保持现有测试不变（标记 {}）。", D2_MARKERS[0]),
                format!(
                    "补充 B：新增测试名必须以 r4_ 开头（标记 {}）。",
                    D2_MARKERS[1]
                ),
                format!(
                    "补充 C：最终回答列出三条补充要求并按 A/B/C 标注（标记 {}）。",
                    D2_MARKERS[2]
                ),
            ];
            submit(
                &client,
                &session,
                "Run ONE foreground command with run_command: program \"sh\", \
                 args [\"-c\",\"sleep 20; echo d2 >> d2.log\"]. No background. \
                 The user may add several instructions while it runs; follow all of them \
                 in the order they were sent.",
            )
            .await;
            let pairs: Vec<(String, MessageId)> =
                steers.iter().cloned().zip(ids.iter().cloned()).collect();
            let turn = run_and_steer(
                &client,
                &mut events,
                &session,
                pairs,
                Duration::from_secs(420),
            )
            .await;
            assert_ne!(turn.outcome, "timeout", "steer turn: {turn:?}");
            assert_eq!(turn.steers_sent, 3, "three appends in one running turn");

            let db = env.db().await;
            let turns = TurnRepository::new(&db).list(&session).await.unwrap();
            assert_eq!(turns.len(), 1, "three appends stay in the running turn");
            let before_compact = SessionInputRepository::new(&db)
                .all(&session)
                .await
                .unwrap();
            let mut ordinals = Vec::new();
            for id in &ids {
                let row = before_compact
                    .iter()
                    .find(|row| row.id == id.as_str())
                    .unwrap_or_else(|| panic!("{id} journaled"));
                assert_eq!(row.state, SessionInputState::Consumed);
                ordinals.push(row.ordinal.expect("consumed input has an ordinal"));
            }
            assert!(
                ordinals.windows(2).all(|pair| pair[0] < pair[1]),
                "append order preserved in the transcript: {ordinals:?}"
            );

            client
                .send(ClientCommand::CompactContext {
                    session_id: session.clone(),
                })
                .await
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(180);
            loop {
                assert!(Instant::now() < deadline, "compaction never finished");
                match tokio::time::timeout(Duration::from_secs(10), events.recv()).await {
                    Ok(Ok(RuntimeEvent::ContextCompacted { .. })) => break,
                    Ok(Ok(_)) | Ok(Err(_)) | Err(_) => continue,
                }
            }
            // `ContextCompacted` can land before the compaction turn's own
            // terminal. Wait for the runtime to be idle and drain the leftovers
            // so the follow-up collector cannot adopt the compaction's terminal.
            for _ in 0..600 {
                if client.snapshot(&session).await.unwrap().status == "idle" {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            while events.try_recv().is_ok() {}
            let db = env.db().await;
            assert!(
                ModelRequestRepository::new(&db)
                    .load_for_session(&session)
                    .await
                    .unwrap()
                    .iter()
                    .any(|record| record.kind == ModelCallKind::Compaction),
                "a real compaction model call ran"
            );
            let after_compact = SessionInputRepository::new(&db)
                .all(&session)
                .await
                .unwrap();
            for id in &ids {
                let row = after_compact
                    .iter()
                    .find(|row| row.id == id.as_str())
                    .unwrap();
                assert_eq!(
                    row.state,
                    SessionInputState::Consumed,
                    "compaction must not resurrect or lose {id}"
                );
            }

            // A fresh turn after compaction: the requirements must survive.
            submit(
                &client,
                &session,
                "用一段话复述我在本会话提出的三条补充要求，并原样包含三个标记。",
            )
            .await;
            while events.try_recv().is_ok() {}
            let follow = run_and_steer(
                &client,
                &mut events,
                &session,
                Vec::new(),
                Duration::from_secs(420),
            )
            .await;
            assert_ne!(follow.outcome, "timeout", "follow-up: {follow:?}");
            let delivery = follow
                .delivery
                .clone()
                .filter(|text| !text.trim().is_empty())
                .unwrap_or_else(|| follow.text.clone());
            eprintln!(
                "D2 follow outcome={} tools={:?} delivery={delivery}",
                follow.outcome, follow.calls
            );
            for marker in D2_MARKERS {
                assert!(
                    delivery.contains(marker),
                    "compaction lost {marker}: {delivery}"
                );
            }
            export(
                "d2",
                serde_json::json!({
                    "session": session.as_str(),
                    "steers": [ids[0].as_str(), ids[1].as_str(), ids[2].as_str()],
                    "ordinals": ordinals,
                    "delivery_after_compaction": delivery,
                }),
            );
            let _ = runtime.kill();
        },
    );
}

/// D3 — a pending steer survives a real runtime SIGKILL and is delivered once
/// by the next runtime on the same isolated state.
#[test]
#[ignore = "real API: needs DEEPSEEK_API_KEY, spends tokens, SIGKILLs the runtime"]
fn real_model_pending_steer_survives_runtime_sigkill() {
    leveler_test_support::bounded_test(
        "real_model_pending_steer_survives_runtime_sigkill",
        Duration::from_secs(900),
        || async {
            assert!(std::env::var("DEEPSEEK_API_KEY").is_ok_and(|v| !v.trim().is_empty()));
            let env = Environment::new();
            let (mut runtime_a, ready_a) = env.serve("d3-a");
            ready(&ready_a, &mut runtime_a).await;
            let runtime_a_pid = runtime_a.id();
            let client_a = LocalSocketRuntimeClient::connect(&env.socket())
                .await
                .unwrap();
            let session = create_chat_session(&client_a, "crash steer certification").await;
            let mut events_a = client_a.subscribe_session(&session);

            let steer_id = MessageId::new("d3-steer-id");
            let steer = format!(
                "补充：最终回答必须包含标记 {D3_MARKER}，并说明你是在崩溃恢复后收到这条要求的。"
            );
            submit(
                &client_a,
                &session,
                "Run ONE foreground command with run_command: program \"sh\", \
                 args [\"-c\",\"sleep 45; echo d3 >> d3.log\"]. No background. \
                 The user may add instructions while it runs; follow them.",
            )
            .await;

            // Wait for the tool, send the steer, then kill as soon as the
            // journal says the input is durable (not consumed).
            let deadline = Instant::now() + Duration::from_secs(240);
            loop {
                assert!(Instant::now() < deadline, "the tool never started");
                match tokio::time::timeout(Duration::from_secs(20), events_a.recv()).await {
                    Ok(Ok(RuntimeEvent::ToolCallStarted { .. })) => {
                        client_a
                            .send(ClientCommand::SteerCurrentTurn {
                                session_id: session.clone(),
                                content: steer.clone(),
                                message_id: Some(steer_id.clone()),
                            })
                            .await
                            .unwrap();
                        break;
                    }
                    Ok(Ok(_)) | Ok(Err(_)) | Err(_) => continue,
                }
            }
            let mut pending = None;
            for _ in 0..200 {
                let db = env.db().await;
                pending = SessionInputRepository::new(&db)
                    .load(&session, steer_id.as_str())
                    .await
                    .unwrap();
                if pending
                    .as_ref()
                    .is_some_and(|row| row.state == SessionInputState::Pending)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            let pending = pending.expect("the steer is journaled");
            assert_eq!(pending.state, SessionInputState::Pending);

            runtime_a.kill().expect("SIGKILL runtime A");
            let status = runtime_a.wait().unwrap();
            assert_eq!(status.signal(), Some(9), "real SIGKILL: {status}");
            assert!(!process_alive(runtime_a_pid));

            let db = env.db().await;
            let after_kill = SessionInputRepository::new(&db)
                .load(&session, steer_id.as_str())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                after_kill.state,
                SessionInputState::Pending,
                "an accepted steer must survive the crash unconsumed"
            );

            // A new runtime on the same state.
            let (mut runtime_b, ready_b) = env.serve("d3-b");
            ready(&ready_b, &mut runtime_b).await;
            assert_ne!(runtime_b.id(), runtime_a_pid);
            let client_b = LocalSocketRuntimeClient::connect(&env.socket())
                .await
                .unwrap();
            client_b
                .send(ClientCommand::OpenSession {
                    session_id: session.clone(),
                })
                .await
                .unwrap();
            let mut events_b = client_b.subscribe_session(&session);
            // A real SIGKILL left the tool call's result unknown. The runtime
            // refuses to resume until a person has seen that window; the
            // harness is that person here, and acknowledges it — it does not
            // retry or re-run anything.
            if !client_b
                .snapshot(&session)
                .await
                .unwrap()
                .interrupted_tool_calls
                .is_empty()
            {
                client_b
                    .send(ClientCommand::AcknowledgeCrashWindow {
                        session_id: session.clone(),
                    })
                    .await
                    .unwrap();
                let deadline = Instant::now() + Duration::from_secs(30);
                loop {
                    assert!(Instant::now() < deadline, "no crash-window acknowledgement");
                    match tokio::time::timeout(Duration::from_secs(5), events_b.recv()).await {
                        Ok(Ok(RuntimeEvent::CrashWindowAcknowledged { .. })) => break,
                        Ok(Ok(_)) | Ok(Err(_)) | Err(_) => continue,
                    }
                }
            }
            client_b
                .send(ClientCommand::ResumeTask {
                    session_id: session.clone(),
                    content: "继续刚才的工作，并遵守运行中追加的要求。".into(),
                })
                .await
                .unwrap();
            let turn = run_and_steer(
                &client_b,
                &mut events_b,
                &session,
                Vec::new(),
                Duration::from_secs(420),
            )
            .await;
            let delivery = turn
                .delivery
                .clone()
                .filter(|text| !text.trim().is_empty())
                .unwrap_or_else(|| turn.text.clone());
            eprintln!("D3 resumed outcome={} delivery={delivery}", turn.outcome);
            assert_ne!(turn.outcome, "timeout", "{turn:?}");
            assert!(
                delivery.contains(D3_MARKER),
                "the recovered steer never reached the model: {delivery}"
            );

            let db = env.db().await;
            let rows = SessionInputRepository::new(&db)
                .all(&session)
                .await
                .unwrap();
            let row = rows.iter().find(|row| row.id == steer_id.as_str()).unwrap();
            assert_eq!(row.state, SessionInputState::Consumed);
            let messages = MessageRepository::new(&db)
                .load_timed(&session)
                .await
                .unwrap();
            assert_eq!(
                messages
                    .iter()
                    .filter(|m| m.input_id.as_deref() == Some(steer_id.as_str()))
                    .count(),
                1,
                "the steer is delivered exactly once across both runtimes"
            );
            export(
                "d3",
                serde_json::json!({
                    "session": session.as_str(),
                    "runtime_a_pid": runtime_a_pid,
                    "runtime_b_pid": runtime_b.id(),
                    "steer_input_id": steer_id.as_str(),
                    "state_before_kill": after_kill.state.as_str(),
                    "state_after_resume": row.state.as_str(),
                    "delivery": delivery,
                }),
            );
            let _ = runtime_b.kill();
        },
    );
}

fn process_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
}

/// Write the durable evidence a reviewer can check without this test's stdout.
fn export(name: &str, payload: serde_json::Value) {
    let Some(root) = std::env::var_os("R4_EVIDENCE_DIR") else {
        return;
    };
    let dir = PathBuf::from(root).join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("session.json"),
        serde_json::to_vec_pretty(&payload).unwrap(),
    )
    .unwrap();
}
