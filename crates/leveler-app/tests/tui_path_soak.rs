//! TUI client-path soak: drive [`InProcessRuntimeClient`] the same way the TUI
//! does (subscribe → SubmitMessage/RunGoal → drain `RuntimeEvent` to a terminal
//! outcome). Uses a scripted OpenAI-compatible mock so CI has no live key.
//!
//! Coverage matrix (short / long / goal) with cumulative model rounds ≥ 80.
//! Empty-spin / hang-as-success is a hard fail (wall-clock timeout without a
//! terminal event).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{ClientCommand, InteractiveRuntimeClient, RuntimeEvent};
use leveler_execution::PermissionProfile;
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_test_support::{MockResponse, MockServer};

// ── fixtures ──────────────────────────────────────────────────────────────

fn isolate_global_config() {
    use std::sync::OnceLock;
    static EMPTY_HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    let dir = EMPTY_HOME.get_or_init(|| tempfile::tempdir().unwrap());
    // SAFETY: test-only process isolation; single-threaded setup before async work.
    unsafe {
        std::env::set_var("LEVELER_HOME", dir.path());
    }
}

fn write_provider_bundle(root: &std::path::Path, base_url: &str) {
    isolate_global_config();
    std::fs::create_dir_all(root.join("configs/providers")).unwrap();
    std::fs::create_dir_all(root.join("configs/models")).unwrap();
    std::fs::write(
        root.join("configs/providers/mock.yaml"),
        format!("id: mock\nprotocol: openai_chat\nbase_url: {base_url}\n"),
    )
    .unwrap();
    std::fs::write(
        root.join("configs/models/m.yaml"),
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
  context_window: 8192
  reliable_context: 4096
  max_output_tokens: 1024
  max_tool_schema_bytes: 8192
  max_parallel_tool_calls: 1
compatibility:
  middleware: []
  synthesize_tool_call_ids: true
  drop_unsupported_fields: true
"#,
    )
    .unwrap();
}

fn seed_real_mini_project(root: &std::path::Path) {
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn hello() -> &'static str {\n    \"ok\"\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("README.md"),
        "# soak fixture\n\nReal on-disk project for TUI-path soak.\n",
    )
    .unwrap();
    std::fs::create_dir_all(root.join(".leveler")).unwrap();
    // No verify gates — soak focuses on client path / empty-spin / terminals.
    std::fs::write(
        root.join(".leveler/config.yaml"),
        "agents:\n  delegation: true\n",
    )
    .unwrap();
}

fn sse(frames: &[&str]) -> MockResponse {
    MockResponse::sse(frames)
}

fn text_stop(text: &str) -> String {
    serde_json::json!({
        "choices": [{
            "delta": { "content": text },
            "finish_reason": "stop"
        }]
    })
    .to_string()
}

fn tool_call(id: &str, name: &str, arguments: serde_json::Value) -> String {
    serde_json::json!({
        "choices": [{
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": id,
                    "function": {
                        "name": name,
                        "arguments": arguments.to_string()
                    }
                }]
            }
        }]
    })
    .to_string()
}

fn finish_tools() -> String {
    serde_json::json!({
        "choices": [{
            "delta": {},
            "finish_reason": "tool_calls"
        }]
    })
    .to_string()
}

fn tool_round(id: &str, name: &str, arguments: serde_json::Value) -> MockResponse {
    let a = tool_call(id, name, arguments);
    let f = finish_tools();
    sse(&[&a, &f])
}

fn text_round(text: &str) -> MockResponse {
    let t = text_stop(text);
    sse(&[&t])
}

// ── turn driver (TUI path) ────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
enum TerminalKind {
    Completed,
    Answered,
    Incomplete,
    Unverified,
    Failed,
    Cancelled,
    TimedOut,
}

#[derive(Debug, Clone)]
struct TurnReport {
    kind: TerminalKind,
    wall: Duration,
    tool_starts: usize,
    tool_ends: usize,
    assistant_deltas: usize,
    edit_tools: usize,
    sub_agent_activity: usize,
    bad_prompt_hits: Vec<String>,
    notes: Vec<String>,
}

const BAD_PROMPT_MARKERS: &[&str] = &[
    "任务完成",
    "已全面分析",
    "纯问答类任务",
    "no previous context",
    "I am an AI language model",
];

fn scan_bad_prompt(text: &str, hits: &mut Vec<String>) {
    for m in BAD_PROMPT_MARKERS {
        if text.contains(m) {
            hits.push((*m).to_string());
        }
    }
}

async fn drive_turn(
    client: &InProcessRuntimeClient,
    _session_id: &leveler_core::SessionId,
    command: ClientCommand,
    wall_limit: Duration,
) -> TurnReport {
    let mut rx = client.subscribe();
    let started = Instant::now();
    client.send(command).await.expect("send command");

    let mut tool_starts = 0usize;
    let mut tool_ends = 0usize;
    let mut assistant_deltas = 0usize;
    let mut edit_tools = 0usize;
    let mut sub_agent_activity = 0usize;
    let mut bad_prompt_hits = Vec::new();
    let mut notes = Vec::new();
    let mut assistant_buf = String::new();

    loop {
        let remaining = wall_limit.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return TurnReport {
                kind: TerminalKind::TimedOut,
                wall: started.elapsed(),
                tool_starts,
                tool_ends,
                assistant_deltas,
                edit_tools,
                sub_agent_activity,
                bad_prompt_hits,
                notes: {
                    notes.push("wall-clock timeout without terminal event".into());
                    notes
                },
            };
        }
        let event = match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(e)) => e,
            Ok(Err(e)) => {
                notes.push(format!("recv error: {e}"));
                return TurnReport {
                    kind: TerminalKind::TimedOut,
                    wall: started.elapsed(),
                    tool_starts,
                    tool_ends,
                    assistant_deltas,
                    edit_tools,
                    sub_agent_activity,
                    bad_prompt_hits,
                    notes,
                };
            }
            Err(_) => {
                notes.push("wall-clock timeout without terminal event".into());
                return TurnReport {
                    kind: TerminalKind::TimedOut,
                    wall: started.elapsed(),
                    tool_starts,
                    tool_ends,
                    assistant_deltas,
                    edit_tools,
                    sub_agent_activity,
                    bad_prompt_hits,
                    notes,
                };
            }
        };

        match event {
            RuntimeEvent::AssistantTextDelta { delta, .. } => {
                assistant_deltas += 1;
                assistant_buf.push_str(&delta);
                scan_bad_prompt(&delta, &mut bad_prompt_hits);
            }
            RuntimeEvent::AssistantMessageCompleted { .. } => {
                scan_bad_prompt(&assistant_buf, &mut bad_prompt_hits);
            }
            RuntimeEvent::ToolCallStarted { name, .. } => {
                tool_starts += 1;
                if matches!(name.as_str(), "apply_patch" | "replace") {
                    edit_tools += 1;
                }
                notes.push(format!("tool_start:{name}"));
            }
            RuntimeEvent::ToolCallCompleted { ok, .. } => {
                tool_ends += 1;
                notes.push(format!("tool_end:ok={ok}"));
            }
            RuntimeEvent::SubAgentActivity { tool, phase, .. } => {
                sub_agent_activity += 1;
                notes.push(format!("sub_agent:{phase}:{tool}"));
            }
            RuntimeEvent::SubAgentUpdated { nickname, done, .. } => {
                notes.push(format!("sub_agent_updated:{nickname}:done={done}"));
            }
            RuntimeEvent::ApprovalRequested { request } => {
                // Unattended: approve once so write tools can land.
                let _ = client
                    .send(ClientCommand::ApprovalDecision {
                        request_id: request.id,
                        decision: leveler_client_protocol::ApprovalDecision::ApproveOnce,
                    })
                    .await;
            }
            RuntimeEvent::ClarificationRequested { request } => {
                let answer = request
                    .options
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "ok".into());
                let _ = client
                    .send(ClientCommand::AnswerClarification {
                        request_id: request.id,
                        answer,
                    })
                    .await;
            }
            RuntimeEvent::TurnCompleted => {
                return TurnReport {
                    kind: TerminalKind::Completed,
                    wall: started.elapsed(),
                    tool_starts,
                    tool_ends,
                    assistant_deltas,
                    edit_tools,
                    sub_agent_activity,
                    bad_prompt_hits,
                    notes,
                };
            }
            RuntimeEvent::TurnAnswered => {
                return TurnReport {
                    kind: TerminalKind::Answered,
                    wall: started.elapsed(),
                    tool_starts,
                    tool_ends,
                    assistant_deltas,
                    edit_tools,
                    sub_agent_activity,
                    bad_prompt_hits,
                    notes,
                };
            }
            RuntimeEvent::TurnIncomplete { reason } => {
                notes.push(format!("incomplete:{reason}"));
                return TurnReport {
                    kind: TerminalKind::Incomplete,
                    wall: started.elapsed(),
                    tool_starts,
                    tool_ends,
                    assistant_deltas,
                    edit_tools,
                    sub_agent_activity,
                    bad_prompt_hits,
                    notes,
                };
            }
            RuntimeEvent::TurnCompletedUnverified { reason } => {
                notes.push(format!("unverified:{reason}"));
                return TurnReport {
                    kind: TerminalKind::Unverified,
                    wall: started.elapsed(),
                    tool_starts,
                    tool_ends,
                    assistant_deltas,
                    edit_tools,
                    sub_agent_activity,
                    bad_prompt_hits,
                    notes,
                };
            }
            RuntimeEvent::TurnFailed { error } => {
                notes.push(format!("failed:{error}"));
                return TurnReport {
                    kind: TerminalKind::Failed,
                    wall: started.elapsed(),
                    tool_starts,
                    tool_ends,
                    assistant_deltas,
                    edit_tools,
                    sub_agent_activity,
                    bad_prompt_hits,
                    notes,
                };
            }
            RuntimeEvent::TurnCancelled => {
                return TurnReport {
                    kind: TerminalKind::Cancelled,
                    wall: started.elapsed(),
                    tool_starts,
                    tool_ends,
                    assistant_deltas,
                    edit_tools,
                    sub_agent_activity,
                    bad_prompt_hits,
                    notes,
                };
            }
            _ => {}
        }
    }
}

fn is_success_terminal(kind: &TerminalKind) -> bool {
    matches!(
        kind,
        TerminalKind::Completed
            | TerminalKind::Answered
            | TerminalKind::Unverified
            | TerminalKind::Incomplete // incomplete can be a correct gate outcome
    )
}

// ── scenarios ─────────────────────────────────────────────────────────────

struct ScenarioResult {
    name: String,
    class: &'static str, // short | long | goal
    rounds: usize,
    report: TurnReport,
}

async fn setup_app(
    responses: Vec<MockResponse>,
) -> (
    tempfile::TempDir,
    MockServer,
    Arc<Application>,
    Arc<InProcessRuntimeClient>,
    leveler_core::SessionId,
) {
    let server = MockServer::start(responses).await;
    let tmp = tempfile::tempdir().unwrap();
    seed_real_mini_project(tmp.path());
    write_provider_bundle(tmp.path(), &server.base_url());
    let layout = Layout {
        repo_root: tmp.path().to_path_buf(),
        config_dir: tmp.path().join("configs"),
        state_dir: tmp.path().join("state"),
    };
    let app = Arc::new(Application::assemble(layout).unwrap());
    let model = ModelRef::new("mock", "m");
    let session_id = app.create_session(&model, "tui path soak").await.unwrap();
    let client = Arc::new(InProcessRuntimeClient::new_with_options(
        app.clone(),
        model,
        PermissionProfile::Assisted,
        false,
        true, // auto_approve unattended
    ));
    client.attach_session(session_id.clone());
    (tmp, server, app, client, session_id)
}

/// Short chat: model answers in one stream (1 round).
async fn scenario_short_chat(n: usize) -> Vec<ScenarioResult> {
    let mut responses = Vec::new();
    for i in 0..n {
        responses.push(text_round(&format!("简短回复 {i}。")));
    }
    let (_tmp, server, _app, client, session_id) = setup_app(responses).await;
    let mut out = Vec::new();
    let mut prev = server.request_count();
    for i in 0..n {
        let report = drive_turn(
            client.as_ref(),
            &session_id,
            ClientCommand::SubmitMessage {
                session_id: session_id.clone(),
                content: format!("你好，第 {i} 条短问题？"),
                attachments: vec![],
            },
            Duration::from_secs(30),
        )
        .await;
        let now = server.request_count();
        let rounds = now.saturating_sub(prev).max(1);
        prev = now;
        out.push(ScenarioResult {
            name: format!("short_chat_{i}"),
            class: "short",
            rounds,
            report,
        });
    }
    out
}

/// Long multi-tool: alternate distinct tools so search/loop guards do not
/// short-circuit the multi-round campaign, then finish with text.
async fn scenario_long_multitool(tool_rounds: usize) -> ScenarioResult {
    let mut responses = Vec::new();
    for i in 0..tool_rounds {
        let resp = match i % 3 {
            0 => tool_round(
                &format!("t{i}"),
                "list_files",
                serde_json::json!({ "path": if i % 2 == 0 { "src" } else { "." } }),
            ),
            1 => tool_round(
                &format!("t{i}"),
                "read_file",
                serde_json::json!({
                    "path": "src/lib.rs",
                    "start_line": 1,
                    "end_line": 20
                }),
            ),
            _ => tool_round(
                &format!("t{i}"),
                "grep",
                serde_json::json!({
                    "pattern": format!("hello{i}"),
                    "path": "src"
                }),
            ),
        };
        responses.push(resp);
    }
    responses.push(text_round("多工具勘察完成，仓库结构正常。"));
    let (_tmp, server, _app, client, session_id) = setup_app(responses).await;
    let before = server.request_count();
    let report = drive_turn(
        client.as_ref(),
        &session_id,
        ClientCommand::SubmitMessage {
            session_id: session_id.clone(),
            content: "请用 list_files / read_file / grep 多轮勘察 src，再给结论。".into(),
            attachments: vec![],
        },
        Duration::from_secs(180),
    )
    .await;
    let rounds = server.request_count().saturating_sub(before).max(1);
    ScenarioResult {
        name: format!("long_multitool_{tool_rounds}"),
        class: "long",
        rounds,
        report,
    }
}

/// Long edit: apply_patch then answer (exercises TUI edit path events).
async fn scenario_long_edit(tag: usize) -> ScenarioResult {
    let patch = format!(
        "*** Begin Patch\n*** Update File: src/lib.rs\n@@ -1,3 +1,4 @@\n pub fn hello() -> &'static str {{\n-    \"ok\"\n+    \"ok-soak-{tag}\"\n }}\n+// soak marker {tag}\n*** End Patch"
    );
    let responses = vec![
        tool_round(
            &format!("p{tag}"),
            "apply_patch",
            serde_json::json!({ "patch": patch }),
        ),
        text_round(&format!("已更新 hello 返回值为 ok-soak-{tag}。")),
    ];
    let (_tmp, server, _app, client, session_id) = setup_app(responses).await;
    let before = server.request_count();
    let report = drive_turn(
        client.as_ref(),
        &session_id,
        ClientCommand::SubmitMessage {
            session_id: session_id.clone(),
            content: format!("把 hello 的返回值改成 ok-soak-{tag}。"),
            attachments: vec![],
        },
        Duration::from_secs(60),
    )
    .await;
    let rounds = server.request_count().saturating_sub(before).max(1);
    ScenarioResult {
        name: format!("long_edit_{tag}"),
        class: "long",
        rounds,
        report,
    }
}

/// Goal turn: tool then update_goal(complete).
async fn scenario_goal(n: usize) -> Vec<ScenarioResult> {
    let mut responses = Vec::new();
    for i in 0..n {
        responses.push(tool_round(
            &format!("g{i}"),
            "list_files",
            serde_json::json!({ "path": "." }),
        ));
        responses.push(tool_round(
            &format!("ug{i}"),
            "update_goal",
            serde_json::json!({
                "status": "complete",
                "summary": format!("goal {i} done")
            }),
        ));
    }
    let (_tmp, server, _app, client, session_id) = setup_app(responses).await;
    let before = server.request_count();
    let mut out = Vec::new();
    let mut prev = before;
    for i in 0..n {
        let report = drive_turn(
            client.as_ref(),
            &session_id,
            ClientCommand::RunGoal {
                session_id: session_id.clone(),
                content: format!("检查仓库根目录并完成目标 {i}"),
            },
            Duration::from_secs(60),
        )
        .await;
        let now = server.request_count();
        let rounds = now.saturating_sub(prev).max(1);
        prev = now;
        out.push(ScenarioResult {
            name: format!("goal_{i}"),
            class: "goal",
            rounds,
            report,
        });
    }
    out
}

fn write_coverage(path: &PathBuf, results: &[ScenarioResult]) {
    let mut short = 0usize;
    let mut long = 0usize;
    let mut goal = 0usize;
    let mut short_r = 0usize;
    let mut long_r = 0usize;
    let mut goal_r = 0usize;
    let mut lines = String::from("# TUI path soak coverage\n\n");
    lines.push_str(
        "| scenario | class | rounds | terminal | wall_ms | tools | edits | sub_agent | bad_prompts |\n",
    );
    lines.push_str("| --- | --- | ---: | --- | ---: | ---: | ---: | ---: | --- |\n");
    for s in results {
        match s.class {
            "short" => {
                short += 1;
                short_r += s.rounds;
            }
            "long" => {
                long += 1;
                long_r += s.rounds;
            }
            "goal" => {
                goal += 1;
                goal_r += s.rounds;
            }
            _ => {}
        }
        lines.push_str(&format!(
            "| {} | {} | {} | {:?} | {} | {}/{} | {} | sa={} | {} |\n",
            s.name,
            s.class,
            s.rounds,
            s.report.kind,
            s.report.wall.as_millis(),
            s.report.tool_starts,
            s.report.tool_ends,
            s.report.edit_tools,
            s.report.sub_agent_activity,
            if s.report.bad_prompt_hits.is_empty() {
                "—".into()
            } else {
                s.report.bad_prompt_hits.join(";")
            }
        ));
    }
    let total_r = short_r + long_r + goal_r;
    lines.push_str(&format!(
        "\n## Totals\n\n- short scenarios: {short} ({short_r} rounds)\n- long scenarios: {long} ({long_r} rounds)\n- goal scenarios: {goal} ({goal_r} rounds)\n- **cumulative rounds: {total_r}**\n"
    ));
    std::fs::write(path, lines).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tui_path_soak_short_long_goal_reaches_80_rounds() {
    // Matrix sized for ≥80 model rounds on the real InProcessRuntimeClient path.
    // Host loop-guards cap identical multi-tool chains (~7–8 rounds), so we
    // accumulate rounds across many short/edit/goal turns plus multi-tool.
    // short 40 + long multitool ~8 + 12 edits ×2 + 10 goals ×2 ≈ 40+8+24+20 = 92.
    let mut all: Vec<ScenarioResult> = Vec::new();

    all.extend(scenario_short_chat(40).await);
    all.push(scenario_long_multitool(25).await);
    for i in 0..12 {
        all.push(scenario_long_edit(i).await);
    }
    all.extend(scenario_goal(10).await);

    let total_rounds: usize = all.iter().map(|s| s.rounds).sum();
    let short_n = all.iter().filter(|s| s.class == "short").count();
    let long_n = all.iter().filter(|s| s.class == "long").count();
    let goal_n = all.iter().filter(|s| s.class == "goal").count();

    // Write coverage artifact for the goal harness (env override or CARGO_TARGET_TMPDIR).
    let coverage_path = std::env::var_os("LEVELER_SOAK_COVERAGE")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!(
                "leveler-tui-soak-coverage-{}.md",
                std::process::id()
            ))
        });
    write_coverage(&coverage_path, &all);
    eprintln!("soak coverage written to {}", coverage_path.display());

    assert!(
        short_n > 0 && long_n > 0 && goal_n > 0,
        "matrix cells must be non-empty: short={short_n} long={long_n} goal={goal_n}"
    );
    assert!(
        total_rounds >= 80,
        "need ≥80 cumulative model rounds, got {total_rounds}; see {}",
        coverage_path.display()
    );

    for s in &all {
        assert_ne!(
            s.report.kind,
            TerminalKind::TimedOut,
            "{} hung/empty-spin: {:?}",
            s.name,
            s.report.notes
        );
        assert!(
            is_success_terminal(&s.report.kind)
                || matches!(
                    s.report.kind,
                    TerminalKind::Failed | TerminalKind::Cancelled
                ),
            "{} unexpected terminal {:?}: {:?}",
            s.name,
            s.report.kind,
            s.report.notes
        );
        // No hang-as-success: a "success" terminal with zero assistant activity
        // and zero tools is empty-spin.
        if matches!(
            s.report.kind,
            TerminalKind::Completed | TerminalKind::Answered
        ) {
            assert!(
                s.report.assistant_deltas > 0 || s.report.tool_ends > 0 || s.report.tool_starts > 0,
                "{} success with no model/tool activity (empty-spin): {:?}",
                s.name,
                s.report
            );
        }
        assert!(
            s.report.bad_prompt_hits.is_empty(),
            "{} bad TUI-facing closeout copy {:?}: {:?}",
            s.name,
            s.report.bad_prompt_hits,
            s.report.notes
        );
        // Tool starts and ends should not permanently diverge (leaked in-flight).
        assert!(
            s.report.tool_starts <= s.report.tool_ends + 1,
            "{} tool start/end imbalance {}/{}: {:?}",
            s.name,
            s.report.tool_starts,
            s.report.tool_ends,
            s.report.notes
        );
    }

    // Edit scenarios must have exercised an edit tool event for TUI diff path.
    let edit_ok = all
        .iter()
        .filter(|s| s.name.starts_with("long_edit_"))
        .filter(|s| s.report.edit_tools >= 1)
        .count();
    assert!(
        edit_ok >= 1,
        "at least one long_edit must emit apply_patch/replace start: {:?}",
        all.iter()
            .filter(|s| s.name.starts_with("long_edit_"))
            .map(|s| (&s.name, s.report.edit_tools, &s.report.kind))
            .collect::<Vec<_>>()
    );
}
