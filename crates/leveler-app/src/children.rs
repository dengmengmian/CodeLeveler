//! A session's delegated children, projected from the durable record.
//!
//! Read-only over the event log and `model_requests`: the same facts every
//! other reader uses, so a client that reconnects renders exactly what the
//! runtime recorded rather than what a live stream it missed would have said.

use std::collections::HashMap;

use leveler_client_protocol::{UiChildAgent, UiChildState};
use leveler_core::SessionId;
use leveler_engine::EngineEvent;
use leveler_storage::{Database, EventStore, ModelRequestStore};

use crate::AppError;

/// Every child of `session_id`, oldest first. `turn_live` says whether this
/// runtime holds a live turn for the session: without one, no activation can
/// be running, and a child the log still shows as running is reported as
/// interrupted — the state the next turn will record for it.
pub(crate) async fn project_children(
    db: &Database,
    session_id: &SessionId,
    turn_live: bool,
) -> Result<Vec<UiChildAgent>, AppError> {
    let rows = db
        .load_by_types(
            session_id,
            &[
                "sub_agent_started",
                "sub_agent_finished",
                "sub_agent_interrupted",
                "sub_agent_resumed",
            ],
        )
        .await
        .map_err(AppError::from)?;
    let mut children: Vec<UiChildAgent> = Vec::new();
    for row in &rows {
        let event =
            EngineEvent::from_payload(&row.payload).map_err(|e| AppError::Engine(e.to_string()))?;
        match event {
            EngineEvent::SubAgentStarted {
                id,
                nickname,
                role,
                task,
                profile_id,
                read_only,
                spec,
                ..
            } => {
                let spec = spec.unwrap_or_default();
                children.push(UiChildAgent {
                    id,
                    nickname,
                    role,
                    profile_id,
                    read_only,
                    agent: crate::agents::child_agent_identity(&spec),
                    title: spec.title.clone(),
                    purpose: task,
                    state: UiChildState::Running,
                    ok: false,
                    background: spec.background,
                    scope: spec.files,
                    resumes: 0,
                    outcome: None,
                    stop: None,
                    // No bound has fired while the child is unstarted/resumed.
                    limit: None,
                    summary: None,
                    input_tokens: 0,
                    output_tokens: 0,
                    cost_usd_micros: None,
                });
            }
            EngineEvent::SubAgentFinished {
                id,
                ok,
                summary,
                outcome,
                stop,
                limit,
                ..
            } => {
                if let Some(child) = children.iter_mut().find(|c| c.id == id)
                    && child.state != UiChildState::Settled
                {
                    child.state = UiChildState::Settled;
                    child.ok = ok;
                    child.summary = Some(summary);
                    child.outcome = outcome.map(crate::event_bridge::project_child_outcome);
                    child.stop = stop.map(crate::event_bridge::project_child_stop);
                    child.limit = limit.map(crate::event_bridge::project_child_limit);
                }
            }
            EngineEvent::SubAgentInterrupted { id } => {
                if let Some(child) = children.iter_mut().find(|c| c.id == id)
                    && child.state == UiChildState::Running
                {
                    child.state = UiChildState::Interrupted;
                }
            }
            EngineEvent::SubAgentResumed { id, attempt } => {
                if let Some(child) = children.iter_mut().find(|c| c.id == id)
                    && child.state == UiChildState::Interrupted
                {
                    child.state = UiChildState::Running;
                    child.resumes = child.resumes.max(attempt);
                }
            }
            _ => {}
        }
    }
    if !turn_live {
        for child in children
            .iter_mut()
            .filter(|c| c.state == UiChildState::Running)
        {
            child.state = UiChildState::Interrupted;
        }
    }
    if !children.is_empty() {
        let mut usage: HashMap<String, (u64, u64, Option<u64>)> = HashMap::new();
        for record in db
            .load_for_session(session_id)
            .await
            .map_err(AppError::from)?
        {
            let Some(agent) = record.agent_id else {
                continue;
            };
            let entry = usage.entry(agent).or_insert((0, 0, None));
            entry.0 = entry.0.saturating_add(record.input_tokens);
            entry.1 = entry.1.saturating_add(record.output_tokens);
            if let Some(cost) = record.cost_usd_micros {
                entry.2 = Some(entry.2.unwrap_or(0).saturating_add(cost));
            }
        }
        for child in &mut children {
            if let Some((input, output, cost)) = usage.get(&child.id) {
                child.input_tokens = *input;
                child.output_tokens = *output;
                child.cost_usd_micros = *cost;
            }
        }
    }
    Ok(children)
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_client_protocol::{ChildLimit, ChildOutcome, ChildStop};
    use leveler_core::now;
    use leveler_storage::{ModelRequestRecord, SessionRecord, SessionRepository};

    async fn persist(db: &Database, sid: &SessionId, ev: EngineEvent) {
        let (ty, payload) = ev.to_row().unwrap();
        db.append(sid, None, &ty, &payload, now()).await.unwrap();
    }

    fn started(id: &str, nickname: &str, background: bool, files: &[&str]) -> EngineEvent {
        EngineEvent::SubAgentStarted {
            id: id.into(),
            nickname: nickname.into(),
            role: if files.is_empty() {
                "explorer"
            } else {
                "worker"
            }
            .into(),
            task: format!("task of {nickname}"),
            profile_id: Some(
                if files.is_empty() {
                    "explorer"
                } else {
                    "worker"
                }
                .into(),
            ),
            profile_role: None,
            read_only: files.is_empty(),
            spec: Some(leveler_lifecycle::ChildSpawnSpec {
                files: files.iter().map(|f| f.to_string()).collect(),
                background,
                ..Default::default()
            }),
        }
    }

    async fn usage(db: &Database, sid: &SessionId, agent: &str, input: u64, cost: Option<u64>) {
        db.insert(&ModelRequestRecord {
            id: leveler_core::new_uuid_string(),
            provider_request_id: None,
            session_id: sid.clone(),
            provider: "mock".into(),
            model: "m".into(),
            input_tokens: input,
            output_tokens: 10,
            cached_input_tokens: Some(0),
            cost_usd_micros: cost,
            agent_id: Some(agent.into()),
            finish_reason: Some("stop".into()),
            error_kind: None,
            latency_ms: Some(1),
            retry_count: 0,
            kind: leveler_storage::ModelCallKind::Round,
            created_at: now(),
            reasoning_effort: None,
            reasoning_tokens: None,
        })
        .await
        .unwrap();
    }

    /// The agent a child was spawned from is part of its durable projection,
    /// and a child recorded before agents existed projects with none.
    #[tokio::test]
    async fn a_childs_agent_identity_projects_and_old_rows_have_none() {
        let db = Database::connect_in_memory().await.unwrap();
        let rec = SessionRecord::new("/repo", "children", "mock/m", now());
        let sid = SessionId::new(rec.id.clone());
        SessionRepository::new(&db).create(&rec).await.unwrap();
        let mut with_agent = started("c1", "Curie", true, &[]);
        if let EngineEvent::SubAgentStarted {
            spec: Some(spec), ..
        } = &mut with_agent
        {
            spec.agent = Some(Box::new(leveler_lifecycle::ChildAgentSnapshot {
                name: "security-reviewer".into(),
                source: "project".into(),
                fingerprint: "sha256:abc".into(),
                capability: "read_only".into(),
                ..Default::default()
            }));
        }
        persist(&db, &sid, with_agent).await;
        persist(&db, &sid, started("c2", "Newton", true, &[])).await;
        let children = project_children(&db, &sid, true).await.unwrap();
        let curie = children.iter().find(|c| c.id == "c1").unwrap();
        assert_eq!(
            curie.agent.as_ref().map(|a| a.name.as_str()),
            Some("security-reviewer")
        );
        assert!(
            children
                .iter()
                .find(|c| c.id == "c2")
                .unwrap()
                .agent
                .is_none()
        );
    }

    /// The child's short task title is spawn-time identity, persisted with the
    /// spawn record and projected read-only. A child recorded before titles
    /// existed projects with none, and the client falls back to its purpose.
    #[tokio::test]
    async fn a_childs_task_title_projects_and_old_rows_have_none() {
        let db = Database::connect_in_memory().await.unwrap();
        let rec = SessionRecord::new("/repo", "children", "mock/m", now());
        let sid = SessionId::new(rec.id.clone());
        SessionRepository::new(&db).create(&rec).await.unwrap();
        let mut titled = started("c1", "Euclid", true, &[]);
        if let EngineEvent::SubAgentStarted {
            spec: Some(spec), ..
        } = &mut titled
        {
            spec.title = Some("调查 Windows CI 两个 flaky tests".into());
        }
        persist(&db, &sid, titled).await;
        persist(&db, &sid, started("c2", "Newton", true, &[])).await;
        let children = project_children(&db, &sid, true).await.unwrap();
        let euclid = children.iter().find(|c| c.id == "c1").unwrap();
        assert_eq!(
            euclid.title.as_deref(),
            Some("调查 Windows CI 两个 flaky tests")
        );
        assert_eq!(euclid.purpose, "task of Euclid", "the full task is kept");
        assert!(
            children
                .iter()
                .find(|c| c.id == "c2")
                .unwrap()
                .title
                .is_none()
        );
    }

    /// Settled, interrupted-then-resumed and still-open children project from
    /// the log alone, with their typed terminal and their own usage.
    #[tokio::test]
    async fn children_project_from_the_durable_record() {
        let db = Database::connect_in_memory().await.unwrap();
        let rec = SessionRecord::new("/repo", "children", "mock/m", now());
        let sid = SessionId::new(rec.id.clone());
        SessionRepository::new(&db).create(&rec).await.unwrap();

        persist(&db, &sid, started("c1", "Euclid", true, &[])).await;
        persist(
            &db,
            &sid,
            EngineEvent::SubAgentFinished {
                id: "c1".into(),
                nickname: "Euclid".into(),
                ok: false,
                summary: "stopped at its cap".into(),
                contribution: None,
                outcome: Some(leveler_lifecycle::ChildStatus::IncompletePartial),
                stop: Some(leveler_lifecycle::ChildStop::Budget),
                limit: Some(leveler_lifecycle::ChildLimit::Duration),
            },
        )
        .await;
        persist(&db, &sid, started("c2", "Newton", true, &["src/a.rs"])).await;
        persist(
            &db,
            &sid,
            EngineEvent::SubAgentInterrupted { id: "c2".into() },
        )
        .await;
        persist(
            &db,
            &sid,
            EngineEvent::SubAgentResumed {
                id: "c2".into(),
                attempt: 1,
            },
        )
        .await;
        persist(&db, &sid, started("c3", "Curie", false, &[])).await;
        usage(&db, &sid, "c1", 100, Some(5)).await;
        usage(&db, &sid, "c1", 50, None).await;

        let live = project_children(&db, &sid, true).await.unwrap();
        assert_eq!(
            live.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            ["c1", "c2", "c3"]
        );
        let c1 = &live[0];
        assert_eq!(c1.state, UiChildState::Settled);
        assert_eq!(c1.outcome, Some(ChildOutcome::IncompletePartial));
        assert_eq!(c1.stop, Some(ChildStop::Budget));
        assert_eq!(
            c1.limit,
            Some(ChildLimit::Duration),
            "the bound is projected from the durable terminal, not dropped"
        );
        assert_eq!(c1.summary.as_deref(), Some("stopped at its cap"));
        assert_eq!((c1.input_tokens, c1.output_tokens), (150, 20));
        assert_eq!(
            c1.cost_usd_micros,
            Some(5),
            "unpriced rows add nothing, priced ones do"
        );
        assert!(c1.background && c1.read_only);
        let c2 = &live[1];
        assert_eq!(c2.state, UiChildState::Running, "resumed and live");
        assert_eq!(c2.resumes, 1);
        assert_eq!(c2.scope, vec!["src/a.rs".to_string()]);
        assert_eq!(
            c2.cost_usd_micros, None,
            "no priced call: unknown, not zero"
        );
        assert_eq!(live[2].purpose, "task of Curie");

        let dead = project_children(&db, &sid, false).await.unwrap();
        assert_eq!(dead[0].state, UiChildState::Settled);
        assert_eq!(
            dead[1].state,
            UiChildState::Interrupted,
            "no live turn: an open child cannot be running"
        );
        assert_eq!(dead[2].state, UiChildState::Interrupted);
    }
}
