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
    println!("  duration {}", opt_duration(s.duration_ms));
    println!(
        "  requests {}   in {}  cached {}  out {}  last_lat {:?}",
        s.request_count,
        s.input_tokens,
        opt_num(s.cached_input_tokens),
        s.output_tokens,
        s.last_latency_ms
    );
    println!("  cost     {}", opt_cost(s.cost_usd_micros));
    println!(
        "  tools    started {}  finished {}   verify {}  agents {}",
        s.tool_started, s.tool_finished, s.verification, s.subagent_started
    );
    for lane in &s.lanes {
        // A lane with nothing in it says nothing; skip it rather than print a
        // row of zeros that looks like a measurement.
        if lane.requests == 0 {
            continue;
        }
        println!(
            "  {:<8} requests {}  in {}  cached {}  out {}  cost {}",
            lane.lane,
            lane.requests,
            lane.input_tokens,
            opt_num(lane.cached_input_tokens),
            lane.output_tokens,
            opt_cost(lane.cost_usd_micros)
        );
    }
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
        "  interrupted {}  snapshots {}  review {:?}",
        loaded.recovery.interrupted_turns,
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

/// A figure nobody recorded is UNAVAILABLE, never a zero that reads as one.
fn opt_num(v: Option<u64>) -> String {
    v.map_or_else(|| "UNAVAILABLE".to_string(), |n| n.to_string())
}

fn opt_cost(micros: Option<u64>) -> String {
    micros.map_or_else(
        || "UNAVAILABLE".to_string(),
        |m| format!("${:.4}", m as f64 / 1_000_000.0),
    )
}

fn opt_duration(ms: Option<u64>) -> String {
    let Some(ms) = ms else {
        return "UNAVAILABLE".to_string();
    };
    let secs = ms / 1000;
    if secs >= 60 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}
