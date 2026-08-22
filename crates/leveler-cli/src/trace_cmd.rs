//! `leveler trace`: durable observatory via the same query as TUI `/trace`.

use leveler_app::Application;
use leveler_core::SessionId;
use leveler_project::Layout;
use leveler_storage::SessionRepository;

use crate::output::Line;

pub(crate) async fn cmd_trace(
    layout: Layout,
    session: Option<String>,
    seq: Option<i64>,
    before: u32,
    after: u32,
    json: bool,
) -> anyhow::Result<std::process::ExitCode> {
    let app = Application::assemble(layout)?;
    let db = app.open_database().await?;
    let sid = match session {
        Some(id) => SessionId::new(id),
        None => {
            let list = SessionRepository::new(&db).list().await?;
            let Some(first) = list.into_iter().next() else {
                anyhow::bail!("no sessions in this repository");
            };
            SessionId::new(first.id)
        }
    };
    let loaded =
        leveler_app::observability::query_observability(&db, &sid, seq, before, after).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&loaded)?);
        return Ok(std::process::ExitCode::SUCCESS);
    }

    let s = &loaded.session;
    println!("{}", Line::heading("Runtime Observatory"));
    println!("  session  {}", s.session_id.as_str());
    println!("  goal     {}", s.goal);
    println!("  status   {}   model {}", s.status, s.model);
    println!("  axes     {} / {}", s.work_profile, s.collaboration);
    println!(
        "  requests {}   in {}  out {}  last_lat {:?}",
        s.request_count, s.input_tokens, s.output_tokens, s.last_latency_ms
    );
    println!(
        "  tools    started {}  finished {}   verify×{}  agents {}  repair {}",
        s.tool_started, s.tool_finished, s.verification_runs, s.subagent_started, s.repair_started
    );
    if let Some(last) = s.last_sequence {
        println!(
            "  sequence last {last}   window {}–{}",
            loaded.window_from, loaded.window_to
        );
    }

    if !loaded.window.is_empty() {
        println!("\n{}", Line::heading("Trace"));
        for row in &loaded.window {
            println!(
                "  #{:<5} {:<8} {:<16} {}",
                row.sequence,
                row.class.tag(),
                row.title,
                row.target
            );
        }
    }
    if !loaded.requests.is_empty() {
        println!("\n{}", Line::heading("Requests"));
        for (i, r) in loaded.requests.iter().enumerate() {
            println!(
                "  #{:<3} {:<16} in {:>6} out {:>5} {} {:?}",
                i + 1,
                r.model,
                r.input_tokens,
                r.output_tokens,
                r.latency_ms
                    .map(|ms| format!("{ms}ms"))
                    .unwrap_or_else(|| "—".into()),
                r.finish_reason.as_deref().or(r.error_kind.as_deref())
            );
        }
    }
    if !loaded.tools.is_empty() {
        println!("\n{}", Line::heading("Tools (session-wide)"));
        for t in &loaded.tools {
            println!(
                "  {:<16} calls {:>3} ok {:>3} fail {:>2} unfin {:>2} total {:?} avg {:?}",
                t.name, t.calls, t.succeeded, t.failed, t.unfinished, t.total_ms, t.avg_ms
            );
        }
    }
    if !loaded.agents.is_empty() {
        println!("\n{}", Line::heading("Agents (session-wide)"));
        for a in &loaded.agents {
            println!("  {}  {}  {}  {}", a.nickname, a.role, a.status, a.summary);
        }
    }
    println!("\n{}", Line::heading("Recovery"));
    println!(
        "  interrupted {}  repair {}  snapshots {}  review {:?}",
        loaded.recovery.interrupted_turns,
        loaded.recovery.repair_attempts,
        loaded.recovery.workspace_snapshots,
        loaded.recovery.review_stages
    );
    if !loaded.relations.is_empty() {
        println!("\n{}", Line::heading("Relations"));
        for r in &loaded.relations {
            println!("  #{}  {}  {}", r.sequence, r.kind, r.label);
        }
    }
    Ok(std::process::ExitCode::SUCCESS)
}
