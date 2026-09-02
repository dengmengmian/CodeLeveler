//! Durable observatory query: read EventLog + model_requests, project DTOs.
//! Not a second log. Fail-closed on corrupt canonical rows.

use std::collections::HashMap;

use leveler_client_protocol::{
    OBSERVABILITY_REQUESTS_MAX, OBSERVABILITY_WINDOW_MAX, ObservationClass, UiAgentObservation,
    UiEventRelation, UiObservabilityLoaded, UiObservationField, UiObservationRow,
    UiRecoveryObservation, UiRequestObservation, UiSessionObservation, UiToolAggregate,
    classify_tool,
};
use leveler_core::SessionId;
use leveler_engine::EngineEvent;
use leveler_storage::{
    Database, EventRecord, EventStore, ModelRequestStore, SessionRepository, TurnRepository,
};

use crate::AppError;

const DEFAULT_BEFORE: u32 = 20;
const DEFAULT_AFTER: u32 = 80;

/// Query a session's durable observation. `center_seq = None` uses the latest
/// sequence. Window sizes are capped at [`OBSERVABILITY_WINDOW_MAX`].
pub async fn query_observability(
    db: &Database,
    session_id: &SessionId,
    center_seq: Option<i64>,
    before: u32,
    after: u32,
) -> Result<UiObservabilityLoaded, AppError> {
    let session = SessionRepository::new(db)
        .get(session_id)
        .await
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::NotFound(format!("session {}", session_id.as_str())))?;

    let latest = db
        .latest_sequence(session_id)
        .await
        .map_err(AppError::from)?;
    let latest_seq = latest.unwrap_or(0);
    let after = if after == 0 {
        DEFAULT_AFTER
    } else {
        after.min(OBSERVABILITY_WINDOW_MAX)
    };
    let (from_seq, to_seq) = if let Some(center) = center_seq {
        let before = if before == 0 {
            DEFAULT_BEFORE
        } else {
            before.min(OBSERVABILITY_WINDOW_MAX)
        };
        (
            center.saturating_sub(i64::from(before)),
            center.saturating_add(i64::from(after)),
        )
    } else {
        // Tail: last `after` durable sequences, not a range past the tip.
        let from = latest_seq.saturating_sub(i64::from(after.saturating_sub(1)));
        (from.max(1), latest_seq.max(1))
    };

    let records = db
        .load_window(session_id, from_seq, to_seq)
        .await
        .map_err(AppError::from)?;
    let decoded = decode_records(&records)?;

    let counts = db.count_by_type(session_id).await.map_err(AppError::from)?;
    let count = |t: &str| -> u32 {
        counts
            .iter()
            .find(|(k, _)| k == t)
            .map(|(_, n)| *n as u32)
            .unwrap_or(0)
    };

    // Whole-session `model_requests` (not the event window). Display list is
    // capped; overview counts below use this same store.
    let mut requests = db
        .load_for_session(session_id)
        .await
        .map_err(AppError::from)?;
    if requests.len() > OBSERVABILITY_REQUESTS_MAX {
        let skip = requests.len() - OBSERVABILITY_REQUESTS_MAX;
        requests = requests.split_off(skip);
    }
    let request_views: Vec<UiRequestObservation> = requests.iter().map(request_view).collect();
    let (avg_latency_ms, last_latency_ms, request_failures, request_retries, in_tok, out_tok) =
        request_stats(&requests);

    let turns = TurnRepository::new(db)
        .list(session_id)
        .await
        .map_err(AppError::from)?;
    let interrupted_turns = turns
        .iter()
        .filter(|t| t.status == "interrupted" || (t.status == "running" && t.finished_at.is_none()))
        .count() as u32;

    let window = project_window(&decoded);
    // Session-wide: tool lifecycle rows only. Never the event window, never
    // the full log into the TUI.
    let tool_rows = db
        .load_by_types(session_id, &["tool_call_started", "tool_call_finished"])
        .await
        .map_err(AppError::from)?;
    let tools = aggregate_tools(&decode_records(&tool_rows)?);
    // Session-wide: sub-agent start/finish only. Same class of bug Tools had
    // when `collect_agents` read the event window.
    let agent_rows = db
        .load_by_types(session_id, &["sub_agent_started", "sub_agent_finished"])
        .await
        .map_err(AppError::from)?;
    let agents = collect_agents(&decode_records(&agent_rows)?);
    let review_stages = collect_review_stages(&decoded);
    let relations = if let Some(seq) = center_seq {
        relations_for(&decoded, seq)
    } else {
        Vec::new()
    };

    Ok(UiObservabilityLoaded {
        session: UiSessionObservation {
            session_id: session_id.clone(),
            goal: session.goal,
            repository: session.repository,
            created_at: session.created_at,
            updated_at: session.updated_at,
            status: session.status.as_str().to_string(),
            model: session.model,
            work_profile: session.work_profile,
            collaboration: session.collaboration,
            last_sequence: latest,
            request_count: request_views.len() as u32,
            input_tokens: in_tok,
            output_tokens: out_tok,
            avg_latency_ms,
            last_latency_ms,
            request_failures,
            request_retries,
            tool_started: count("tool_call_started"),
            tool_finished: count("tool_call_finished"),
            verification_runs: count("verification_started"),
            compact_count: count("compacted"),
            subagent_started: count("sub_agent_started"),
            repair_started: count("repair_started"),
        },
        window,
        window_from: from_seq,
        window_to: to_seq,
        requests: request_views,
        tools,
        agents,
        recovery: UiRecoveryObservation {
            interrupted_turns,
            repair_attempts: count("repair_started"),
            workspace_snapshots: count("workspace_snapshot_created"),
            review_stages,
        },
        relations,
    })
}

fn decode_records(records: &[EventRecord]) -> Result<Vec<(EventRecord, EngineEvent)>, AppError> {
    let mut out = Vec::with_capacity(records.len());
    for rec in records {
        let event = EngineEvent::from_payload(&rec.payload).map_err(|e| {
            AppError::Engine(format!(
                "corrupt authoritative event: session {} sequence {} type '{}': {e}",
                rec.session_id, rec.sequence, rec.event_type
            ))
        })?;
        out.push((rec.clone(), event));
    }
    Ok(out)
}

fn project_window(decoded: &[(EventRecord, EngineEvent)]) -> Vec<UiObservationRow> {
    let mut open: HashMap<String, (usize, i64)> = HashMap::new();
    let mut rows: Vec<UiObservationRow> = Vec::new();
    for (rec, ev) in decoded {
        if let Some(mut row) = project_event(rec, ev) {
            if let EngineEvent::ToolCallStarted { call_id, .. } = ev {
                open.insert(call_id.clone(), (rows.len(), rec.sequence));
            }
            if let EngineEvent::ToolCallFinished { call_id, .. } = ev
                && let Some((idx, start_seq)) = open.remove(call_id)
                && let (Some(start_ms), Some(end_ms)) = (
                    parse_millis(&decoded[idx].0.created_at),
                    parse_millis(&rec.created_at),
                )
            {
                row.duration_ms = Some(end_ms.saturating_sub(start_ms) as u64);
                row.fields.push(UiObservationField {
                    key: "PairedStart".into(),
                    value: start_seq.to_string(),
                });
            }
            rows.push(row);
        }
    }
    rows
}

fn project_event(rec: &EventRecord, ev: &EngineEvent) -> Option<UiObservationRow> {
    let (class, title, target, status, fields) = match ev {
        EngineEvent::ToolCallStarted {
            call_id,
            name,
            arguments,
            agent_id,
            ..
        } => (
            classify_tool(name),
            name.clone(),
            safe_target(arguments),
            "running".into(),
            vec![
                ("Call".into(), call_id.clone()),
                ("Tool".into(), name.clone()),
                (
                    "Agent".into(),
                    agent_id.clone().unwrap_or_else(|| "main".into()),
                ),
            ],
        ),
        EngineEvent::ToolCallFinished {
            call_id,
            name,
            is_error,
            agent_id,
            ..
        } => (
            classify_tool(name),
            name.clone(),
            String::new(),
            if *is_error { "fail" } else { "ok" }.into(),
            vec![
                ("Call".into(), call_id.clone()),
                ("Tool".into(), name.clone()),
                (
                    "Agent".into(),
                    agent_id.clone().unwrap_or_else(|| "main".into()),
                ),
            ],
        ),
        EngineEvent::TurnStarted { turn_id, kind } => (
            ObservationClass::Terminal,
            "turn started".into(),
            kind.as_str().to_string(),
            "info".into(),
            vec![("Turn".into(), turn_id.as_str().to_string())],
        ),
        EngineEvent::TurnFinished {
            turn_id,
            outcome,
            stop_reason,
            rounds,
            ..
        } => (
            ObservationClass::Terminal,
            format!("turn {outcome:?}"),
            stop_reason.clone(),
            match outcome {
                leveler_lifecycle::TurnOutcome::Failed => "fail",
                _ => "ok",
            }
            .into(),
            vec![
                ("Turn".into(), turn_id.as_str().to_string()),
                ("Rounds".into(), rounds.to_string()),
                ("Outcome".into(), format!("{outcome:?}")),
            ],
        ),
        EngineEvent::TaskFinished {
            outcome, reason, ..
        } => (
            ObservationClass::Terminal,
            format!("task {outcome:?}"),
            reason.clone().unwrap_or_default(),
            "info".into(),
            vec![("Outcome".into(), format!("{outcome:?}"))],
        ),
        EngineEvent::VerificationStarted => (
            ObservationClass::Verify,
            "verify started".into(),
            String::new(),
            "running".into(),
            vec![],
        ),
        EngineEvent::VerificationCheck { name, status, .. } => (
            ObservationClass::Verify,
            name.clone(),
            status.clone(),
            if status == "failed" { "fail" } else { "info" }.into(),
            vec![
                ("Check".into(), name.clone()),
                ("Status".into(), status.clone()),
            ],
        ),
        EngineEvent::VerificationFinished { passed } => (
            ObservationClass::Verify,
            "verify finished".into(),
            String::new(),
            if *passed { "ok" } else { "fail" }.into(),
            vec![("Passed".into(), passed.to_string())],
        ),
        EngineEvent::SubAgentStarted {
            id,
            nickname,
            role,
            task,
            profile_id,
            capabilities,
            ..
        } => (
            ObservationClass::Agent,
            format!("{nickname} started"),
            role.clone(),
            "running".into(),
            {
                let mut fields = vec![
                    ("Id".into(), id.clone()),
                    ("Task".into(), truncate(task, 64)),
                ];
                if let Some(pid) = profile_id {
                    fields.push(("Profile".into(), pid.clone()));
                }
                if !capabilities.is_empty() {
                    fields.push(("Capabilities".into(), capabilities.join(", ")));
                }
                fields
            },
        ),
        EngineEvent::SubAgentFinished {
            id,
            nickname,
            ok,
            summary,
            contribution,
        } => (
            ObservationClass::Agent,
            format!("{nickname} done"),
            String::new(),
            if *ok { "ok" } else { "fail" }.into(),
            {
                let mut fields = vec![
                    ("Id".into(), id.clone()),
                    ("Summary".into(), truncate(summary, 64)),
                ];
                // What makes a child's contribution readable without joining
                // the ledger by hand. Absent for children that never reported
                // and for events written before contribution tracing.
                if let Some(c) = contribution {
                    if let Some(pid) = &c.profile_id {
                        fields.push(("Profile".into(), pid.clone()));
                    }
                    if !c.capabilities.is_empty() {
                        fields.push(("Capabilities".into(), c.capabilities.join(", ")));
                    }
                    fields.push((
                        "Findings".into(),
                        format!(
                            "{} reported · {} accepted · {} rejected · {} verified{}",
                            c.findings_total,
                            c.findings_accepted,
                            c.findings_rejected,
                            c.findings_verified,
                            if c.findings_open_blocking > 0 {
                                format!(" · {} open blocking", c.findings_open_blocking)
                            } else {
                                String::new()
                            }
                        ),
                    ));
                }
                fields
            },
        ),
        EngineEvent::ReviewStage { action, detail, .. } => (
            ObservationClass::Agent,
            format!("review {action}"),
            truncate(detail, 64),
            "info".into(),
            vec![("Action".into(), action.clone())],
        ),
        EngineEvent::RepairStarted { attempt } => (
            ObservationClass::Recovery,
            "repair started".into(),
            format!("attempt {attempt}"),
            "info".into(),
            vec![("Attempt".into(), attempt.to_string())],
        ),
        EngineEvent::WorkspaceSnapshotCreated { call_id, .. } => (
            ObservationClass::Recovery,
            "workspace snapshot".into(),
            String::new(),
            "info".into(),
            vec![("Call".into(), call_id.clone())],
        ),
        EngineEvent::Compacted { from, to } => (
            ObservationClass::System,
            "compact".into(),
            format!("{from} → {to}"),
            "info".into(),
            vec![],
        ),
        EngineEvent::UserShellStarted { command, .. } => (
            ObservationClass::Shell,
            "user shell".into(),
            truncate(command, 64),
            "running".into(),
            vec![],
        ),
        EngineEvent::UserShellFinished {
            status,
            duration_ms,
            ..
        } => (
            ObservationClass::Shell,
            "user shell".into(),
            status.clone(),
            if status == "failed" { "fail" } else { "ok" }.into(),
            vec![("Duration".into(), duration_ms.to_string())],
        ),
        EngineEvent::DelegationStage { action, .. } => (
            ObservationClass::Agent,
            format!("delegation {action}"),
            String::new(),
            "info".into(),
            vec![],
        ),
        EngineEvent::GoalIntercepted { kind, detail } => (
            ObservationClass::System,
            format!("goal intercept {kind}"),
            truncate(detail, 64),
            "info".into(),
            vec![],
        ),
        // Prompt-like or high-volume rows stay out of the default trace.
        EngineEvent::ContextSnapshot { .. }
        | EngineEvent::AssistantMessage { .. }
        | EngineEvent::AssistantDelta { .. }
        | EngineEvent::ReasoningDelta { .. }
        | EngineEvent::EvidenceLedgerUpdated { .. }
        | EngineEvent::ProgressUpdated { .. }
        | EngineEvent::TokenUsage { .. } => return None,
        _ => (
            ObservationClass::System,
            rec.event_type.clone(),
            String::new(),
            "info".into(),
            vec![],
        ),
    };
    Some(UiObservationRow {
        sequence: rec.sequence,
        created_at: rec.created_at.clone(),
        turn_id: rec.turn_id.clone(),
        class,
        title,
        target,
        status,
        duration_ms: None,
        event_type: rec.event_type.clone(),
        fields: fields
            .into_iter()
            .map(|(key, value)| UiObservationField { key, value })
            .collect(),
    })
}

fn aggregate_tools(decoded: &[(EventRecord, EngineEvent)]) -> Vec<UiToolAggregate> {
    #[derive(Default)]
    struct Acc {
        calls: u32,
        succeeded: u32,
        failed: u32,
        unfinished: u32,
        total_ms: u64,
        timed: u32,
    }
    // Same identity EventLog uses for dangling calls: (call_id, agent_id).
    let mut open: HashMap<(String, Option<String>), (String, Option<i64>)> = HashMap::new();
    let mut by_name: HashMap<String, Acc> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for (rec, ev) in decoded {
        match ev {
            EngineEvent::ToolCallStarted {
                call_id,
                name,
                agent_id,
                ..
            } => {
                open.insert(
                    (call_id.clone(), agent_id.clone()),
                    (name.clone(), parse_millis(&rec.created_at)),
                );
                let acc = by_name.entry(name.clone()).or_insert_with(|| {
                    order.push(name.clone());
                    Acc::default()
                });
                acc.calls += 1;
            }
            EngineEvent::ToolCallFinished {
                call_id,
                name,
                is_error,
                agent_id,
                ..
            } => {
                let key = (call_id.clone(), agent_id.clone());
                if let Some((start_name, start_ms)) = open.remove(&key) {
                    let acc = by_name.entry(start_name.clone()).or_insert_with(|| {
                        order.push(start_name.clone());
                        Acc::default()
                    });
                    if *is_error {
                        acc.failed += 1;
                    } else {
                        acc.succeeded += 1;
                    }
                    if let (Some(start), Some(end)) = (start_ms, parse_millis(&rec.created_at)) {
                        acc.total_ms += end.saturating_sub(start) as u64;
                        acc.timed += 1;
                    }
                } else {
                    let acc = by_name.entry(name.clone()).or_insert_with(|| {
                        order.push(name.clone());
                        Acc::default()
                    });
                    acc.calls += 1;
                    if *is_error {
                        acc.failed += 1;
                    } else {
                        acc.succeeded += 1;
                    }
                }
            }
            _ => {}
        }
    }
    for (_, (name, _)) in open {
        let acc = by_name.entry(name.clone()).or_insert_with(|| {
            order.push(name.clone());
            Acc::default()
        });
        acc.unfinished += 1;
    }
    order
        .into_iter()
        .map(|name| {
            let acc = &by_name[&name];
            let avg = if acc.timed > 0 {
                Some(acc.total_ms / u64::from(acc.timed))
            } else {
                None
            };
            UiToolAggregate {
                class: classify_tool(&name),
                name,
                calls: acc.calls,
                succeeded: acc.succeeded,
                failed: acc.failed,
                unfinished: acc.unfinished,
                total_ms: if acc.timed > 0 {
                    Some(acc.total_ms)
                } else {
                    None
                },
                avg_ms: avg,
            }
        })
        .collect()
}

fn collect_agents(decoded: &[(EventRecord, EngineEvent)]) -> Vec<UiAgentObservation> {
    let mut by_id: HashMap<String, UiAgentObservation> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for (_, ev) in decoded {
        match ev {
            EngineEvent::SubAgentStarted {
                id,
                nickname,
                role,
                task,
                ..
            } => {
                if !by_id.contains_key(id) {
                    order.push(id.clone());
                }
                by_id.insert(
                    id.clone(),
                    UiAgentObservation {
                        id: id.clone(),
                        nickname: nickname.clone(),
                        role: role.clone(),
                        status: "running".into(),
                        summary: truncate(task, 64),
                    },
                );
            }
            // Not folded into `UiAgentObservation`: that type lives in the
            // client protocol, and widening it pulls in schema regeneration and
            // generated TS — UX-phase work. Contribution is already readable
            // through `leveler trace` (the Findings field above), which is what
            // traceability required.
            EngineEvent::SubAgentFinished {
                id,
                nickname,
                ok,
                summary,
                ..
            } => {
                let row = by_id.entry(id.clone()).or_insert_with(|| {
                    order.push(id.clone());
                    UiAgentObservation {
                        id: id.clone(),
                        nickname: nickname.clone(),
                        role: String::new(),
                        status: String::new(),
                        summary: String::new(),
                    }
                });
                row.nickname = nickname.clone();
                row.status = if *ok { "ok" } else { "fail" }.into();
                row.summary = truncate(summary, 64);
            }
            _ => {}
        }
    }
    order
        .into_iter()
        .filter_map(|id| by_id.remove(&id))
        .collect()
}

fn collect_review_stages(decoded: &[(EventRecord, EngineEvent)]) -> Vec<String> {
    decoded
        .iter()
        .filter_map(|(_, ev)| match ev {
            EngineEvent::ReviewStage { action, .. } => Some(action.clone()),
            _ => None,
        })
        .collect()
}

fn relations_for(decoded: &[(EventRecord, EngineEvent)], seq: i64) -> Vec<UiEventRelation> {
    let Some((_, focus)) = decoded.iter().find(|(r, _)| r.sequence == seq) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    match focus {
        EngineEvent::ToolCallStarted { call_id, .. }
        | EngineEvent::ToolCallFinished { call_id, .. } => {
            for (rec, ev) in decoded {
                match ev {
                    EngineEvent::ToolCallStarted { call_id: id, .. }
                        if id == call_id && rec.sequence != seq =>
                    {
                        out.push(rel(rec.sequence, "pair_start", "tool started"));
                    }
                    EngineEvent::ToolCallFinished { call_id: id, .. }
                        if id == call_id && rec.sequence != seq =>
                    {
                        out.push(rel(rec.sequence, "pair_end", "tool finished"));
                    }
                    _ => {}
                }
            }
        }
        EngineEvent::SubAgentStarted { id, .. } | EngineEvent::SubAgentFinished { id, .. } => {
            for (rec, ev) in decoded {
                match ev {
                    EngineEvent::SubAgentStarted { id: other, .. }
                    | EngineEvent::SubAgentFinished { id: other, .. }
                        if other == id && rec.sequence != seq =>
                    {
                        out.push(rel(rec.sequence, "same_agent", "same sub-agent"));
                    }
                    EngineEvent::ToolCallStarted {
                        agent_id: Some(a), ..
                    }
                    | EngineEvent::ToolCallFinished {
                        agent_id: Some(a), ..
                    } if a == id => {
                        out.push(rel(rec.sequence, "same_agent", "tool by this agent"));
                    }
                    _ => {}
                }
            }
        }
        _ => {
            if let Some(turn) = decoded
                .iter()
                .find(|(r, _)| r.sequence == seq)
                .and_then(|(r, _)| r.turn_id.clone())
            {
                for (rec, _) in decoded {
                    if rec.turn_id.as_deref() == Some(turn.as_str()) && rec.sequence != seq {
                        out.push(rel(rec.sequence, "same_turn", "same turn"));
                    }
                }
            }
        }
    }
    out.truncate(24);
    out
}

fn rel(sequence: i64, kind: &str, label: &str) -> UiEventRelation {
    UiEventRelation {
        sequence,
        kind: kind.into(),
        label: label.into(),
    }
}

fn request_view(r: &leveler_storage::ModelRequestRecord) -> UiRequestObservation {
    UiRequestObservation {
        id: r.id.clone(),
        provider: r.provider.clone(),
        model: r.model.clone(),
        input_tokens: r.input_tokens,
        output_tokens: r.output_tokens,
        finish_reason: r.finish_reason.clone(),
        error_kind: r.error_kind.clone(),
        latency_ms: r.latency_ms,
        retry_count: r.retry_count,
        created_at: r.created_at.to_rfc3339(),
    }
}

fn request_stats(
    rows: &[leveler_storage::ModelRequestRecord],
) -> (Option<u64>, Option<u64>, u32, u32, u64, u64) {
    let mut lat_sum = 0u64;
    let mut lat_n = 0u32;
    let mut last = None;
    let mut failures = 0u32;
    let mut retries = 0u32;
    let mut input = 0u64;
    let mut output = 0u64;
    for r in rows {
        input += r.input_tokens;
        output += r.output_tokens;
        retries += r.retry_count;
        if r.error_kind.is_some() {
            failures += 1;
        }
        if let Some(ms) = r.latency_ms {
            lat_sum += ms;
            lat_n += 1;
            last = Some(ms);
        }
    }
    let avg = if lat_n > 0 {
        Some(lat_sum / u64::from(lat_n))
    } else {
        None
    };
    (avg, last, failures, retries, input, output)
}

fn safe_target(arguments: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(arguments) else {
        return String::new();
    };
    let Some(obj) = v.as_object() else {
        return String::new();
    };
    for key in [
        "path", "file", "pattern", "query", "program", "command", "cmd",
    ] {
        let lk = key.to_ascii_lowercase();
        if lk.contains("token") || lk.contains("secret") || lk.contains("password") {
            continue;
        }
        if let Some(s) = obj.get(key).and_then(|x| x.as_str()) {
            return truncate(s, 64);
        }
    }
    String::new()
}

fn truncate(s: &str, max: usize) -> String {
    let t = s.trim();
    if t.chars().count() <= max {
        return t.to_string();
    }
    let mut out: String = t.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn parse_millis(rfc3339: &str) -> Option<i64> {
    rfc3339
        .parse::<leveler_core::Timestamp>()
        .ok()
        .map(|t| t.timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_core::now;
    use leveler_engine::EngineEvent;
    use leveler_storage::{
        EventStore, ModelRequestRecord, ModelRequestStore, SessionRecord, SessionRepository,
    };

    async fn persist(db: &Database, sid: &SessionId, ev: EngineEvent) {
        let (ty, payload) = ev.to_row().unwrap();
        db.append(sid, None, &ty, &payload, now()).await.unwrap();
    }

    #[tokio::test]
    async fn reconstructs_after_restart_from_durable_facts() {
        let db = Database::connect_in_memory().await.unwrap();
        let rec = SessionRecord::new("/repo", "fix auth", "glm/5", now());
        let sid = SessionId::new(rec.id.clone());
        SessionRepository::new(&db).create(&rec).await.unwrap();

        persist(
            &db,
            &sid,
            EngineEvent::ToolCallStarted {
                call_id: "c1".into(),
                name: "read_file".into(),
                arguments: r#"{"path":"src/lib.rs","token":"SECRET"}"#.into(),
                parallel: false,
                risk: None,
                agent_id: None,
            },
        )
        .await;
        persist(
            &db,
            &sid,
            EngineEvent::ToolCallFinished {
                call_id: "c1".into(),
                name: "read_file".into(),
                is_error: false,
                preview: "FILE BODY MUST NOT LEAK".into(),
                agent_id: None,
            },
        )
        .await;
        persist(&db, &sid, EngineEvent::VerificationStarted).await;
        persist(
            &db,
            &sid,
            EngineEvent::VerificationFinished { passed: false },
        )
        .await;
        persist(&db, &sid, EngineEvent::RepairStarted { attempt: 1 }).await;
        persist(
            &db,
            &sid,
            EngineEvent::SubAgentStarted {
                id: "ag1".into(),
                nickname: "Reviewer".into(),
                role: "reviewer".into(),
                task: "review patch".into(),
                profile_id: Some("reviewer".into()),
                profile_role: Some("reviewer".into()),
                capabilities: vec!["code_review".into(), "verification".into()],
            },
        )
        .await;
        persist(
            &db,
            &sid,
            EngineEvent::SubAgentFinished {
                id: "ag1".into(),
                nickname: "Reviewer".into(),
                ok: true,
                summary: "ok".into(),
                contribution: Some(leveler_lifecycle::ChildResultProjection {
                    child_id: "ag1".into(),
                    role: "reviewer".into(),
                    findings_total: 3,
                    findings_acknowledged: 3,
                    findings_accepted: 2,
                    findings_verified: 1,
                    findings_rejected: 1,
                    findings_open_blocking: 0,
                    ..Default::default()
                }),
            },
        )
        .await;
        db.insert(&ModelRequestRecord {
            id: "req-1".into(),
            session_id: sid.clone(),
            provider: "glm".into(),
            model: "glm-5.2".into(),
            input_tokens: 1200,
            output_tokens: 80,
            finish_reason: Some("stop".into()),
            error_kind: None,
            latency_ms: Some(7100),
            retry_count: 0,
            kind: leveler_storage::ModelCallKind::Round,
            created_at: now(),
        })
        .await
        .unwrap();

        // Simulate a new process: query the same durable stores with no TUI state.
        let loaded = query_observability(&db, &sid, None, 0, 80).await.unwrap();
        assert_eq!(loaded.session.goal, "fix auth");
        assert_eq!(loaded.session.request_count, 1);
        assert_eq!(loaded.session.input_tokens, 1200);
        assert_eq!(loaded.session.last_latency_ms, Some(7100));
        assert!(
            loaded
                .window
                .iter()
                .any(|r| r.class == ObservationClass::Read),
            "read_file must classify as READ: {:?}",
            loaded.window
        );
        // Contribution has to reach the trace, not merely compile. Without
        // this, a child's findings stay joinable only by hand — which is the
        // state MA-VALUE-A was measured in.
        let findings_field = loaded
            .window
            .iter()
            .filter(|r| r.class == ObservationClass::Agent)
            .flat_map(|r| r.fields.iter())
            .find(|f| f.key == "Findings")
            .map(|f| f.value.clone());
        assert_eq!(
            findings_field.as_deref(),
            Some("3 reported · 2 accepted · 1 rejected · 1 verified"),
            "the child's contribution must be readable from the trace: {:?}",
            loaded.window
        );
        assert!(
            loaded
                .window
                .iter()
                .any(|r| r.class == ObservationClass::Verify)
        );
        assert!(
            loaded
                .window
                .iter()
                .any(|r| r.class == ObservationClass::Recovery)
        );
        assert_eq!(loaded.agents.len(), 1);
        assert_eq!(loaded.agents[0].nickname, "Reviewer");
        assert!(
            loaded.window.iter().all(|r| !r.target.contains("SECRET")
                && !format!("{:?}", r.fields).contains("FILE BODY")),
            "secrets and raw tool bodies must stay out"
        );
        let rel = query_observability(&db, &sid, Some(1), 10, 10)
            .await
            .unwrap()
            .relations;
        assert!(
            rel.iter()
                .any(|r| r.kind == "pair_end" || r.kind == "pair_start"),
            "call_id pair must be identity-linked: {rel:?}"
        );
    }

    fn tool_start(id: &str, name: &str) -> EngineEvent {
        EngineEvent::ToolCallStarted {
            call_id: id.into(),
            name: name.into(),
            arguments: "{}".into(),
            parallel: false,
            risk: None,
            agent_id: None,
        }
    }

    fn tool_end(id: &str, name: &str, is_error: bool) -> EngineEvent {
        EngineEvent::ToolCallFinished {
            call_id: id.into(),
            name: name.into(),
            is_error,
            preview: String::new(),
            agent_id: None,
        }
    }

    fn add_ms(t: leveler_core::Timestamp, ms: i64) -> leveler_core::Timestamp {
        leveler_core::Timestamp::from_timestamp_millis(t.timestamp_millis() + ms)
            .expect("timestamp in range")
    }

    fn by_name<'a>(tools: &'a [UiToolAggregate], name: &str) -> &'a UiToolAggregate {
        tools
            .iter()
            .find(|t| t.name == name)
            .unwrap_or_else(|| panic!("missing tool {name} in {tools:?}"))
    }

    #[tokio::test]
    async fn tool_summary_is_session_wide_not_window() {
        let db = Database::connect_in_memory().await.unwrap();
        let rec = SessionRecord::new("/repo", "wide tools", "glm/5", now());
        let sid = SessionId::new(rec.id.clone());
        SessionRepository::new(&db).create(&rec).await.unwrap();

        // 40+20+10+8 pairs = 156 tool events; plus 10 verify markers → 166.
        for i in 0..40 {
            persist(&db, &sid, tool_start(&format!("r{i}"), "read_file")).await;
            persist(&db, &sid, tool_end(&format!("r{i}"), "read_file", false)).await;
        }
        for i in 0..20 {
            persist(&db, &sid, tool_start(&format!("s{i}"), "grep")).await;
            persist(&db, &sid, tool_end(&format!("s{i}"), "grep", false)).await;
        }
        for i in 0..10 {
            persist(&db, &sid, tool_start(&format!("e{i}"), "apply_patch")).await;
            persist(&db, &sid, tool_end(&format!("e{i}"), "apply_patch", false)).await;
        }
        for i in 0..8 {
            persist(&db, &sid, tool_start(&format!("h{i}"), "run_command")).await;
            persist(&db, &sid, tool_end(&format!("h{i}"), "run_command", false)).await;
        }
        for _ in 0..10 {
            persist(&db, &sid, EngineEvent::VerificationStarted).await;
        }

        let loaded = query_observability(&db, &sid, None, 0, 20).await.unwrap();
        assert!(
            loaded.window.len() <= 20,
            "trace window must stay bounded: {}",
            loaded.window.len()
        );
        assert_eq!(by_name(&loaded.tools, "read_file").calls, 40);
        assert_eq!(by_name(&loaded.tools, "grep").calls, 20);
        assert_eq!(by_name(&loaded.tools, "apply_patch").calls, 10);
        assert_eq!(by_name(&loaded.tools, "run_command").calls, 8);
        let window_reads = loaded
            .window
            .iter()
            .filter(|r| r.title == "read_file")
            .count();
        assert!(
            window_reads < 40,
            "window must not contain every read: {window_reads}"
        );
    }

    #[tokio::test]
    async fn tool_duration_pairs_by_call_id_only() {
        let db = Database::connect_in_memory().await.unwrap();
        let rec = SessionRecord::new("/repo", "dur", "glm/5", now());
        let sid = SessionId::new(rec.id.clone());
        SessionRepository::new(&db).create(&rec).await.unwrap();
        let t0 = now();
        let (ty, payload) = tool_start("a1", "read_file").to_row().unwrap();
        db.append(&sid, None, &ty, &payload, t0).await.unwrap();
        let (ty, payload) = tool_end("a1", "read_file", false).to_row().unwrap();
        db.append(&sid, None, &ty, &payload, add_ms(t0, 50))
            .await
            .unwrap();
        let (ty, payload) = tool_start("a2", "read_file").to_row().unwrap();
        db.append(&sid, None, &ty, &payload, add_ms(t0, 100))
            .await
            .unwrap();
        let (ty, payload) = tool_end("a2", "read_file", false).to_row().unwrap();
        db.append(&sid, None, &ty, &payload, add_ms(t0, 200))
            .await
            .unwrap();

        let loaded = query_observability(&db, &sid, None, 0, 80).await.unwrap();
        let read = by_name(&loaded.tools, "read_file");
        assert_eq!(read.calls, 2);
        assert_eq!(read.total_ms, Some(150));
        assert_eq!(read.avg_ms, Some(75));
        assert_eq!(read.failed, 0);
        assert_eq!(read.succeeded, 2);
        assert_eq!(read.unfinished, 0);
    }

    #[tokio::test]
    async fn tool_failures_count_finished_errors_only() {
        let db = Database::connect_in_memory().await.unwrap();
        let rec = SessionRecord::new("/repo", "fail", "glm/5", now());
        let sid = SessionId::new(rec.id.clone());
        SessionRepository::new(&db).create(&rec).await.unwrap();
        for i in 0..3 {
            persist(&db, &sid, tool_start(&format!("ok{i}"), "run_command")).await;
            persist(&db, &sid, tool_end(&format!("ok{i}"), "run_command", false)).await;
        }
        for i in 0..2 {
            persist(&db, &sid, tool_start(&format!("bad{i}"), "run_command")).await;
            persist(&db, &sid, tool_end(&format!("bad{i}"), "run_command", true)).await;
        }
        let loaded = query_observability(&db, &sid, None, 0, 80).await.unwrap();
        let shell = by_name(&loaded.tools, "run_command");
        assert_eq!(shell.calls, 5);
        assert_eq!(shell.failed, 2);
        assert_eq!(shell.succeeded, 3);
        assert_eq!(shell.unfinished, 0);
    }

    #[tokio::test]
    async fn unfinished_tool_is_not_success_and_has_no_duration() {
        let db = Database::connect_in_memory().await.unwrap();
        let rec = SessionRecord::new("/repo", "open", "glm/5", now());
        let sid = SessionId::new(rec.id.clone());
        SessionRepository::new(&db).create(&rec).await.unwrap();
        persist(&db, &sid, tool_start("open1", "read_file")).await;
        let loaded = query_observability(&db, &sid, None, 0, 80).await.unwrap();
        let read = by_name(&loaded.tools, "read_file");
        assert_eq!(read.calls, 1);
        assert_eq!(read.succeeded, 0);
        assert_eq!(read.failed, 0);
        assert_eq!(read.unfinished, 1);
        assert_eq!(read.total_ms, None);
        assert_eq!(read.avg_ms, None);
    }

    fn agent_start(id: &str, nickname: &str, role: &str, task: &str) -> EngineEvent {
        EngineEvent::SubAgentStarted {
            id: id.into(),
            nickname: nickname.into(),
            role: role.into(),
            task: task.into(),
            profile_id: None,
            profile_role: None,
            capabilities: Vec::new(),
        }
    }

    fn agent_end(id: &str, nickname: &str, ok: bool, summary: &str) -> EngineEvent {
        EngineEvent::SubAgentFinished {
            id: id.into(),
            nickname: nickname.into(),
            ok,
            summary: summary.into(),
            contribution: None,
        }
    }

    fn by_agent<'a>(agents: &'a [UiAgentObservation], id: &str) -> &'a UiAgentObservation {
        agents
            .iter()
            .find(|a| a.id == id)
            .unwrap_or_else(|| panic!("missing agent {id} in {agents:?}"))
    }

    #[tokio::test]
    async fn agent_list_is_session_wide_not_window() {
        let db = Database::connect_in_memory().await.unwrap();
        let rec = SessionRecord::new("/repo", "wide agents", "glm/5", now());
        let sid = SessionId::new(rec.id.clone());
        SessionRepository::new(&db).create(&rec).await.unwrap();

        persist(
            &db,
            &sid,
            agent_start(
                "agent-1",
                "Explorer",
                "explorer",
                "Inspect authentication flow",
            ),
        )
        .await;
        persist(
            &db,
            &sid,
            agent_end("agent-1", "Explorer", true, "auth flow mapped"),
        )
        .await;
        persist(
            &db,
            &sid,
            agent_start("agent-2", "Worker", "worker", "Add tests"),
        )
        .await;

        for i in 0..40 {
            persist(&db, &sid, tool_start(&format!("r{i}"), "read_file")).await;
            persist(&db, &sid, tool_end(&format!("r{i}"), "read_file", false)).await;
        }

        persist(
            &db,
            &sid,
            agent_end("agent-2", "Worker", false, "tests still failing"),
        )
        .await;

        let loaded = query_observability(&db, &sid, None, 0, 20).await.unwrap();
        assert!(
            loaded.window.len() <= 20,
            "trace window must stay bounded: {}",
            loaded.window.len()
        );
        let window_agent_rows = loaded
            .window
            .iter()
            .filter(|r| r.class == ObservationClass::Agent)
            .count();
        assert!(
            window_agent_rows < 4,
            "window must not contain every agent lifecycle row: {window_agent_rows}"
        );

        assert_eq!(
            loaded.agents.len(),
            2,
            "agents must be session-wide: {:?}",
            loaded.agents
        );
        let explorer = by_agent(&loaded.agents, "agent-1");
        assert_eq!(explorer.nickname, "Explorer");
        assert_eq!(explorer.role, "explorer");
        assert_eq!(explorer.status, "ok");
        assert_eq!(explorer.summary, "auth flow mapped");

        let worker = by_agent(&loaded.agents, "agent-2");
        assert_eq!(worker.nickname, "Worker");
        assert_eq!(worker.role, "worker");
        assert_eq!(worker.status, "fail");
        assert_eq!(worker.summary, "tests still failing");
    }

    #[tokio::test]
    async fn running_agent_outside_window_still_listed() {
        let db = Database::connect_in_memory().await.unwrap();
        let rec = SessionRecord::new("/repo", "open agent", "glm/5", now());
        let sid = SessionId::new(rec.id.clone());
        SessionRepository::new(&db).create(&rec).await.unwrap();

        persist(
            &db,
            &sid,
            agent_start(
                "agent-1",
                "Explorer",
                "explorer",
                "Inspect authentication flow",
            ),
        )
        .await;
        for _ in 0..30 {
            persist(&db, &sid, EngineEvent::VerificationStarted).await;
        }

        let loaded = query_observability(&db, &sid, None, 0, 20).await.unwrap();
        assert!(
            loaded
                .window
                .iter()
                .all(|r| r.class != ObservationClass::Agent),
            "started agent must sit outside the event window: {:?}",
            loaded.window
        );
        assert_eq!(loaded.agents.len(), 1);
        let explorer = by_agent(&loaded.agents, "agent-1");
        assert_eq!(explorer.role, "explorer");
        assert_eq!(explorer.status, "running");
        assert_eq!(explorer.summary, "Inspect authentication flow");
    }

    #[tokio::test]
    async fn first_spawn_of_each_turn_is_a_distinct_session_agent() {
        let db = Database::connect_in_memory().await.unwrap();
        let rec = SessionRecord::new("/repo", "cross-turn agents", "glm/5", now());
        let sid = SessionId::new(rec.id.clone());
        SessionRepository::new(&db).create(&rec).await.unwrap();

        let turn_a = leveler_core::AgentId::generate().into_inner();
        let turn_b = leveler_core::AgentId::generate().into_inner();
        assert_ne!(turn_a, turn_b);

        persist(
            &db,
            &sid,
            agent_start(
                &turn_a,
                "Explorer",
                "explorer",
                "Inspect authentication flow",
            ),
        )
        .await;
        persist(
            &db,
            &sid,
            agent_end(&turn_a, "Explorer", true, "auth flow mapped"),
        )
        .await;
        persist(
            &db,
            &sid,
            agent_start(&turn_b, "Worker", "worker", "Add tests"),
        )
        .await;
        persist(&db, &sid, agent_end(&turn_b, "Worker", true, "tests added")).await;

        let loaded = query_observability(&db, &sid, None, 0, 80).await.unwrap();
        assert_eq!(loaded.agents.len(), 2, "{:?}", loaded.agents);
        let explorer = by_agent(&loaded.agents, &turn_a);
        let worker = by_agent(&loaded.agents, &turn_b);
        assert_eq!(explorer.nickname, "Explorer");
        assert_eq!(explorer.role, "explorer");
        assert_eq!(explorer.status, "ok");
        assert_eq!(explorer.summary, "auth flow mapped");
        assert_eq!(worker.nickname, "Worker");
        assert_eq!(worker.role, "worker");
        assert_eq!(worker.status, "ok");
        assert_eq!(worker.summary, "tests added");
    }

    fn tool_start_by(id: &str, name: &str, agent: &str) -> EngineEvent {
        EngineEvent::ToolCallStarted {
            call_id: id.into(),
            name: name.into(),
            arguments: "{}".into(),
            parallel: false,
            risk: None,
            agent_id: Some(agent.into()),
        }
    }

    fn tool_end_by(id: &str, name: &str, agent: &str) -> EngineEvent {
        EngineEvent::ToolCallFinished {
            call_id: id.into(),
            name: name.into(),
            is_error: false,
            preview: String::new(),
            agent_id: Some(agent.into()),
        }
    }

    #[tokio::test]
    async fn relations_do_not_cross_distinct_delegated_agents() {
        let db = Database::connect_in_memory().await.unwrap();
        let rec = SessionRecord::new("/repo", "rel", "glm/5", now());
        let sid = SessionId::new(rec.id.clone());
        SessionRepository::new(&db).create(&rec).await.unwrap();

        let explorer = leveler_core::AgentId::generate().into_inner();
        let worker = leveler_core::AgentId::generate().into_inner();
        persist(
            &db,
            &sid,
            agent_start(&explorer, "Explorer", "explorer", "inspect"),
        )
        .await;
        persist(&db, &sid, tool_start_by("e1", "read_file", &explorer)).await;
        persist(&db, &sid, tool_end_by("e1", "read_file", &explorer)).await;
        persist(&db, &sid, agent_end(&explorer, "Explorer", true, "mapped")).await;
        persist(&db, &sid, tool_start("m1", "run_command")).await;
        persist(&db, &sid, tool_end("m1", "run_command", false)).await;
        persist(&db, &sid, agent_start(&worker, "Worker", "worker", "tests")).await;
        persist(&db, &sid, tool_start_by("w1", "run_command", &worker)).await;
        persist(&db, &sid, tool_end_by("w1", "run_command", &worker)).await;
        persist(&db, &sid, agent_end(&worker, "Worker", true, "ok")).await;

        let focused = query_observability(&db, &sid, Some(1), 20, 20)
            .await
            .unwrap();
        assert!(
            focused
                .relations
                .iter()
                .any(|r| r.kind == "same_agent" && r.label == "same sub-agent"),
            "{:?}",
            focused.relations
        );
        assert!(
            focused
                .relations
                .iter()
                .any(|r| r.kind == "same_agent" && r.label == "tool by this agent"),
            "{:?}",
            focused.relations
        );
        let related: std::collections::HashSet<i64> =
            focused.relations.iter().map(|r| r.sequence).collect();
        // Worker start is sequence 7 (1 start, 2-3 tools, 4 finish, 5-6 main tools).
        assert!(
            !related.contains(&7),
            "worker start must not relate to explorer: {:?}",
            focused.relations
        );
        assert!(
            !related.contains(&8) && !related.contains(&9),
            "worker tools must not relate to explorer: {:?}",
            focused.relations
        );
        assert!(
            !related.contains(&5) && !related.contains(&6),
            "main tools must not relate to explorer: {:?}",
            focused.relations
        );
    }

    #[tokio::test]
    async fn session_wide_tool_aggregates_survive_file_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("obs.sqlite");
        let sid = {
            let db = Database::connect(&path).await.unwrap();
            let rec = SessionRecord::new("/repo", "reopen", "glm/5", now());
            let sid = SessionId::new(rec.id.clone());
            SessionRepository::new(&db).create(&rec).await.unwrap();
            for i in 0..6 {
                persist(&db, &sid, tool_start(&format!("r{i}"), "read_file")).await;
                persist(&db, &sid, tool_end(&format!("r{i}"), "read_file", false)).await;
            }
            persist(&db, &sid, tool_start("open", "grep")).await;
            sid
        };
        let db = Database::connect(&path).await.unwrap();
        let loaded = query_observability(&db, &sid, None, 0, 20).await.unwrap();
        assert_eq!(by_name(&loaded.tools, "read_file").calls, 6);
        assert_eq!(by_name(&loaded.tools, "grep").unfinished, 1);
        assert_eq!(by_name(&loaded.tools, "grep").succeeded, 0);
        assert_eq!(by_name(&loaded.tools, "grep").total_ms, None);
    }
}
