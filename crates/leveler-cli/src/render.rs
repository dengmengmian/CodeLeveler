//! Event rendering for the CLI: agent events in text or JSONL form.

use leveler_agent::{AdvisoryKind, AgentEvent, AgentVerificationStatus};

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
        AgentEvent::AssistantDelta(_) => {}
        AgentEvent::ReasoningDelta(_) => {}
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
        AgentEvent::VerificationStarted => {
            println!("{} verification started", console::style("→").blue());
        }
        AgentEvent::VerificationCheck {
            name,
            status,
            evidence,
        } => {
            let mark = match status {
                AgentVerificationStatus::Passed => console::style("✓").green(),
                AgentVerificationStatus::Failed => console::style("✗").red(),
                AgentVerificationStatus::Skipped => console::style("–").dim(),
            };
            println!("  {mark} verification {name}");
            if let Some(evidence) = evidence {
                let trimmed = evidence.trim();
                if !trimmed.is_empty() {
                    println!("    {}", console::style(trimmed).dim());
                }
            }
        }
        AgentEvent::VerificationFinished { passed } => {
            let label = if passed { "passed" } else { "failed" };
            println!("{} verification {label}", console::style("•").cyan());
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
        AgentEvent::EvidenceLedgerUpdated { ledger } => {
            println!(
                "  {} evidence ledger · mut={} verify={} intercepts={}",
                console::style("📒").blue(),
                ledger.mutations.len(),
                ledger.verifications.len(),
                ledger.intercepts.len()
            );
        }
        AgentEvent::ProgressUpdated { ledger } => {
            if ledger.closing || ledger.no_progress_streak > 0 {
                println!(
                    "  {} progress · closing={} streak={} closeout_denies={}",
                    console::style("📈").blue(),
                    ledger.closing,
                    ledger.no_progress_streak,
                    ledger.closeout_deny_rounds
                );
            }
        }
        AgentEvent::AdvisoryStarted { kind } => {
            // Closeout model round trips after the visible answer; name the
            // wait instead of showing a bare "waiting for model".
            let label = match kind {
                AdvisoryKind::ContextCompaction => "compacting context",
                AdvisoryKind::GoalContinuation => "continuing active goal",
                AdvisoryKind::CloseoutNudge(reason) => match reason {
                    leveler_agent::closeout::CloseoutReason::GoalUnresolved => {
                        "nudge: goal unresolved"
                    }
                    leveler_agent::closeout::CloseoutReason::EmptyAnswer => "nudge: empty answer",
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
        AgentEvent::Finished(_) => {}
    }
}

fn render_event_jsonl(event: AgentEvent) {
    let value = match event {
        AgentEvent::StreamAttemptStarted => {
            serde_json::json!({ "type": "stream_attempt_started" })
        }
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
        AgentEvent::VerificationStarted => serde_json::json!({
            "type": "verification_started",
        }),
        AgentEvent::VerificationCheck {
            name,
            status,
            evidence,
        } => serde_json::json!({
            "type": "verification_check",
            "name": name,
            "status": match status {
                AgentVerificationStatus::Passed => "passed",
                AgentVerificationStatus::Failed => "failed",
                AgentVerificationStatus::Skipped => "skipped",
            },
            "evidence": evidence,
        }),
        AgentEvent::VerificationFinished { passed } => serde_json::json!({
            "type": "verification_finished", "passed": passed,
        }),
        AgentEvent::SubAgentStarted {
            id,
            nickname,
            role,
            task,
        } => serde_json::json!({
            "type": "sub_agent_started",
            "id": id, "nickname": nickname, "role": role, "task": task,
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
        } => serde_json::json!({
            "type": "sub_agent_finished",
            "id": id, "nickname": nickname, "ok": ok, "summary": summary,
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
        AgentEvent::EvidenceLedgerUpdated { ledger } => serde_json::json!({
            "type": "evidence_ledger_updated",
            "mutations": ledger.mutations.len(),
            "verifications": ledger.verifications.len(),
            "intercepts": ledger.intercepts.len(),
            "step_receipts": ledger.step_receipts.len(),
        }),
        AgentEvent::ProgressUpdated { ledger } => serde_json::json!({
            "type": "progress_updated",
            "closing": ledger.closing,
            "no_progress_streak": ledger.no_progress_streak,
            "closeout_deny_rounds": ledger.closeout_deny_rounds,
        }),
        AgentEvent::AdvisoryStarted { kind } => serde_json::json!({
            "type": "advisory_started", "kind": kind.as_key(),
        }),
        AgentEvent::CommandProgress { label, elapsed_ms } => serde_json::json!({
            "type": "command_progress", "label": label, "elapsed_ms": elapsed_ms,
        }),
    };
    emit_jsonl(value);
}

pub(crate) fn emit_jsonl(value: serde_json::Value) {
    println!("{value}");
}
