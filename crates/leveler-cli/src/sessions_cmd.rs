//! The `sessions` subcommand: list, show (readable or JSON), and delete
//! persisted sessions.

use leveler_app::Application;
use leveler_project::Layout;
use leveler_storage::SessionRepository;

use crate::cli::SessionsCommand;
use crate::output::Line;

/// Aggregate token usage across a session's model requests: total requests,
/// summed input/output tokens, and a per-model breakdown (model → (count,
/// input, output)), ordered by first appearance.
fn summarize_usage(
    requests: &[leveler_storage::ModelRequestRecord],
) -> (usize, u64, u64, Vec<(String, usize, u64, u64)>) {
    let mut order: Vec<String> = Vec::new();
    let mut per: std::collections::HashMap<String, (usize, u64, u64)> =
        std::collections::HashMap::new();
    let (mut total_in, mut total_out) = (0u64, 0u64);
    for r in requests {
        total_in += r.input_tokens;
        total_out += r.output_tokens;
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
    (requests.len(), total_in, total_out, breakdown)
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
        let (count, total_in, total_out, per_model) = summarize_usage(&requests);
        println!("\n{}", Line::heading("Token usage"));
        println!(
            "  {count} request(s)   input: {total_in}   output: {total_out}   total: {}",
            total_in + total_out
        );
        if per_model.len() > 1 {
            for (model, c, i, o) in per_model {
                println!("    {model}: {c} req, in {i}, out {o}");
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

pub(crate) async fn cmd_sessions(
    layout: Layout,
    command: SessionsCommand,
) -> anyhow::Result<std::process::ExitCode> {
    let command = match command {
        SessionsCommand::MigrateState { confirm } => {
            migrate_state(&layout, confirm)?;
            return Ok(std::process::ExitCode::SUCCESS);
        }
        other => other,
    };
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
                        s.status.as_str(),
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
        SessionsCommand::MigrateState { .. } => unreachable!("handled before opening state"),
    }
    Ok(std::process::ExitCode::SUCCESS)
}

fn migrate_state(layout: &Layout, confirm: bool) -> anyhow::Result<()> {
    let projects = layout.state_dir.parent().ok_or_else(|| {
        anyhow::anyhow!("invalid state directory: {}", layout.state_dir.display())
    })?;
    let home = projects
        .parent()
        .ok_or_else(|| anyhow::anyhow!("invalid projects directory: {}", projects.display()))?;
    let (source, destination) = leveler_project::legacy_repo_state_paths(home, &layout.repo_root);
    println!("Legacy state source: {}", source.display());
    println!("Hashed state destination: {}", destination.display());
    if !confirm {
        anyhow::bail!("migration not performed; inspect the paths, then rerun with --confirm");
    }
    if leveler_project::migrate_legacy_repo_state(home, &layout.repo_root)? {
        println!("Migrated state by rename; no directories were merged.");
    } else {
        anyhow::bail!("legacy state source does not exist: {}", source.display());
    }
    Ok(())
}

#[cfg(test)]
mod migration_tests {
    use super::*;

    #[test]
    fn migration_requires_confirmation_and_never_merges() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let home = temp.path().join("home");
        let (source, destination) = leveler_project::legacy_repo_state_paths(&home, &repo);
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("sessions.db"), b"legacy").unwrap();
        let layout = Layout {
            repo_root: repo,
            config_dir: temp.path().join("config"),
            state_dir: destination.clone(),
        };

        assert!(migrate_state(&layout, false).is_err());
        assert!(source.exists());
        assert!(!destination.exists());
        migrate_state(&layout, true).unwrap();
        assert!(!source.exists());
        assert_eq!(
            std::fs::read(destination.join("sessions.db")).unwrap(),
            b"legacy"
        );
    }
}

#[cfg(test)]
mod usage_tests {
    use super::summarize_usage;
    use leveler_storage::ModelRequestRecord;

    fn req(model: &str, input: u64, output: u64) -> ModelRequestRecord {
        ModelRequestRecord {
            id: "r".into(),
            session_id: leveler_core::SessionId::new("s"),
            provider: "p".into(),
            model: model.into(),
            input_tokens: input,
            output_tokens: output,
            finish_reason: None,
            error_kind: None,
            latency_ms: None,
            retry_count: 0,
            created_at: leveler_core::now(),
        }
    }

    #[test]
    fn sums_totals_and_breaks_down_per_model_in_first_seen_order() {
        let reqs = vec![
            req("deepseek/v4", 100, 20),
            req("kimi/k2", 50, 10),
            req("deepseek/v4", 200, 30),
        ];
        let (count, total_in, total_out, per_model) = summarize_usage(&reqs);
        assert_eq!(count, 3);
        assert_eq!(total_in, 350);
        assert_eq!(total_out, 60);
        // First-seen order: deepseek before kimi; deepseek's two requests fold.
        assert_eq!(
            per_model,
            vec![
                ("deepseek/v4".to_string(), 2, 300, 50),
                ("kimi/k2".to_string(), 1, 50, 10),
            ]
        );
    }

    #[test]
    fn empty_requests_summarize_to_zero() {
        let (count, total_in, total_out, per_model) = summarize_usage(&[]);
        assert_eq!((count, total_in, total_out), (0, 0, 0));
        assert!(per_model.is_empty());
    }
}
