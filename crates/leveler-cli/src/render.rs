//! Event rendering for the CLI: agent events in text or JSONL form.

use leveler_agent::{AdvisoryKind, AgentEvent};

use crate::cli::OutputFormat;
pub(crate) fn render_event(event: AgentEvent, output: OutputFormat) {
    match output {
        OutputFormat::Text => render_event_text(event),
        OutputFormat::Jsonl => render_event_jsonl(event),
    }
}

fn render_event_text(event: AgentEvent) {
    match event {
        // The CLI renders whole messages, not token deltas.
        AgentEvent::StreamAttemptStarted => {}
        // Introspection only; `/context` in the TUI renders it. The CLI stream
        // stays a clean answer flow.
        AgentEvent::ContextUsage { .. } => {}
        // A model round is about to retry. Ephemeral connectivity, not a
        // transcript entry: a concise status line on stderr, so stdout stays a
        // clean answer stream for piping.
        AgentEvent::ModelRetrying {
            attempt,
            max_attempts,
            ..
        } => {
            eprintln!(
                "{} reconnecting ({attempt}/{max_attempts})",
                console::style("[network]").dim()
            );
        }
        // A durable accounting row, intercepted by the drive loop. The child's
        // running totals reach the screen as SubAgentProgress instead.
        AgentEvent::SubAgentModelRequest { .. } => {}
        AgentEvent::AssistantDelta(_) => {}
        AgentEvent::ReasoningDelta(_) => {}
        // Live command output; the finished tool result carries the record.
        AgentEvent::ToolOutput { .. } => {}
        AgentEvent::AssistantText(text) => {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                println!("{} {trimmed}", console::style("»").cyan());
            }
        }
        AgentEvent::ToolCall {
            name, arguments, ..
        } => {
            println!(
                "{} {} {}",
                console::style("→").blue(),
                console::style(&name).bold(),
                console::style(&arguments).dim()
            );
        }
        AgentEvent::ToolResult {
            name,
            is_error,
            preview,
            ..
        } => {
            let mark = if is_error {
                console::style("✗").red()
            } else {
                console::style("✓").green()
            };
            println!("  {mark} {name}: {}", console::style(preview).dim());
        }
        AgentEvent::WorkspaceSnapshot { .. } => {}
        AgentEvent::Usage { .. } => {}
        AgentEvent::Compacted { from, to } => {
            println!(
                "  {} context compacted {from} → {to} messages",
                console::style("⋯").yellow()
            );
        }
        AgentEvent::ContextSnapshot { .. } => {}
        AgentEvent::PlanUpdated { steps } => {
            println!("{} plan", console::style("☰").blue());
            for s in steps {
                let mark = match s.status.as_str() {
                    "completed" => console::style("[x]").green(),
                    "in_progress" => console::style("[~]").yellow(),
                    _ => console::style("[ ]").dim(),
                };
                println!("  {mark} {}", s.step);
            }
        }
        AgentEvent::SubAgentStarted {
            nickname,
            role,
            task,
            ..
        } => {
            println!(
                "{} sub-agent {}[{role}] started: {task}",
                console::style("↗").magenta(),
                console::style(&nickname).cyan().bold()
            );
        }
        AgentEvent::SubAgentProgress {
            id,
            active,
            input_tokens,
            output_tokens,
            ..
        } => {
            let state = if active { "running" } else { "waiting" };
            println!(
                "{} sub-agent {id} {state} · ↑ {input_tokens} · ↓ {output_tokens}",
                console::style("↻").magenta()
            );
        }
        AgentEvent::SubAgentFinished {
            nickname,
            ok,
            summary,
            ..
        } => {
            let mark = if ok {
                console::style("↘").green()
            } else {
                console::style("↘").red()
            };
            println!("{mark} sub-agent {nickname}: {summary}");
        }
        AgentEvent::SubAgentActivity {
            id,
            phase,
            tool,
            preview,
            is_error,
        } => {
            let mark = if is_error {
                console::style("·").red()
            } else {
                console::style("·").magenta()
            };
            let preview = if preview.is_empty() {
                String::new()
            } else {
                format!(" {}", console::style(&preview).dim())
            };
            println!("{mark} sub-agent {id} {phase} {tool}{preview}");
        }
        AgentEvent::GoalIntercepted { kind, detail } => {
            println!(
                "  {} gate refused {kind}: {}",
                console::style("⛔").yellow(),
                console::style(detail).dim()
            );
        }
        AgentEvent::DelegationStage { action, detail } => {
            println!(
                "  {} delegation {action} {}",
                console::style("⑂").cyan(),
                console::style(detail).dim()
            );
        }
        AgentEvent::EvidenceLedgerUpdated { ledger } => {
            println!(
                "  {} evidence ledger · mut={} intercepts={}",
                console::style("📒").blue(),
                ledger.mutations.len(),
                ledger.intercepts.len()
            );
        }
        AgentEvent::ProgressUpdated { ledger } => {
            if ledger.closing || ledger.no_progress_streak > 0 {
                println!(
                    "  {} progress · closing={} streak={}",
                    console::style("📈").blue(),
                    ledger.closing,
                    ledger.no_progress_streak,
                );
            }
        }
        AgentEvent::AdvisoryStarted { kind } => {
            // Closeout model round trips after the visible answer; name the
            // wait instead of showing a bare "waiting for model".
            let label = match kind {
                AdvisoryKind::ContextCompaction => "compacting context",
                AdvisoryKind::CloseoutNudge(reason) => match reason {
                    leveler_agent::closeout::CloseoutReason::GoalUnresolved => {
                        "nudge: goal unresolved"
                    }
                    leveler_agent::closeout::CloseoutReason::EmptyAnswer => "nudge: empty answer",
                    leveler_agent::closeout::CloseoutReason::PlanUnreconciled => {
                        "nudge: plan unreconciled"
                    }
                },
            };
            println!("  {} {label}", console::style("⋯").yellow());
        }
        AgentEvent::CommandProgress { label, elapsed_ms } => {
            // Long-command heartbeat so headless runs aren't a silent wait.
            println!(
                "  {} 运行 {label} · {}s",
                console::style("⋯").yellow(),
                elapsed_ms / 1000
            );
        }
        AgentEvent::FinalizationStarted => {
            println!("{} finalizing", console::style("⋯").yellow());
        }
        AgentEvent::FinalizationPhaseStarted { phase } => {
            println!("  {} finalizing: {phase}", console::style("⋯").yellow());
        }
        AgentEvent::FinalizationPhaseFinished { .. } => {}
        AgentEvent::MemoryRecalled { count, .. } => {
            println!("{} memory recalled {count}", console::style("●").cyan());
        }
        AgentEvent::MemoryChanged {
            operation,
            id,
            title,
            ..
        } => {
            println!(
                "{} memory {operation} [{id}] {title}",
                console::style("●").cyan()
            );
        }
        AgentEvent::Finished(_) => {}
    }
}

fn render_event_jsonl(event: AgentEvent) {
    emit_jsonl(event_jsonl(event));
}

fn event_jsonl(event: AgentEvent) -> serde_json::Value {
    match event {
        AgentEvent::StreamAttemptStarted => {
            serde_json::json!({ "type": "stream_attempt_started" })
        }
        AgentEvent::ContextUsage { accounting } => serde_json::json!({
            "type": "context_usage",
            "accounting": accounting,
        }),
        // Structured connectivity, never a pre-formatted UI string: a consumer
        // decides how to show it.
        AgentEvent::ModelRetrying {
            attempt,
            max_attempts,
            delay_ms,
        } => serde_json::json!({
            "type": "model_retrying",
            "attempt": attempt,
            "max_attempts": max_attempts,
            "delay_ms": delay_ms,
        }),
        AgentEvent::ToolOutput { id, stream, text } => serde_json::json!({
            "type": "tool_output",
            "id": id,
            "stream": match stream {
                leveler_execution::OutputStream::Stdout => "stdout",
                leveler_execution::OutputStream::Stderr => "stderr",
            },
            "text": text,
        }),
        AgentEvent::SubAgentModelRequest { record } => serde_json::json!({
            "type": "sub_agent_model_request",
            "agent_id": record.agent_id,
            "input_tokens": record.usage.input_tokens,
            "cached_input_tokens": record.usage.cached_input_tokens,
            "output_tokens": record.usage.output_tokens,
            "cost_usd_micros": record.cost_usd_micros,
        }),
        AgentEvent::AssistantDelta(delta) => {
            serde_json::json!({ "type": "assistant_delta", "delta": delta })
        }
        AgentEvent::ReasoningDelta(delta) => {
            serde_json::json!({ "type": "reasoning_delta", "delta": delta })
        }
        AgentEvent::AssistantText(text) => {
            serde_json::json!({ "type": "assistant_text", "text": text })
        }
        AgentEvent::ToolCall {
            id,
            name,
            arguments,
            ..
        } => {
            serde_json::json!({ "type": "tool_call", "id": id, "tool": name, "arguments": arguments })
        }
        AgentEvent::ToolResult {
            id,
            name,
            is_error,
            preview,
            ..
        } => serde_json::json!({
            "type": "tool_result", "id": id, "tool": name, "is_error": is_error, "preview": preview,
        }),
        AgentEvent::WorkspaceSnapshot { call_id, snapshot } => serde_json::json!({
            "type": "workspace_snapshot", "call_id": call_id, "snapshot": snapshot,
        }),
        AgentEvent::Finished(text) => serde_json::json!({ "type": "finished", "text": text }),
        AgentEvent::Usage {
            input_tokens,
            output_tokens,
            cached_input_tokens,
        } => serde_json::json!({
            "type": "usage",
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
            "cached_input_tokens": cached_input_tokens,
        }),
        AgentEvent::Compacted { from, to } => serde_json::json!({
            "type": "compacted", "from": from, "to": to,
        }),
        AgentEvent::ContextSnapshot { messages } => serde_json::json!({
            "type": "context_snapshot", "messages": messages,
        }),
        AgentEvent::PlanUpdated { steps } => serde_json::json!({
            "type": "plan_updated", "steps": steps,
        }),
        AgentEvent::SubAgentStarted {
            id,
            nickname,
            role,
            task,
            profile_id,
            profile_role,
            read_only,
            spec: _,
        } => serde_json::json!({
            "type": "sub_agent_started",
            "id": id, "nickname": nickname, "role": role, "task": task,
            "profile_id": profile_id,
            "profile_role": profile_role,
            "read_only": read_only,
        }),
        AgentEvent::SubAgentProgress {
            id,
            active,
            input_tokens,
            output_tokens,
            cached_input_tokens,
        } => serde_json::json!({
            "type": "sub_agent_progress",
            "id": id,
            "active": active,
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
            "cached_input_tokens": cached_input_tokens,
        }),
        AgentEvent::SubAgentFinished {
            id,
            nickname,
            ok,
            summary,
            contribution,
            outcome,
            stop,
            limit,
        } => serde_json::json!({
            "type": "sub_agent_finished",
            "id": id, "nickname": nickname, "ok": ok, "summary": summary,
            // Machine-readable contribution, so eval collectors read counts
            // instead of parsing prose. Null for children that never reported
            // and for logs written before contribution tracing.
            "contribution": contribution,
            // The typed terminal: null only for rows written before it was
            // recorded.
            "outcome": outcome,
            "stop": stop,
            // Which bound fired when `stop` is budget; null otherwise.
            "limit": limit,
        }),
        AgentEvent::SubAgentActivity {
            id,
            phase,
            tool,
            preview,
            is_error,
        } => serde_json::json!({
            "type": "sub_agent_activity",
            "id": id,
            "phase": phase,
            "tool": tool,
            "preview": preview,
            "is_error": is_error,
        }),
        AgentEvent::GoalIntercepted { kind, detail } => serde_json::json!({
            "type": "goal_intercepted", "kind": kind, "detail": detail,
        }),
        AgentEvent::DelegationStage { action, detail } => serde_json::json!({
            "type": "delegation_stage", "action": action, "detail": detail,
        }),
        AgentEvent::EvidenceLedgerUpdated { ledger } => serde_json::json!({
            "type": "evidence_ledger_updated",
            "mutations": ledger.mutations.len(),
            "intercepts": ledger.intercepts.len(),
        }),
        AgentEvent::ProgressUpdated { ledger } => serde_json::json!({
            "type": "progress_updated",
            "closing": ledger.closing,
            "no_progress_streak": ledger.no_progress_streak,
        }),
        AgentEvent::AdvisoryStarted { kind } => serde_json::json!({
            "type": "advisory_started", "kind": kind.as_key(),
        }),
        AgentEvent::CommandProgress { label, elapsed_ms } => serde_json::json!({
            "type": "command_progress", "label": label, "elapsed_ms": elapsed_ms,
        }),
        AgentEvent::FinalizationStarted => serde_json::json!({
            "type": "finalization_started",
        }),
        AgentEvent::FinalizationPhaseStarted { phase } => serde_json::json!({
            "type": "finalization_phase_started", "phase": phase,
        }),
        AgentEvent::FinalizationPhaseFinished { phase, elapsed_ms } => serde_json::json!({
            "type": "finalization_phase_finished", "phase": phase, "elapsed_ms": elapsed_ms,
        }),
        AgentEvent::MemoryRecalled { ids, count } => serde_json::json!({
            "type": "memory_recalled", "count": count, "ids": ids,
        }),
        AgentEvent::MemoryChanged {
            operation,
            id,
            title,
            authority,
        } => serde_json::json!({
            "type": "memory_changed", "operation": operation, "id": id,
            "title": title, "authority": authority,
        }),
    }
}

pub(crate) fn emit_jsonl(value: serde_json::Value) {
    println!("{value}");
}

#[cfg(test)]
mod jsonl_tests {
    use super::*;

    /// Eval collectors read a child's terminal from this line. The typed
    /// reading must be on it; recovering it from `summary` is prose parsing.
    #[test]
    fn a_child_terminal_line_carries_its_typed_reading() {
        let line = event_jsonl(AgentEvent::SubAgentFinished {
            id: "a1".into(),
            nickname: "Newton".into(),
            ok: false,
            summary: "stopped".into(),
            contribution: None,
            outcome: Some(leveler_lifecycle::ChildStatus::IncompleteNoResult),
            stop: Some(leveler_lifecycle::ChildStop::Lost),
            limit: None,
        });
        assert_eq!(line["outcome"], "incomplete_no_result");
        assert_eq!(line["stop"], "lost");
    }
}
