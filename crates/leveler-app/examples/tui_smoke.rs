//! End-to-end smoke / live campaign driver of the TUI runtime client.
//!
//! Drives `InProcessRuntimeClient` exactly as the TUI does — subscribe, submit a
//! message (or goal), consume `RuntimeEvent`s — without a full-screen terminal.
//! Approvals default to **auto-approve** so unattended multi-tool runs do not
//! block; set `LEVELER_SMOKE_DENY=1` to auto-deny instead.
//!
//! Round counting: each `AssistantMessageStarted` is one model stream round
//! (live equivalent of mock `MockServer` request counts).
//!
//! Run:
//! ```text
//! cargo run -p leveler-app --example tui_smoke --release -- \
//!   deepseek/deepseek-v4-pro short:"你好" long:"用 list_files/read_file 勘察" \
//!   goal:"检查仓库根目录并完成"
//! ```
//!
//! Env:
//! - `LEVELER_SMOKE_COVERAGE` — append a markdown coverage row file path
//! - `LEVELER_SMOKE_CLASS` — default class label (short|long|goal|multi|edit)
//! - `LEVELER_SMOKE_TIMEOUT_SECS` — per-turn wall timeout (default 300)
//! - `LEVELER_SMOKE_DENY=1` — auto-deny approvals
//! - `LEVELER_SMOKE_LABEL` — scenario name override

use std::env;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{ClientCommand, InteractiveRuntimeClient, RuntimeEvent};
use leveler_execution::PermissionProfile;
use leveler_model::ModelRef;
use leveler_project::Layout;

#[derive(Debug, Clone)]
struct Scenario {
    name: String,
    class: String,
    command: ClientCommand,
}

#[derive(Debug)]
struct TurnReport {
    kind: String,
    wall_ms: u128,
    model_rounds: usize,
    tool_starts: usize,
    tool_ends: usize,
    edit_tools: usize,
    sub_agent_activity: usize,
    sub_agent_updated: usize,
    assistant_deltas: usize,
    notes: Vec<String>,
    empty_spin: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args: Vec<String> = env::args().skip(1).collect();
    let model_arg = if args
        .first()
        .map(|s| s.contains('/') && !s.contains(':'))
        .unwrap_or(false)
    {
        args.remove(0)
    } else {
        env::var("LEVELER_DEFAULT_MODEL").unwrap_or_else(|_| "deepseek/deepseek-v4-pro".into())
    };
    let model = ModelRef::parse(&model_arg).expect("provider/model");

    let layout = Layout::resolve(std::env::current_dir()?, None);
    let repo = layout.repo_root.display().to_string();
    let app = Arc::new(Application::assemble(layout)?);
    let session_id = app
        .create_session(&model, "tui smoke / live campaign")
        .await?;

    let auto_deny = env::var("LEVELER_SMOKE_DENY").ok().as_deref() == Some("1");
    let client = Arc::new(InProcessRuntimeClient::new_with_options(
        app.clone(),
        model.clone(),
        PermissionProfile::Assisted,
        false,
        !auto_deny, // auto_approve when not denying
    ));
    client.attach_session(session_id.clone());

    let snap = client.snapshot(&session_id).await?;
    println!(
        "[snapshot] repo={} model={:?} vision={} models={} auto_deny={auto_deny}",
        snap.repository,
        snap.model,
        snap.vision,
        snap.available_models.len()
    );

    let scenarios = parse_scenarios(&session_id, &args);
    if scenarios.is_empty() {
        anyhow::bail!(
            "no scenarios: pass args like short:你好 long:勘察 goal:完成 multi:并行 edit:改文件"
        );
    }

    let timeout = Duration::from_secs(
        env::var("LEVELER_SMOKE_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(300),
    );

    let mut total_rounds = 0usize;
    let mut reports: Vec<(Scenario, TurnReport)> = Vec::new();

    for sc in scenarios {
        println!(
            "\n========== {} [{}] ==========\n>>> {:?}",
            sc.name, sc.class, sc.command
        );
        let report = drive_turn(client.as_ref(), &sc.command, timeout, auto_deny).await;
        total_rounds += report.model_rounds;
        println!(
            "=== terminal={} rounds={} tools={}/{} edits={} sa_act={} sa_upd={} deltas={} wall={}ms empty_spin={}",
            report.kind,
            report.model_rounds,
            report.tool_starts,
            report.tool_ends,
            report.edit_tools,
            report.sub_agent_activity,
            report.sub_agent_updated,
            report.assistant_deltas,
            report.wall_ms,
            report.empty_spin
        );
        for n in &report.notes {
            println!("  note: {n}");
        }
        if report.empty_spin {
            eprintln!("ERROR: empty-spin on {}", sc.name);
        }
        reports.push((sc, report));
    }

    println!(
        "\n## cumulative model_rounds={total_rounds} scenarios={}",
        reports.len()
    );

    if let Ok(path) = env::var("LEVELER_SMOKE_COVERAGE") {
        append_coverage(
            &PathBuf::from(path),
            &repo,
            &model_arg,
            &reports,
            total_rounds,
        )?;
    }

    let failed = reports
        .iter()
        .any(|(_, r)| r.empty_spin || r.kind == "timeout" || r.kind == "recv_error");
    if failed {
        anyhow::bail!("one or more scenarios empty-spin / timeout / recv_error");
    }
    Ok(())
}

fn parse_scenarios(session_id: &leveler_core::SessionId, args: &[String]) -> Vec<Scenario> {
    let default_class = env::var("LEVELER_SMOKE_CLASS").unwrap_or_else(|_| "short".into());
    let label_prefix = env::var("LEVELER_SMOKE_LABEL").unwrap_or_default();
    let mut out = Vec::new();
    if args.is_empty() {
        out.push(Scenario {
            name: label_or(&label_prefix, "default_short", 0),
            class: default_class,
            command: ClientCommand::SubmitMessage {
                session_id: session_id.clone(),
                content: "用一句话中文回答：你是谁？".into(),
                attachments: Vec::new(),
            },
        });
        return out;
    }

    for (i, raw) in args.iter().enumerate() {
        let (class, content) = if let Some((k, v)) = raw.split_once(':') {
            let key = k.to_ascii_lowercase();
            match key.as_str() {
                "short" | "long" | "goal" | "multi" | "edit" | "chat" => (key, v.to_string()),
                _ => (default_class.clone(), raw.clone()),
            }
        } else {
            (default_class.clone(), raw.clone())
        };

        let name = label_or(&label_prefix, &class, i);
        let command = if class == "goal" {
            ClientCommand::RunGoal {
                session_id: session_id.clone(),
                content,
            }
        } else {
            ClientCommand::SubmitMessage {
                session_id: session_id.clone(),
                content,
                attachments: Vec::new(),
            }
        };
        out.push(Scenario {
            name,
            class,
            command,
        });
    }
    out
}

fn label_or(prefix: &str, class: &str, i: usize) -> String {
    if prefix.is_empty() {
        format!("{class}_{i}")
    } else {
        format!("{prefix}_{class}_{i}")
    }
}

async fn drive_turn(
    client: &dyn InteractiveRuntimeClient,
    command: &ClientCommand,
    wall_limit: Duration,
    auto_deny: bool,
) -> TurnReport {
    let started = Instant::now();
    let mut rx = client.subscribe();
    if let Err(e) = client.send(command.clone()).await {
        return TurnReport {
            kind: format!("send_error:{e}"),
            wall_ms: started.elapsed().as_millis(),
            model_rounds: 0,
            tool_starts: 0,
            tool_ends: 0,
            edit_tools: 0,
            sub_agent_activity: 0,
            sub_agent_updated: 0,
            assistant_deltas: 0,
            notes: vec![format!("send failed: {e}")],
            empty_spin: true,
        };
    }

    let mut model_rounds = 0usize;
    let mut tool_starts = 0usize;
    let mut tool_ends = 0usize;
    let mut edit_tools = 0usize;
    let mut sub_agent_activity = 0usize;
    let mut sub_agent_updated = 0usize;
    let mut assistant_deltas = 0usize;
    let mut notes = Vec::new();
    let mut assistant_buf = String::new();

    loop {
        let remaining = wall_limit.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            notes.push("wall-clock timeout without terminal event".into());
            return finish(
                "timeout",
                started,
                model_rounds,
                tool_starts,
                tool_ends,
                edit_tools,
                sub_agent_activity,
                sub_agent_updated,
                assistant_deltas,
                notes,
            );
        }
        let event = match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(e)) => e,
            Ok(Err(e)) => {
                notes.push(format!("recv error: {e}"));
                return finish(
                    "recv_error",
                    started,
                    model_rounds,
                    tool_starts,
                    tool_ends,
                    edit_tools,
                    sub_agent_activity,
                    sub_agent_updated,
                    assistant_deltas,
                    notes,
                );
            }
            Err(_) => {
                notes.push("wall-clock timeout without terminal event".into());
                return finish(
                    "timeout",
                    started,
                    model_rounds,
                    tool_starts,
                    tool_ends,
                    edit_tools,
                    sub_agent_activity,
                    sub_agent_updated,
                    assistant_deltas,
                    notes,
                );
            }
        };

        match event {
            RuntimeEvent::UserMessageAdded { message } => {
                println!("[user] {}", truncate(&message.text, 200));
            }
            RuntimeEvent::AssistantMessageStarted { .. } => {
                model_rounds += 1;
                assistant_buf.clear();
                println!("[assistant] <started round={model_rounds}>");
            }
            RuntimeEvent::AssistantTextDelta { delta, .. } => {
                assistant_deltas += 1;
                assistant_buf.push_str(&delta);
                print!("{delta}");
            }
            RuntimeEvent::AssistantMessageCompleted { .. } => {
                println!("\n[assistant] <completed round={model_rounds}>");
            }
            RuntimeEvent::ToolCallStarted { name, .. } => {
                tool_starts += 1;
                if matches!(name.as_str(), "apply_patch" | "replace" | "write_file") {
                    edit_tools += 1;
                }
                notes.push(format!("tool_start:{name}"));
                println!("[tool] {name} started");
            }
            RuntimeEvent::ToolCallCompleted {
                ok, duration_ms, ..
            } => {
                tool_ends += 1;
                notes.push(format!("tool_end:ok={ok}"));
                println!("[tool] done ok={ok} {duration_ms}ms");
            }
            RuntimeEvent::SubAgentActivity {
                id,
                tool,
                phase,
                preview,
                is_error,
                ..
            } => {
                sub_agent_activity += 1;
                notes.push(format!("sub_agent:{phase}:{tool}:err={is_error}"));
                println!(
                    "[sub_agent] id={id} {phase} {tool} err={is_error} {}",
                    truncate(&preview, 80)
                );
            }
            RuntimeEvent::SubAgentUpdated {
                nickname, done, ok, ..
            } => {
                sub_agent_updated += 1;
                notes.push(format!("sub_agent_updated:{nickname}:done={done}:ok={ok}"));
                println!("[sub_agent_updated] {nickname} done={done} ok={ok}");
            }
            RuntimeEvent::SubAgentProgress {
                id,
                active,
                input_tokens,
                output_tokens,
                ..
            } => {
                notes.push(format!(
                    "sub_agent_progress:{id}:active={active}:in={input_tokens}:out={output_tokens}"
                ));
                println!(
                    "[sub_agent_progress] id={id} active={active} tokens={input_tokens}/{output_tokens}"
                );
            }
            RuntimeEvent::ClarificationRequested { request } => {
                let answer = request
                    .options
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "选项一".into());
                println!("[clarify] Q: {} — auto-answer {answer:?}", request.question);
                let _ = client
                    .send(ClientCommand::AnswerClarification {
                        request_id: request.id,
                        answer,
                    })
                    .await;
            }
            RuntimeEvent::ApprovalRequested { request } => {
                if auto_deny {
                    println!("[approval] {} — auto-deny", request.summary);
                    let _ = client
                        .send(ClientCommand::ApprovalDecision {
                            request_id: request.id,
                            decision: leveler_client_protocol::ApprovalDecision::Deny,
                        })
                        .await;
                } else {
                    println!("[approval] {} — auto-approve-once", request.summary);
                    let _ = client
                        .send(ClientCommand::ApprovalDecision {
                            request_id: request.id,
                            decision: leveler_client_protocol::ApprovalDecision::ApproveOnce,
                        })
                        .await;
                }
            }
            RuntimeEvent::TurnCompleted => {
                println!("[turn] completed");
                return finish(
                    "completed",
                    started,
                    model_rounds,
                    tool_starts,
                    tool_ends,
                    edit_tools,
                    sub_agent_activity,
                    sub_agent_updated,
                    assistant_deltas,
                    notes,
                );
            }
            RuntimeEvent::TurnAnswered => {
                println!("[turn] answered");
                return finish(
                    "answered",
                    started,
                    model_rounds,
                    tool_starts,
                    tool_ends,
                    edit_tools,
                    sub_agent_activity,
                    sub_agent_updated,
                    assistant_deltas,
                    notes,
                );
            }
            RuntimeEvent::TurnIncomplete { reason } => {
                notes.push(format!("incomplete:{reason}"));
                println!("[turn] incomplete: {reason}");
                return finish(
                    "incomplete",
                    started,
                    model_rounds,
                    tool_starts,
                    tool_ends,
                    edit_tools,
                    sub_agent_activity,
                    sub_agent_updated,
                    assistant_deltas,
                    notes,
                );
            }
            RuntimeEvent::TurnCompletedUnverified { reason } => {
                notes.push(format!("unverified:{reason}"));
                println!("[turn] completed_unverified: {reason}");
                return finish(
                    "completed_unverified",
                    started,
                    model_rounds,
                    tool_starts,
                    tool_ends,
                    edit_tools,
                    sub_agent_activity,
                    sub_agent_updated,
                    assistant_deltas,
                    notes,
                );
            }
            RuntimeEvent::TurnFailed { error } => {
                notes.push(format!("failed:{error}"));
                println!("[turn] FAILED: {error}");
                return finish(
                    "failed",
                    started,
                    model_rounds,
                    tool_starts,
                    tool_ends,
                    edit_tools,
                    sub_agent_activity,
                    sub_agent_updated,
                    assistant_deltas,
                    notes,
                );
            }
            RuntimeEvent::TurnCancelled => {
                println!("[turn] cancelled");
                return finish(
                    "cancelled",
                    started,
                    model_rounds,
                    tool_starts,
                    tool_ends,
                    edit_tools,
                    sub_agent_activity,
                    sub_agent_updated,
                    assistant_deltas,
                    notes,
                );
            }
            RuntimeEvent::ReasoningDelta { .. } => {
                // High-volume stream; omit from campaign logs.
            }
            RuntimeEvent::TokenUsage { .. } | RuntimeEvent::TurnProgress { .. } => {}
            other => {
                // Keep noise low but visible for chrome that matters.
                let s = format!("{other:?}");
                if s.len() < 200 {
                    println!("[event] {s}");
                } else {
                    println!("[event] {}…", &s[..200]);
                }
            }
        }
    }
}

fn finish(
    kind: &str,
    started: Instant,
    model_rounds: usize,
    tool_starts: usize,
    tool_ends: usize,
    edit_tools: usize,
    sub_agent_activity: usize,
    sub_agent_updated: usize,
    assistant_deltas: usize,
    notes: Vec<String>,
) -> TurnReport {
    let empty_spin = matches!(kind, "completed" | "answered" | "completed_unverified")
        && model_rounds == 0
        && tool_starts == 0
        && assistant_deltas == 0;
    TurnReport {
        kind: kind.into(),
        wall_ms: started.elapsed().as_millis(),
        model_rounds,
        tool_starts,
        tool_ends,
        edit_tools,
        sub_agent_activity,
        sub_agent_updated,
        assistant_deltas,
        notes,
        empty_spin,
    }
}

fn truncate(s: &str, max: usize) -> String {
    let mut it = s.chars();
    let head: String = it.by_ref().take(max).collect();
    if it.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

fn append_coverage(
    path: &PathBuf,
    repo: &str,
    model: &str,
    reports: &[(Scenario, TurnReport)],
    total_rounds: usize,
) -> anyhow::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    if f.metadata()?.len() == 0 {
        writeln!(
            f,
            "# Live TUI-path campaign coverage\n\nmodel: `{model}`\n\n| repo | scenario | class | rounds | terminal | wall_ms | tools | edits | sa_act | sa_upd | notes |\n| --- | --- | --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | --- |"
        )?;
    }
    for (sc, r) in reports {
        let note = r.notes.join("; ").replace('|', "/");
        writeln!(
            f,
            "| {} | {} | {} | {} | {} | {} | {}/{} | {} | {} | {} | {} |",
            short_repo(repo),
            sc.name,
            sc.class,
            r.model_rounds,
            r.kind,
            r.wall_ms,
            r.tool_starts,
            r.tool_ends,
            r.edit_tools,
            r.sub_agent_activity,
            r.sub_agent_updated,
            if note.is_empty() {
                "—".into()
            } else {
                truncate(&note, 120)
            }
        )?;
    }
    writeln!(
        f,
        "\n<!-- batch repo={repo} cumulative_rounds_this_batch={total_rounds} -->\n"
    )?;
    Ok(())
}

fn short_repo(repo: &str) -> &str {
    repo.rsplit('/').next().unwrap_or(repo)
}
