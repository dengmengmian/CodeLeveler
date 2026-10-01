//! The `sessions` subcommand: list, show (readable or JSON), and delete
//! persisted sessions.

use leveler_app::Application;
use leveler_project::Layout;
use leveler_storage::SessionRepository;

use crate::cli::SessionsCommand;
use crate::output::Line;

/// Aggregate token usage across a session's model requests: total requests,
/// summed input/output tokens, the reasoning share of the output, and a
/// per-model breakdown (model → (count, input, output)), ordered by first
/// appearance.
///
/// `reasoning` is `None` unless every request reported a breakdown: a partial
/// sum would understate reasoning and silently overstate visible output.
/// Reasoning is a subset of `output`, never an addition to it.
fn summarize_usage(
    requests: &[leveler_storage::ModelRequestRecord],
) -> (usize, u64, u64, Option<u64>, Vec<(String, usize, u64, u64)>) {
    let mut order: Vec<String> = Vec::new();
    let mut per: std::collections::HashMap<String, (usize, u64, u64)> =
        std::collections::HashMap::new();
    let (mut total_in, mut total_out) = (0u64, 0u64);
    let mut reasoning: Option<u64> = if requests.is_empty() { None } else { Some(0) };
    for r in requests {
        total_in += r.input_tokens;
        total_out += r.output_tokens;
        reasoning = match (reasoning, r.reasoning_tokens) {
            (Some(total), Some(value)) => Some(total + value),
            _ => None,
        };
        let entry = per.entry(r.model.clone()).or_insert_with(|| {
            order.push(r.model.clone());
            (0, 0, 0)
        });
        entry.0 += 1;
        entry.1 += r.input_tokens;
        entry.2 += r.output_tokens;
    }
    let breakdown = order
        .into_iter()
        .map(|m| {
            let (c, i, o) = per[&m];
            (m, c, i, o)
        })
        .collect();
    (requests.len(), total_in, total_out, reasoning, breakdown)
}

/// One call lane's aggregate spend: how many calls, tokens, time and money
/// went to the coding work versus each kind of runtime overhead.
struct KindUsage {
    kind: String,
    calls: usize,
    input_tokens: u64,
    output_tokens: u64,
    latency_ms: u64,
    /// Total cost where every call in the lane was priced. `None` means at
    /// least one call had no configured pricing — an unknown total, never a
    /// partial sum presented as complete.
    cost_usd_micros: Option<u64>,
}

/// Summarize a session's model calls by [`leveler_storage::ModelCallKind`], in
/// a stable order (rounds first, then the auxiliary lanes alphabetically).
///
/// Auxiliary lanes are listed separately because they are the runtime's own
/// spend: a user asking why a turn took minutes needs to see whether the cost
/// was the model working or the harness's bookkeeping.
fn summarize_usage_by_kind(requests: &[leveler_storage::ModelRequestRecord]) -> Vec<KindUsage> {
    use leveler_storage::ModelCallKind;
    let mut lanes: std::collections::BTreeMap<&'static str, KindUsage> =
        std::collections::BTreeMap::new();
    for r in requests {
        let kind = match r.kind {
            ModelCallKind::Round => "round",
            ModelCallKind::Compaction => "compaction",
            ModelCallKind::MemoryExtraction => "memory_extraction",
            ModelCallKind::PromptSuggestion => "prompt_suggestion",
            ModelCallKind::AwaySummary => "away_summary",
            ModelCallKind::SemanticRecap => "semantic_recap",
            ModelCallKind::SideQuestion => "side_question",
            ModelCallKind::ProviderProbe => "provider_probe",
            ModelCallKind::Advisory => "advisory",
        };
        let lane = lanes.entry(kind).or_insert_with(|| KindUsage {
            kind: kind.to_string(),
            calls: 0,
            input_tokens: 0,
            output_tokens: 0,
            latency_ms: 0,
            cost_usd_micros: Some(0),
        });
        lane.calls += 1;
        lane.input_tokens += r.input_tokens;
        lane.output_tokens += r.output_tokens;
        lane.latency_ms += r.latency_ms.unwrap_or(0);
        lane.cost_usd_micros = match (lane.cost_usd_micros, r.cost_usd_micros) {
            (Some(total), Some(value)) => Some(total + value),
            _ => None,
        };
    }
    lanes.into_values().collect()
}

/// Render the readable `sessions show` view: config, turns, token usage and an
/// event-log overview.
async fn render_session_show(
    db: &leveler_storage::Database,
    sid: &leveler_core::SessionId,
    session: &leveler_storage::SessionRecord,
) -> anyhow::Result<()> {
    use leveler_storage::{
        EventRepository, ModelRequestRepository, SessionRepository, TurnRepository,
    };

    println!("{}", Line::heading(&format!("Session {}", session.id)));
    println!("  goal:    {}", session.goal);
    println!("  model:   {}", session.model);
    println!(
        "  status:  {}  state: {}",
        session.status.as_str(),
        session.state.as_str()
    );
    if let Some((mode, sandbox, kind, outcome)) = SessionRepository::new(db).execution(sid).await? {
        println!(
            "  kind:    {kind}   mode: {mode}   sandbox: {sandbox}   outcome: {}",
            outcome.map(|o| o.as_str()).unwrap_or("—")
        );
    }
    println!(
        "  created: {}   updated: {}",
        session.created_at, session.updated_at
    );

    let turns = TurnRepository::new(db).list(sid).await?;
    if !turns.is_empty() {
        println!("\n{}", Line::heading("Turns"));
        for t in &turns {
            let detail = t.payload.as_deref().unwrap_or("");
            println!(
                "  {:>2}. {:<7} {:<11} {}",
                t.ordinal, t.kind, t.status, detail
            );
        }
    }

    let requests = ModelRequestRepository::new(db)
        .load_for_session(sid)
        .await?;
    if !requests.is_empty() {
        let (count, total_in, total_out, reasoning, per_model) = summarize_usage(&requests);
        println!("\n{}", Line::heading("Token usage"));
        println!(
            "  {count} request(s)   input: {total_in}   output: {total_out}   total: {}",
            total_in + total_out
        );
        match reasoning {
            Some(reasoning) => println!(
                "  reasoning: {reasoning} of the output   visible output: {}",
                total_out.saturating_sub(reasoning)
            ),
            // Unreported is not zero: say so rather than subtract a partial sum.
            None => println!("  reasoning: not reported by every request"),
        }
        if per_model.len() > 1 {
            for (model, c, i, o) in per_model {
                println!("    {model}: {c} req, in {i}, out {o}");
            }
        }
        // Per-lane attribution: which of a session's model calls were the
        // coding work and which were the runtime's own overhead. This is the
        // "why did that take five minutes?" answer.
        let lanes = summarize_usage_by_kind(&requests);
        if lanes.len() > 1 || lanes.first().is_some_and(|lane| lane.kind != "round") {
            println!("\n{}", Line::heading("By call kind"));
            for lane in lanes {
                let cost = match lane.cost_usd_micros {
                    Some(micros) => format!("${:.4}", micros as f64 / 1_000_000.0),
                    // Unpriced is not free.
                    None => "unpriced".to_string(),
                };
                println!(
                    "  {:<18} {:>4} call(s)  in {:<9} out {:<8} latency {:.1}s  {cost}",
                    lane.kind,
                    lane.calls,
                    lane.input_tokens,
                    lane.output_tokens,
                    lane.latency_ms as f64 / 1000.0,
                );
            }
        }
    }

    let events = EventRepository::new(db).load(sid).await?;

    // Acceptance ledger, reconstructed from the acceptance_evidence events.
    let acceptance: Vec<(String, String, String)> = events
        .iter()
        .filter(|e| e.event_type == "acceptance_evidence")
        .filter_map(
            |e| match leveler_engine::EngineEvent::from_payload(&e.payload) {
                Ok(leveler_engine::EngineEvent::AcceptanceEvidence {
                    id,
                    description,
                    status,
                    ..
                }) => Some((id, description, status)),
                _ => None,
            },
        )
        .collect();
    if !acceptance.is_empty() {
        println!("\n{}", Line::heading("Acceptance criteria"));
        for (id, description, status) in &acceptance {
            let mark = match status.as_str() {
                "met" => console::style("✓").green(),
                "unmet" => console::style("✗").red(),
                _ => console::style("–").dim(),
            };
            println!("  {mark} [{id}] {description}");
        }
    }

    if !events.is_empty() {
        // Compact overview: counts per event type, in first-seen order.
        let mut order: Vec<String> = Vec::new();
        let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for e in &events {
            *counts.entry(e.event_type.clone()).or_insert_with(|| {
                order.push(e.event_type.clone());
                0
            }) += 1;
        }
        println!(
            "\n{}",
            Line::heading(&format!("Event log ({})", events.len()))
        );
        let summary: Vec<String> = order.iter().map(|t| format!("{t}×{}", counts[t])).collect();
        println!("  {}", summary.join("  "));
    }
    Ok(())
}

async fn session_list_status(
    db: &leveler_storage::Database,
    session: &leveler_storage::SessionRecord,
) -> anyhow::Result<String> {
    if session.status != leveler_lifecycle::SessionStatus::Completed {
        return Ok(session.status.as_str().to_string());
    }
    let id = leveler_core::SessionId::new(session.id.clone());
    let last = leveler_storage::EventRepository::new(db)
        .load_last_by_type(&id, "task_finished", None)
        .await?;
    let event = last
        .map(|row| leveler_engine::EngineEvent::from_payload(&row.payload))
        .transpose()?;
    let label = match event {
        Some(leveler_engine::EngineEvent::TaskFinished {
            stop: Some(leveler_lifecycle::StopReason::Answered),
            ..
        }) => "已回答",
        Some(leveler_engine::EngineEvent::TaskFinished {
            stop: Some(leveler_lifecycle::StopReason::Completed),
            ..
        }) => "声明完成",
        _ => "已结束",
    };
    Ok(label.to_string())
}

pub(crate) async fn cmd_sessions(
    layout: Layout,
    command: SessionsCommand,
) -> anyhow::Result<std::process::ExitCode> {
    let app = Application::assemble(layout)?;
    let db = app.open_database().await?;
    let repo = SessionRepository::new(&db);

    match command {
        SessionsCommand::List => {
            let sessions = repo.list().await?;
            if sessions.is_empty() {
                println!("{}", Line::warn("No sessions yet."));
            } else {
                println!("{}", Line::heading("Sessions"));
                for s in sessions {
                    println!(
                        "  {}  [{}]  {}  ({})",
                        s.id,
                        session_list_status(&db, &s).await?,
                        s.goal,
                        s.model
                    );
                }
            }
        }
        SessionsCommand::Show { id, json } => {
            let sid = leveler_core::SessionId::new(id.clone());
            let Some(session) = repo.get(&sid).await? else {
                println!("{}", Line::warn(&format!("No session `{id}`.")));
                return Ok(std::process::ExitCode::FAILURE);
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&session)?);
                return Ok(std::process::ExitCode::SUCCESS);
            }
            render_session_show(&db, &sid, &session).await?;
        }
        SessionsCommand::Delete { id } => {
            app.background_tasks()
                .kill_session(&id)
                .await
                .map_err(anyhow::Error::msg)?;
            if repo
                .delete(&leveler_core::SessionId::new(id.clone()))
                .await?
            {
                println!("{}", Line::ok(&format!("Deleted session `{id}`.")));
            } else {
                println!("{}", Line::warn(&format!("No session `{id}`.")));
                return Ok(std::process::ExitCode::FAILURE);
            }
        }
    }
    Ok(std::process::ExitCode::SUCCESS)
}

#[cfg(test)]
mod usage_tests {
    use super::{summarize_usage, summarize_usage_by_kind};
    use leveler_storage::ModelRequestRecord;

    fn req(model: &str, input: u64, output: u64) -> ModelRequestRecord {
        ModelRequestRecord {
            budget_scope: None,
            estimated_tokens: None,
            id: "r".into(),
            provider_request_id: None,
            session_id: leveler_core::SessionId::new("s"),
            provider: "p".into(),
            model: model.into(),
            input_tokens: input,
            output_tokens: output,
            finish_reason: None,
            error_kind: None,
            latency_ms: None,
            attempt_ms: None,
            connect_ms: None,
            ttft_ms: None,
            max_event_gap_ms: None,
            retry_count: 0,
            kind: leveler_storage::ModelCallKind::Round,
            cached_input_tokens: None,
            projected_input_tokens: None,
            projected_reasoning_tokens: None,
            cost_usd_micros: None,
            agent_id: None,
            created_at: leveler_core::now(),
            reasoning_effort: None,
            reasoning_tokens: None,
        }
    }

    #[test]
    fn sums_totals_and_breaks_down_per_model_in_first_seen_order() {
        let reqs = vec![
            req("deepseek/v4", 100, 20),
            req("kimi/k2", 50, 10),
            req("deepseek/v4", 200, 30),
        ];
        let (count, total_in, total_out, reasoning, per_model) = summarize_usage(&reqs);
        assert_eq!(count, 3);
        assert_eq!(total_in, 350);
        assert_eq!(total_out, 60);
        // No row reported a breakdown, so the reasoning total stays unknown
        // rather than being reported as a measured zero.
        assert_eq!(reasoning, None);
        // First-seen order: deepseek before kimi; deepseek's two requests fold.
        assert_eq!(
            per_model,
            vec![
                ("deepseek/v4".to_string(), 2, 300, 50),
                ("kimi/k2".to_string(), 1, 50, 10),
            ]
        );
    }

    /// A single request with no breakdown makes the session's reasoning total
    /// unknown: a partial sum would understate it and overstate visible output.
    #[test]
    fn one_unreported_breakdown_makes_the_reasoning_total_unknown() {
        let mut with = req("deepseek/v4", 100, 20);
        with.reasoning_tokens = Some(15);
        let reqs = vec![with, req("kimi/k2", 50, 10)];
        let (_, _, _, reasoning, _) = summarize_usage(&reqs);
        assert_eq!(reasoning, None);
    }

    #[test]
    fn reasoning_is_summed_when_every_request_reported_it() {
        let mut a = req("deepseek/v4", 100, 20);
        a.reasoning_tokens = Some(15);
        let mut b = req("deepseek/v4", 200, 30);
        b.reasoning_tokens = Some(25);
        let (_, _, total_out, reasoning, _) = summarize_usage(&[a, b]);
        assert_eq!(total_out, 50, "the output total is not re-derived");
        assert_eq!(reasoning, Some(40));
    }

    #[test]
    fn empty_requests_summarize_to_zero() {
        let (count, total_in, total_out, reasoning, per_model) = summarize_usage(&[]);
        assert_eq!((count, total_in, total_out), (0, 0, 0));
        assert_eq!(reasoning, None, "nothing reported nothing");
        assert!(per_model.is_empty());
    }

    #[test]
    fn call_kinds_are_attributed_to_their_own_lane() {
        let mut round = req("deepseek/v4", 100, 20);
        round.latency_ms = Some(1_000);
        let mut compaction = req("deepseek/v4", 200, 30);
        compaction.kind = leveler_storage::ModelCallKind::Compaction;
        compaction.latency_ms = Some(2_000);
        let mut memory = req("deepseek/v4", 50, 10);
        memory.kind = leveler_storage::ModelCallKind::MemoryExtraction;
        memory.latency_ms = Some(300);
        let lanes = summarize_usage_by_kind(&[round, compaction, memory]);
        let by_name: std::collections::HashMap<_, _> =
            lanes.iter().map(|l| (l.kind.as_str(), l)).collect();
        assert_eq!(by_name["round"].calls, 1);
        assert_eq!(by_name["compaction"].calls, 1);
        assert_eq!(by_name["memory_extraction"].calls, 1);
        assert_eq!(by_name["memory_extraction"].input_tokens, 50);
        assert_eq!(by_name["memory_extraction"].latency_ms, 300);
        // A lane with any unpriced call reports no cost rather than a partial
        // sum: `None` is "unknown", not "free".
        assert_eq!(by_name["round"].cost_usd_micros, None);
    }
}

#[cfg(test)]
mod session_list_tests {
    use super::*;

    #[tokio::test]
    async fn durable_stop_distinguishes_answered_declared_and_legacy_sessions() {
        let db = leveler_storage::Database::connect_in_memory()
            .await
            .unwrap();
        let mut session =
            leveler_storage::SessionRecord::new("/repo", "goal", "mock/m", leveler_core::now());
        SessionRepository::new(&db).create(&session).await.unwrap();
        session.status = leveler_lifecycle::SessionStatus::Completed;
        assert_eq!(session_list_status(&db, &session).await.unwrap(), "已结束");
        let id = leveler_core::SessionId::new(session.id.clone());
        for (stop, label) in [
            ("answered", "已回答"),
            ("completed", "声明完成"),
            ("budget_exhausted", "已结束"),
        ] {
            let payload = serde_json::json!({"type":"task_finished", "payload":{"outcome":"completed", "reason":null, "stop":stop}}).to_string();
            leveler_storage::EventRepository::new(&db)
                .append(&id, None, "task_finished", &payload, leveler_core::now())
                .await
                .unwrap();
            assert_eq!(session_list_status(&db, &session).await.unwrap(), label);
        }
        session.status = leveler_lifecycle::SessionStatus::Running;
        assert_eq!(session_list_status(&db, &session).await.unwrap(), "running");
    }
}
