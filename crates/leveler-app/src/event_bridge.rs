use std::collections::HashMap;
use std::time::Instant;

use tokio::sync::broadcast;

use leveler_agent::{AgentError, AgentEvent, AgentOutcome, AgentVerificationStatus, StopReason};
use leveler_core::ToolCallId;
use leveler_orchestrator::NodeStatus;

use leveler_client_protocol::{
    CheckState, MessageId, NotificationLevel, PlanStepStatus, RuntimeEvent, UiCheck, UiPlan,
    UiPlanStep, UiVerification,
};

use crate::AppError;

pub(crate) fn turn_runtime_event(result: Result<AgentOutcome, AppError>) -> RuntimeEvent {
    match result {
        Ok(outcome) => {
            let detail = outcome.stop_detail.filter(|s| !s.trim().is_empty());
            match outcome.stop_reason {
                StopReason::Completed => RuntimeEvent::TurnCompleted,
                StopReason::Answered => RuntimeEvent::TurnAnswered,
                // Plan done, but the model wouldn't stop re-auditing so a guard
                // ended the turn. Presented as an ended turn (completion is the
                // verify layer's call), not as Incomplete.
                StopReason::CloseoutForced => RuntimeEvent::TurnAnswered,
                StopReason::Incomplete => RuntimeEvent::TurnIncomplete {
                    reason: detail.unwrap_or_else(|| "完整性检查未通过或无法完成".to_string()),
                },
                // The work so far is real and still on disk. A bare "budget
                // exhausted" reads as a dead end, so name the way forward:
                // /goal is the profile that grants further work-windows instead
                // of stopping at one round budget.
                StopReason::BudgetExhausted => RuntimeEvent::TurnIncomplete {
                    reason: detail.unwrap_or_else(|| "预算用尽 · 说「继续」或 /goal 接着做".into()),
                },
                StopReason::Blocked => RuntimeEvent::TurnIncomplete {
                    reason: detail.unwrap_or_else(|| "目标被标记为阻塞".to_string()),
                },
                StopReason::Stalled => RuntimeEvent::TurnIncomplete {
                    reason: detail.unwrap_or_else(|| "goal 未确认完成".into()),
                },
                StopReason::CompletedUnverified => RuntimeEvent::TurnCompletedUnverified {
                    reason: detail.unwrap_or_else(|| {
                        leveler_client_protocol::REASON_NO_AUTOMATIC_VERIFICATION.to_string()
                    }),
                },
            }
        }
        Err(AppError::Agent(AgentError::Cancelled)) => RuntimeEvent::TurnCancelled,
        Err(AppError::Agent(AgentError::Model(error)))
            if error.kind == leveler_model::ModelErrorKind::Truncated =>
        {
            RuntimeEvent::TurnTruncated {
                error: error.to_string(),
            }
        }
        Err(error) => RuntimeEvent::TurnFailed {
            error: error.to_string(),
        },
    }
}

/// Translates the runtime's synchronous `AgentEvent`s into protocol events. Tool
/// calls carry a stable id, so a `ToolResult` pairs with its `ToolCall` by id
/// (NOT arrival order — read-only tools run in parallel, so results can arrive
/// out of order or after an interleaved serial tool). `tool_starts` records each
/// call's start time by id for the client-side duration.
pub(crate) struct EventBridge {
    events: broadcast::Sender<RuntimeEvent>,
    tool_starts: HashMap<String, Instant>,
    /// The in-flight assistant message id, open while deltas stream (spec §16).
    open_assistant: Option<MessageId>,
    verification_checks: Vec<UiCheck>,
}

impl EventBridge {
    pub(crate) fn new(events: broadcast::Sender<RuntimeEvent>) -> Self {
        Self {
            events,
            tool_starts: HashMap::new(),
            open_assistant: None,
            verification_checks: Vec::new(),
        }
    }

    pub(crate) fn forward(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::StreamAttemptStarted => {
                let message_id = self.open_assistant.take();
                let _ = self
                    .events
                    .send(RuntimeEvent::AssistantAttemptReset { message_id });
            }
            AgentEvent::AssistantDelta(delta) => {
                if self.open_assistant.is_none() {
                    let id = MessageId::new(leveler_core::new_uuid_string());
                    let _ = self.events.send(RuntimeEvent::AssistantMessageStarted {
                        message_id: id.clone(),
                    });
                    self.open_assistant = Some(id);
                }
                if let Some(id) = &self.open_assistant {
                    let _ = self.events.send(RuntimeEvent::AssistantTextDelta {
                        message_id: id.clone(),
                        delta,
                    });
                }
            }
            AgentEvent::ReasoningDelta(delta) => {
                let _ = self.events.send(RuntimeEvent::ReasoningDelta { delta });
            }
            AgentEvent::AssistantText(text) => {
                // Streamed path: close the open message. Non-streamed fallback:
                // synthesize the whole message as one delta.
                if let Some(id) = self.open_assistant.take() {
                    let _ = self
                        .events
                        .send(RuntimeEvent::AssistantMessageCompleted { message_id: id });
                } else if !text.trim().is_empty() {
                    let id = MessageId::new(leveler_core::new_uuid_string());
                    let _ = self.events.send(RuntimeEvent::AssistantMessageStarted {
                        message_id: id.clone(),
                    });
                    let _ = self.events.send(RuntimeEvent::AssistantTextDelta {
                        message_id: id.clone(),
                        delta: text,
                    });
                    let _ = self
                        .events
                        .send(RuntimeEvent::AssistantMessageCompleted { message_id: id });
                }
            }
            AgentEvent::ToolCall {
                id,
                name,
                arguments,
            } => {
                // A tool call ends the current assistant thought. Close any open
                // streamed message so the next round's text opens a fresh block
                // instead of being concatenated onto this one.
                if let Some(open) = self.open_assistant.take() {
                    let _ = self
                        .events
                        .send(RuntimeEvent::AssistantMessageCompleted { message_id: open });
                }
                self.tool_starts.insert(id.clone(), Instant::now());
                let _ = self.events.send(RuntimeEvent::ToolCallStarted {
                    id: ToolCallId::new(id),
                    name,
                    arguments,
                });
            }
            AgentEvent::ToolResult {
                id,
                name,
                is_error,
                preview,
            } => {
                // Pair with the ToolCall by id, whatever order results arrive in.
                // A denial/guard result has no prior ToolCall — synthesize a
                // started block first so it still renders and isn't dropped.
                let start = match self.tool_starts.remove(&id) {
                    Some(start) => start,
                    None => {
                        let _ = self.events.send(RuntimeEvent::ToolCallStarted {
                            id: ToolCallId::new(id.clone()),
                            name,
                            arguments: String::new(),
                        });
                        Instant::now()
                    }
                };
                let _ = self.events.send(RuntimeEvent::ToolCallCompleted {
                    id: ToolCallId::new(id),
                    ok: !is_error,
                    preview,
                    duration_ms: start.elapsed().as_millis() as u64,
                });
            }
            AgentEvent::WorkspaceSnapshot { .. } => {
                // Durability metadata is persisted by the engine; it has no
                // standalone transcript cell in the TUI.
            }
            AgentEvent::Usage {
                input_tokens,
                output_tokens,
                cached_input_tokens,
            } => {
                let _ = self.events.send(RuntimeEvent::TokenUsage {
                    input_tokens,
                    output_tokens,
                    cached_input_tokens,
                });
            }
            AgentEvent::Compacted { from, to } => {
                let _ = self.events.send(RuntimeEvent::Notification {
                    level: NotificationLevel::Info,
                    message: format!("上下文已压缩 {from} → {to} 条"),
                });
            }
            AgentEvent::ContextSnapshot { .. } => {
                // Engine durability metadata; no standalone UI cell.
            }
            AgentEvent::PlanUpdated { steps } => {
                let plan = UiPlan {
                    steps: steps
                        .into_iter()
                        .enumerate()
                        .map(|(index, s)| UiPlanStep {
                            index,
                            description: s.step,
                            status: match s.status.as_str() {
                                "in_progress" => PlanStepStatus::Running,
                                "completed" => PlanStepStatus::Done,
                                _ => PlanStepStatus::Pending,
                            },
                        })
                        .collect(),
                };
                let _ = self.events.send(RuntimeEvent::PlanUpdated { plan });
            }
            AgentEvent::GoalIntercepted { kind, detail } => {
                // Surface as activity label; full tool error remains the model path.
                let _ = self.events.send(RuntimeEvent::AgentActivity {
                    label: format!("gate refused {kind}: {detail}"),
                });
            }
            AgentEvent::EvidenceLedgerUpdated { .. } => {
                // Persisted by engine; no dedicated UI cell in v1.
            }
            AgentEvent::ProgressUpdated { ledger } => {
                let phase = match ledger.phase {
                    leveler_lifecycle::TurnPhase::Active => "active",
                    leveler_lifecycle::TurnPhase::AwaitingModel => "awaiting_model",
                    leveler_lifecycle::TurnPhase::ToolBatch => "tool_batch",
                    leveler_lifecycle::TurnPhase::Closing => "closing",
                    leveler_lifecycle::TurnPhase::AwaitingUser => "awaiting_user",
                    leveler_lifecycle::TurnPhase::Terminal => "terminal",
                };
                let _ = self.events.send(RuntimeEvent::TurnProgress {
                    phase: phase.to_string(),
                    closing: ledger.closing,
                    no_progress_streak: ledger.no_progress_streak,
                    closeout_deny_rounds: ledger.closeout_deny_rounds,
                });
                if ledger.closing {
                    let _ = self.events.send(RuntimeEvent::AgentActivity {
                        label: "收口中 · 勿重复空转观察".into(),
                    });
                } else if ledger.no_progress_streak > 0 {
                    let _ = self.events.send(RuntimeEvent::AgentActivity {
                        label: format!("无进展 streak {}", ledger.no_progress_streak),
                    });
                }
            }
            AgentEvent::VerificationStarted => {
                self.verification_checks.clear();
                self.emit_verification(None);
            }
            AgentEvent::VerificationCheck {
                name,
                status,
                evidence,
            } => {
                self.verification_checks.push(UiCheck {
                    name,
                    status: map_agent_check_status(status),
                    evidence,
                });
                self.emit_verification(None);
            }
            AgentEvent::VerificationFinished { passed } => self.emit_verification(Some(passed)),
            AgentEvent::SubAgentStarted {
                id,
                nickname,
                role,
                task,
            } => {
                let _ = self.events.send(RuntimeEvent::SubAgentUpdated {
                    id,
                    nickname,
                    role,
                    done: false,
                    ok: false,
                    detail: task,
                });
            }
            AgentEvent::SubAgentProgress {
                id,
                active,
                input_tokens,
                output_tokens,
                cached_input_tokens,
            } => {
                let _ = self.events.send(RuntimeEvent::SubAgentProgress {
                    id,
                    active,
                    input_tokens,
                    output_tokens,
                    cached_input_tokens,
                });
            }
            AgentEvent::SubAgentFinished {
                id,
                nickname,
                ok,
                summary,
            } => {
                let _ = self.events.send(RuntimeEvent::SubAgentUpdated {
                    id,
                    nickname,
                    role: String::new(),
                    done: true,
                    ok,
                    detail: summary,
                });
            }
            AgentEvent::Finished(_) => {
                // Close a still-open streamed message at turn end. Without this, a
                // round that streamed only whitespace (no closing AssistantText,
                // which the executor sends only for non-empty text) would leave
                // the message "streaming" forever and misdirect the next round's
                // deltas to a stale id.
                if let Some(id) = self.open_assistant.take() {
                    let _ = self
                        .events
                        .send(RuntimeEvent::AssistantMessageCompleted { message_id: id });
                }
            }
        }
    }

    fn emit_verification(&self, passed: Option<bool>) {
        let _ = self.events.send(RuntimeEvent::VerificationUpdated {
            verification: UiVerification {
                checks: self.verification_checks.clone(),
                passed,
            },
        });
    }
}

/// Translates orchestrator events into plan/verification protocol events,
/// accumulating plan and check state so each update is a full snapshot the UI
/// can render directly. Inner `AgentEvent`s are forwarded via [`EventBridge`].
pub(crate) struct OrchestratorBridge {
    inner: EventBridge,
    /// Task-node ids in plan order, to map `NodeStarted/Finished` to a step.
    node_ids: Vec<String>,
    plan: UiPlan,
    checks: Vec<UiCheck>,
}

impl OrchestratorBridge {
    pub(crate) fn new(events: broadcast::Sender<RuntimeEvent>) -> Self {
        Self {
            inner: EventBridge::new(events),
            node_ids: Vec::new(),
            plan: UiPlan { steps: Vec::new() },
            checks: Vec::new(),
        }
    }

    fn events(&self) -> &broadcast::Sender<RuntimeEvent> {
        &self.inner.events
    }

    pub(crate) fn forward(&mut self, event: leveler_engine::EngineEvent) {
        use leveler_engine::EngineEvent as E;
        match event {
            E::PhaseChanged { to, .. } => {
                let _ = self.events().send(RuntimeEvent::AgentActivity {
                    label: format!("阶段：{}", to.as_str()),
                });
            }
            E::PlanReady { graph } => {
                self.node_ids = graph.nodes.iter().map(|n| n.id.to_string()).collect();
                self.plan = UiPlan {
                    steps: graph
                        .nodes
                        .iter()
                        .enumerate()
                        .map(|(i, n)| UiPlanStep {
                            index: i,
                            description: n.description.clone(),
                            status: map_node_status(n.status),
                        })
                        .collect(),
                };
                let _ = self.events().send(RuntimeEvent::PlanUpdated {
                    plan: self.plan.clone(),
                });
            }
            E::NodeStarted { node_id, .. } => self.set_step(&node_id, PlanStepStatus::Running),
            E::NodeFinished { node_id, status } => self.set_step(&node_id, map_node_status(status)),
            E::VerificationStarted => {
                self.checks.clear();
                self.emit_verification(None);
            }
            E::VerificationCheck {
                name,
                status,
                evidence,
            } => {
                self.checks.push(UiCheck {
                    name,
                    status: match status.as_str() {
                        "passed" => CheckState::Passed,
                        "failed" => CheckState::Failed,
                        _ => CheckState::Skipped,
                    },
                    evidence,
                });
                self.emit_verification(None);
            }
            E::VerificationFinished { passed } => self.emit_verification(Some(passed)),
            E::ContextReady {
                candidate_files,
                estimated_tokens,
            } => {
                let _ = self.events().send(RuntimeEvent::ContextUpdated {
                    candidate_files,
                    estimated_tokens,
                });
            }
            // Kernel events reuse the legacy AgentEvent bridge.
            other => {
                if let Some(agent_event) = crate::session::engine_event_to_agent(other) {
                    self.inner.forward(agent_event);
                }
            }
        }
    }

    fn set_step(&mut self, id: &str, status: PlanStepStatus) {
        if let Some(i) = self.node_ids.iter().position(|x| x == id) {
            if let Some(step) = self.plan.steps.get_mut(i) {
                step.status = status;
            }
            let _ = self.events().send(RuntimeEvent::PlanUpdated {
                plan: self.plan.clone(),
            });
        }
    }

    fn emit_verification(&self, passed: Option<bool>) {
        let _ = self.events().send(RuntimeEvent::VerificationUpdated {
            verification: UiVerification {
                checks: self.checks.clone(),
                passed,
            },
        });
    }
}

fn map_node_status(status: NodeStatus) -> PlanStepStatus {
    match status {
        NodeStatus::Pending => PlanStepStatus::Pending,
        NodeStatus::Running => PlanStepStatus::Running,
        NodeStatus::Completed => PlanStepStatus::Done,
        NodeStatus::Failed => PlanStepStatus::Failed,
        NodeStatus::Skipped => PlanStepStatus::Skipped,
    }
}

fn map_agent_check_status(status: AgentVerificationStatus) -> CheckState {
    match status {
        AgentVerificationStatus::Passed => CheckState::Passed,
        AgentVerificationStatus::Failed => CheckState::Failed,
        AgentVerificationStatus::Skipped => CheckState::Skipped,
    }
}

#[cfg(test)]
mod bridge_tests {
    use super::*;

    fn drain(rx: &mut broadcast::Receiver<RuntimeEvent>) -> Vec<RuntimeEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    }

    /// The direct path's structured plan must reach the client as PlanUpdated,
    /// with update_plan wire statuses mapped onto the UI step states.
    #[test]
    fn plan_updated_maps_statuses_onto_ui_plan() {
        let (tx, mut rx) = broadcast::channel(16);
        let mut bridge = EventBridge::new(tx);

        bridge.forward(AgentEvent::PlanUpdated {
            steps: vec![
                leveler_agent::PlanStep {
                    step: "locate the bug".into(),
                    status: "completed".into(),
                    id: None,
                    origin: leveler_agent::PlanOrigin::ModelExplicit,
                },
                leveler_agent::PlanStep {
                    step: "fix it".into(),
                    status: "in_progress".into(),
                    id: None,
                    origin: leveler_agent::PlanOrigin::ModelExplicit,
                },
                leveler_agent::PlanStep {
                    step: "run tests".into(),
                    status: "pending".into(),
                    id: None,
                    origin: leveler_agent::PlanOrigin::ModelExplicit,
                },
            ],
        });

        let events = drain(&mut rx);
        let plan = events
            .iter()
            .find_map(|e| match e {
                RuntimeEvent::PlanUpdated { plan } => Some(plan.clone()),
                _ => None,
            })
            .expect("PlanUpdated must be forwarded");
        assert_eq!(plan.steps.len(), 3);
        assert_eq!(plan.steps[0].status, PlanStepStatus::Done);
        assert_eq!(plan.steps[1].status, PlanStepStatus::Running);
        assert_eq!(plan.steps[1].description, "fix it");
        assert_eq!(plan.steps[2].status, PlanStepStatus::Pending);
        assert_eq!(plan.steps[2].index, 2);
    }

    /// The id of the Completed event that carries `preview`.
    fn completed_id_for_preview(events: &[RuntimeEvent], preview: &str) -> String {
        events
            .iter()
            .find_map(|e| match e {
                RuntimeEvent::ToolCallCompleted { id, preview: p, .. } if p == preview => {
                    Some(id.as_str().to_string())
                }
                _ => None,
            })
            .unwrap_or_default()
    }

    /// The id of the Started event for tool `name`.
    fn started_id_for_name(events: &[RuntimeEvent], name: &str) -> String {
        events
            .iter()
            .find_map(|e| match e {
                RuntimeEvent::ToolCallStarted { id, name: n, .. } if n == name => {
                    Some(id.as_str().to_string())
                }
                _ => None,
            })
            .unwrap_or_default()
    }

    #[test]
    fn tool_result_pairs_by_id_even_when_out_of_order() {
        // Mimics a round mixing a parallel read (grep) with a serial edit
        // (apply_patch): the edit's result is emitted before the parallel read's.
        // A FIFO pairing would swap the two previews; id pairing keeps them right.
        let (tx, mut rx) = broadcast::channel(64);
        let mut bridge = EventBridge::new(tx);
        bridge.forward(AgentEvent::ToolCall {
            id: "g".into(),
            name: "grep".into(),
            arguments: String::new(),
        });
        bridge.forward(AgentEvent::ToolCall {
            id: "p".into(),
            name: "apply_patch".into(),
            arguments: String::new(),
        });
        bridge.forward(AgentEvent::ToolResult {
            id: "p".into(),
            name: "apply_patch".into(),
            is_error: false,
            preview: "AP".into(),
        });
        bridge.forward(AgentEvent::ToolResult {
            id: "g".into(),
            name: "grep".into(),
            is_error: false,
            preview: "GR".into(),
        });

        let events = drain(&mut rx);
        // apply_patch's block must complete with apply_patch's preview, grep's with grep's.
        assert_eq!(
            started_id_for_name(&events, "apply_patch"),
            completed_id_for_preview(&events, "AP"),
            "apply_patch result paired to the wrong tool block"
        );
        assert_eq!(
            started_id_for_name(&events, "grep"),
            completed_id_for_preview(&events, "GR"),
            "grep result paired to the wrong tool block"
        );
    }

    #[test]
    fn finished_closes_a_dangling_open_assistant() {
        // A round that opened a streamed message but never sent a closing
        // AssistantText (e.g. whitespace-only output) must still be completed.
        let (tx, mut rx) = broadcast::channel(16);
        let mut bridge = EventBridge::new(tx);
        bridge.forward(AgentEvent::AssistantDelta(" ".into()));
        bridge.forward(AgentEvent::Finished(String::new()));
        let events = drain(&mut rx);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, RuntimeEvent::AssistantMessageCompleted { .. })),
            "Finished must close the open streamed message"
        );
    }

    #[test]
    fn retry_attempt_resets_the_open_transient_message() {
        let (tx, mut rx) = broadcast::channel(16);
        let mut bridge = EventBridge::new(tx);
        bridge.forward(AgentEvent::StreamAttemptStarted);
        bridge.forward(AgentEvent::AssistantDelta("wrong".into()));
        bridge.forward(AgentEvent::StreamAttemptStarted);
        bridge.forward(AgentEvent::AssistantDelta("right".into()));
        let events = drain(&mut rx);

        assert!(matches!(
            &events[0],
            RuntimeEvent::AssistantAttemptReset { message_id: None }
        ));
        let stale_id = match &events[1] {
            RuntimeEvent::AssistantMessageStarted { message_id } => message_id.clone(),
            other => panic!("expected message start, got {other:?}"),
        };
        assert!(events.iter().any(|event| matches!(
            event,
            RuntimeEvent::AssistantAttemptReset { message_id: Some(id) } if id == &stale_id
        )));
    }

    #[test]
    fn a_tool_call_closes_the_open_assistant_so_later_text_is_a_new_block() {
        // A round streams text, then calls a tool; the next round streams more
        // text. Without closing the assistant message at the tool call, the
        // second round's deltas reuse the first message id and the reducer
        // concatenates them into one block ("…presets fileThe test expects…").
        let (tx, mut rx) = broadcast::channel(64);
        let mut bridge = EventBridge::new(tx);
        bridge.forward(AgentEvent::AssistantDelta("round one text".into()));
        bridge.forward(AgentEvent::ToolCall {
            id: "c1".into(),
            name: "read_file".into(),
            arguments: String::new(),
        });
        bridge.forward(AgentEvent::AssistantDelta("round two text".into()));
        let events = drain(&mut rx);

        let started: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                RuntimeEvent::AssistantMessageStarted { message_id } => {
                    Some(message_id.as_str().to_string())
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            started.len(),
            2,
            "the tool call must close the first message so the second text opens a new one"
        );
        assert_ne!(
            started[0], started[1],
            "the two rounds' texts must have distinct message ids"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, RuntimeEvent::AssistantMessageCompleted { .. })),
            "the first assistant message must be completed at the tool call"
        );
    }

    #[test]
    fn denial_result_without_a_toolcall_still_renders() {
        // A guard/denial emits a ToolResult with no prior ToolCall; the bridge
        // must synthesize a Started so the block isn't dropped.
        let (tx, mut rx) = broadcast::channel(16);
        let mut bridge = EventBridge::new(tx);
        bridge.forward(AgentEvent::ToolResult {
            id: "x".into(),
            name: "grep".into(),
            is_error: true,
            preview: "search budget reached".into(),
        });
        let events = drain(&mut rx);
        let started = events
            .iter()
            .any(|e| matches!(e, RuntimeEvent::ToolCallStarted { name, .. } if name == "grep"));
        let completed = events.iter().any(|e| {
            matches!(e, RuntimeEvent::ToolCallCompleted { ok, preview, .. }
                if !ok && preview == "search budget reached")
        });
        assert!(started, "a synthesized Started must be emitted");
        assert!(completed, "the denial result must complete the block");
    }

    fn outcome(stop_reason: StopReason) -> AgentOutcome {
        AgentOutcome {
            final_text: String::new(),
            rounds: 1,
            modified_files: Vec::new(),
            stop_reason,
            stop_detail: None,
            metrics: Default::default(),
            progress: Default::default(),
            objective: leveler_lifecycle::ObjectiveAnchor::from_user_message(""),
        }
    }

    #[test]
    fn answer_end_does_not_emit_task_completed() {
        assert_eq!(
            turn_runtime_event(Ok(outcome(StopReason::Answered))),
            RuntimeEvent::TurnAnswered
        );
        assert_eq!(
            turn_runtime_event(Ok(outcome(StopReason::Completed))),
            RuntimeEvent::TurnCompleted
        );
    }

    #[test]
    fn output_limit_error_has_a_distinct_runtime_event() {
        let error =
            leveler_model::ModelError::new(leveler_model::ModelErrorKind::Truncated, "token limit");
        assert!(matches!(
            turn_runtime_event(Err(AppError::Agent(AgentError::Model(error)))),
            RuntimeEvent::TurnTruncated { .. }
        ));
    }
    #[test]
    fn a_budget_cutoff_tells_the_user_how_to_carry_on() {
        // Short product copy: next action (continue / /goal), not a long essay.
        let RuntimeEvent::TurnIncomplete { reason } =
            turn_runtime_event(Ok(outcome(StopReason::BudgetExhausted)))
        else {
            panic!("a budget cutoff is an incomplete turn");
        };
        assert!(
            reason.contains("/goal") && reason.contains("继续"),
            "point at how to resume: {reason}"
        );
    }
}
